//! llmdil-log-receiver — the ingest + store half of LLMdil log
//! streaming, Prometheus-local (DESIGN-prometheus-local.md).
//!
//! Runs as its own auto-start Windows service on the machine
//! LLMdil runs on (registered through `manager-services`;
//! `llm-service-host` spawns this binary with no arguments). One
//! process, two listeners with disjoint route sets:
//!
//! - Ingest: `127.0.0.1:8473` — `POST /v1/batches` (Bearer ingest
//!   token), `GET /healthz`. No query route here, by design.
//! - Read: `<tailnet IPv4>:8474` — `GET /healthz`, `GET
//!   /v1/status`, `GET /v1/query`, all behind a separate read-only
//!   Bearer token. The bind address is discovered from the
//!   machine's 100.64.0.0/10 (tailnet) addresses and retried with
//!   backoff, because SCM can start this service before Tailscale
//!   has assigned the address.
//!
//! Configuration (environment; the service definition supplies
//! these — see `manager_services::log_receiver_definition`):
//!   LLMDIL_LOG_RECEIVER_BIND   ingest address (default 127.0.0.1:8473)
//!   LLMDIL_LOG_RECEIVER_DB     SQLite path (default ./llmdil-logs.db)
//!   LLMDIL_LOG_INGEST_TOKENS   comma-separated per-machine tokens
//!   LLMDIL_LOG_INGEST_TOKEN_FILE  file with one token per line
//!   LLMDIL_LOG_READ_TOKEN / LLMDIL_LOG_READ_TOKEN_FILE
//!                              read token (env value or file); when
//!                              neither is set the read listener is off
//!   LLMDIL_LOG_READ_BIND       explicit read address (ip:port);
//!                              overrides tailnet discovery
//!   LLMDIL_LOG_READ_PORT       read port for discovery (default 8474)
//!   LLMDIL_LOG_RETENTION_DAYS  default 14 (D8)
//!   LLMDIL_LOG_RETENTION_CAP_MIB  default 512 (§3.3)
//!
//! Subcommands (deploy-time, run by the deploy script):
//!   install-service --data-dir <dir> [--install-dir <dir>]
//!   uninstall-service
//!   definition --data-dir <dir> [--install-dir <dir>]

use std::collections::HashSet;
use std::io::Read as _;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Query as AxumQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use manager_logreceiver::{Query, Record, Store};
use serde::Deserialize;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(300);
const PRUNE_INTERVAL: Duration = Duration::from_secs(3600);
const READ_BIND_BACKOFF_MIN: Duration = Duration::from_secs(5);
const READ_BIND_BACKOFF_MAX: Duration = Duration::from_secs(60);

struct AppState {
    store: Mutex<Store>,
    ingest_tokens: HashSet<String>,
    read_tokens: HashSet<String>,
    db_path: PathBuf,
    retention_days: i64,
    retention_cap_bytes: u64,
    started: Instant,
}

fn load_token_set(list_var: &str, file_var: &str) -> HashSet<String> {
    if let Ok(list) = std::env::var(list_var) {
        return list
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
    }
    if let Ok(path) = std::env::var(file_var) {
        if let Ok(text) = std::fs::read_to_string(path) {
            return text
                .lines()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
        }
    }
    HashSet::new()
}

fn bearer_ok(tokens: &HashSet<String>, headers: &HeaderMap) -> bool {
    let Some(value) = headers.get("authorization").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    tokens.contains(token.trim())
}

fn db_bytes(path: &PathBuf) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

async fn post_batches(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if !bearer_ok(&state.ingest_tokens, &headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let raw: Vec<u8> = if headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("gzip"))
        .unwrap_or(false)
    {
        let mut dec = flate2::read::GzDecoder::new(&body[..]);
        let mut out = Vec::new();
        if dec.read_to_end(&mut out).is_err() {
            return (StatusCode::BAD_REQUEST, "invalid gzip body").into_response();
        }
        out
    } else {
        body.to_vec()
    };
    let mut records: Vec<Record> = match serde_json::from_slice(&raw) {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("invalid batch: {e}")).into_response();
        }
    };
    if records.iter().any(|r| r.v != 1) {
        return (StatusCode::BAD_REQUEST, "unsupported wire version").into_response();
    }
    let now = chrono::Utc::now().to_rfc3339();
    for r in &mut records {
        r.received_at = now.clone();
    }
    let state2 = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut store = state2.store.lock().expect("store lock");
        store.insert_batch(&records)
    })
    .await;
    match result {
        Ok(Ok((accepted, duplicates))) => axum::Json(serde_json::json!({
            "accepted": accepted,
            "duplicates": duplicates,
        }))
        .into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct QueryParams {
    source: Option<String>,
    level: Option<String>,
    grep: Option<String>,
    since: Option<String>,
    limit: Option<usize>,
}

async fn get_query(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumQuery(params): AxumQuery<QueryParams>,
) -> impl IntoResponse {
    if !bearer_ok(&state.read_tokens, &headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let q = Query {
        sources: params
            .source
            .map(|s| {
                s.split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        level: params.level,
        grep: params.grep,
        since: params.since,
        limit: params.limit.unwrap_or(200).min(5000),
    };
    let state2 = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let store = state2.store.lock().expect("store lock");
        store.query(&q)
    })
    .await;
    match result {
        Ok(Ok(records)) => axum::Json(serde_json::json!({ "records": records })).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")).into_response(),
    }
}

/// Store snapshot shared by the read listener's status routes.
struct Snapshot {
    records: i64,
    machines: Vec<(String, i64, i64, String)>,
    last_heartbeat_utc: Option<String>,
}

async fn snapshot(state: &Arc<AppState>) -> Snapshot {
    let state2 = state.clone();
    let computed = tokio::task::spawn_blocking(move || {
        let store = state2.store.lock().expect("store lock");
        let records = store.count().unwrap_or(-1);
        let machines = store.machine_status().unwrap_or_default();
        let last_heartbeat_utc = store.last_heartbeat().ok().flatten().map(|hb| hb.ts_utc);
        (records, machines, last_heartbeat_utc)
    })
    .await;
    let (records, machines, last_heartbeat_utc) = computed.unwrap_or((-1, Vec::new(), None));
    Snapshot {
        records,
        machines,
        last_heartbeat_utc,
    }
}

async fn healthz_ingest(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let snap = snapshot(&state).await;
    axum::Json(serde_json::json!({
        "ok": snap.records >= 0,
        "records": snap.records,
        "db_bytes": db_bytes(&state.db_path),
    }))
    .into_response()
}

async fn healthz_read(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if !bearer_ok(&state.read_tokens, &headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let snap = snapshot(&state).await;
    axum::Json(serde_json::json!({
        "ok": snap.records >= 0,
        "records": snap.records,
        "db_bytes": db_bytes(&state.db_path),
        "last_heartbeat_utc": snap.last_heartbeat_utc,
    }))
    .into_response()
}

/// `GET /v1/status` (design §4): the debugging entry point — one
/// call answers "is anything flowing, from which machine, until
/// when", and separates receiver health from shipper quiet.
async fn get_status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if !bearer_ok(&state.read_tokens, &headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let snap = snapshot(&state).await;
    let machines: Vec<serde_json::Value> = snap
        .machines
        .iter()
        .map(|(machine, count, max_seq, last_received_at)| {
            serde_json::json!({
                "machine": machine,
                "count": count,
                "max_seq": max_seq,
                "last_received_at": last_received_at,
            })
        })
        .collect();
    axum::Json(serde_json::json!({
        "receiver": {
            "uptime_s": state.started.elapsed().as_secs(),
            "records": snap.records,
            "db_bytes": db_bytes(&state.db_path),
            "last_heartbeat_utc": snap.last_heartbeat_utc,
        },
        "machines": machines,
    }))
    .into_response()
}

async fn heartbeat_loop(state: Arc<AppState>) {
    loop {
        tokio::time::sleep(HEARTBEAT_INTERVAL).await;
        write_heartbeat(state.clone()).await;
    }
}

async fn write_heartbeat(state: Arc<AppState>) {
    let uptime_s = state.started.elapsed().as_secs();
    let bytes = db_bytes(&state.db_path);
    let state2 = state.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let store = state2.store.lock().expect("store lock");
        if let Err(e) = store.record_heartbeat(uptime_s, bytes) {
            eprintln!("llmdil-log-receiver: heartbeat write failed: {e}");
        }
    })
    .await;
}

async fn prune_loop(state: Arc<AppState>) {
    loop {
        let state2 = state.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let mut store = state2.store.lock().expect("store lock");
            match store.prune(
                &state2.db_path,
                state2.retention_days,
                state2.retention_cap_bytes,
            ) {
                Ok(detail) => {
                    if detail != "nothing to prune" {
                        eprintln!("prune: {detail}");
                    }
                }
                Err(e) => eprintln!("prune failed: {e}"),
            }
        })
        .await;
        tokio::time::sleep(PRUNE_INTERVAL).await;
    }
}

/// Pick the tailnet IPv4 (CGNAT 100.64.0.0/10) out of the machine's
/// addresses — the read listener binds exactly this address, so it
/// exists on the tailnet interface only (design §3.4).
fn pick_tailnet_ipv4(addrs: &[Ipv4Addr]) -> Option<Ipv4Addr> {
    addrs.iter().copied().find(|a| {
        let o = a.octets();
        o[0] == 100 && (64..=127).contains(&o[1])
    })
}

/// The machine's local IPv4 addresses (Windows: adapter
/// enumeration; elsewhere empty — set LLMDIL_LOG_READ_BIND for
/// development off Windows).
#[cfg(windows)]
fn local_ipv4_addresses() -> Vec<Ipv4Addr> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
        GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

    const ERROR_BUFFER_OVERFLOW: u32 = 111;
    const NO_ERROR: u32 = 0;
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let family = AF_INET as u32;
    let mut size: u32 = 0;
    let probe = unsafe {
        GetAdaptersAddresses(
            family,
            flags,
            std::ptr::null(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if probe != ERROR_BUFFER_OVERFLOW || size == 0 {
        return Vec::new();
    }
    let mut buffer = vec![0u8; size as usize];
    let rc = unsafe {
        GetAdaptersAddresses(
            family,
            flags,
            std::ptr::null(),
            buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
            &mut size,
        )
    };
    if rc != NO_ERROR {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut adapter = buffer.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    while !adapter.is_null() {
        let mut unicast = unsafe { (*adapter).FirstUnicastAddress };
        while !unicast.is_null() {
            let sockaddr = unsafe { (*unicast).Address.lpSockaddr };
            if !sockaddr.is_null() && unsafe { *(sockaddr as *const u16) } == AF_INET {
                let sin = sockaddr as *const SOCKADDR_IN;
                let raw = unsafe { (*sin).sin_addr.S_un.S_addr };
                out.push(Ipv4Addr::from(raw.to_ne_bytes()));
            }
            unicast = unsafe { (*unicast).Next };
        }
        adapter = unsafe { (*adapter).Next };
    }
    out
}

#[cfg(not(windows))]
fn local_ipv4_addresses() -> Vec<Ipv4Addr> {
    Vec::new()
}

struct ReadConfig {
    /// Explicit `LLMDIL_LOG_READ_BIND` (ip:port), when set.
    bind: Option<SocketAddr>,
    /// Port used with tailnet discovery.
    port: u16,
}

fn resolve_read_addr(cfg: &ReadConfig) -> Option<SocketAddr> {
    if let Some(addr) = cfg.bind {
        return Some(addr);
    }
    pick_tailnet_ipv4(&local_ipv4_addresses()).map(|ip| SocketAddr::new(ip.into(), cfg.port))
}

fn read_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz_read))
        .route("/v1/status", get(get_status))
        .route("/v1/query", get(get_query))
        .with_state(state)
}

/// Serve the read listener, retrying the bind with backoff until
/// the tailnet address exists (SCM may start this service before
/// the Tailscale service has joined). A missing read listener
/// never blocks ingest.
async fn read_listener_loop(state: Arc<AppState>, cfg: ReadConfig) {
    let mut backoff = READ_BIND_BACKOFF_MIN;
    let mut announced: Option<String> = None;
    loop {
        match resolve_read_addr(&cfg) {
            Some(addr) => match tokio::net::TcpListener::bind(addr).await {
                Ok(listener) => {
                    eprintln!("llmdil-log-receiver read listener on {addr}");
                    backoff = READ_BIND_BACKOFF_MIN;
                    announced = None;
                    if let Err(e) = axum::serve(listener, read_router(state.clone())).await {
                        eprintln!("llmdil-log-receiver: read serve failed: {e}");
                    }
                }
                Err(e) => {
                    let msg = format!("read bind {addr} failed: {e}");
                    if announced.as_deref() != Some(msg.as_str()) {
                        eprintln!("llmdil-log-receiver: {msg}");
                        announced = Some(msg);
                    }
                }
            },
            None => {
                let msg = "read listener waiting: no tailnet IPv4 (100.64.0.0/10) assigned yet";
                if announced.as_deref() != Some(msg) {
                    eprintln!("llmdil-log-receiver: {msg}");
                    announced = Some(msg.to_string());
                }
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(READ_BIND_BACKOFF_MAX);
    }
}

async fn serve() -> u8 {
    let bind =
        std::env::var("LLMDIL_LOG_RECEIVER_BIND").unwrap_or_else(|_| "127.0.0.1:8473".to_string());
    let db_path = PathBuf::from(
        std::env::var("LLMDIL_LOG_RECEIVER_DB").unwrap_or_else(|_| "llmdil-logs.db".to_string()),
    );
    let ingest_tokens = load_token_set("LLMDIL_LOG_INGEST_TOKENS", "LLMDIL_LOG_INGEST_TOKEN_FILE");
    if ingest_tokens.is_empty() {
        eprintln!(
            "llmdil-log-receiver: no ingest tokens configured \
             (LLMDIL_LOG_INGEST_TOKENS or LLMDIL_LOG_INGEST_TOKEN_FILE); refusing to start"
        );
        return 2;
    }
    let read_tokens = load_token_set("LLMDIL_LOG_READ_TOKEN", "LLMDIL_LOG_READ_TOKEN_FILE");
    let retention_days: i64 = std::env::var("LLMDIL_LOG_RETENTION_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(14);
    let retention_cap_bytes: u64 = std::env::var("LLMDIL_LOG_RETENTION_CAP_MIB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|mib| mib * 1024 * 1024)
        .unwrap_or(512 * 1024 * 1024);
    let store = match Store::open(&db_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "llmdil-log-receiver: cannot open store {}: {e}",
                db_path.display()
            );
            return 2;
        }
    };
    let state = Arc::new(AppState {
        store: Mutex::new(store),
        ingest_tokens,
        read_tokens,
        db_path,
        retention_days,
        retention_cap_bytes,
        started: Instant::now(),
    });
    // One heartbeat at startup, then every interval — deploy
    // verification reads heartbeat age within minutes of install.
    write_heartbeat(state.clone()).await;
    tokio::spawn(heartbeat_loop(state.clone()));
    tokio::spawn(prune_loop(state.clone()));
    if state.read_tokens.is_empty() {
        eprintln!(
            "llmdil-log-receiver: no read token configured \
             (LLMDIL_LOG_READ_TOKEN or LLMDIL_LOG_READ_TOKEN_FILE); read listener disabled"
        );
    } else {
        let read_cfg = ReadConfig {
            bind: std::env::var("LLMDIL_LOG_READ_BIND")
                .ok()
                .and_then(|v| v.parse::<SocketAddr>().ok()),
            port: std::env::var("LLMDIL_LOG_READ_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8474),
        };
        tokio::spawn(read_listener_loop(state.clone(), read_cfg));
    }
    let app = Router::new()
        .route("/v1/batches", post(post_batches))
        .route("/healthz", get(healthz_ingest))
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024))
        .with_state(state);
    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("llmdil-log-receiver: cannot bind ingest {bind}: {e}");
            return 2;
        }
    };
    eprintln!("llmdil-log-receiver ingest listening on {bind}");
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("llmdil-log-receiver: ingest serve failed: {e}");
        return 1;
    }
    0
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}

fn install_dirs(args: &[String]) -> Option<(PathBuf, PathBuf, PathBuf)> {
    let data_dir = PathBuf::from(arg_value(args, "--data-dir")?);
    let exe = std::env::current_exe().ok()?;
    let install_dir = arg_value(args, "--install-dir")
        .map(PathBuf::from)
        .or_else(|| exe.parent().map(|p| p.to_path_buf()))?;
    let receiver_exe = arg_value(args, "--receiver-exe")
        .map(PathBuf::from)
        .unwrap_or_else(|| install_dir.join("llmdil-log-receiver.exe"));
    Some((data_dir, install_dir, receiver_exe))
}

/// `install-service`: write the receiver's service definition and
/// register it with the SCM through the product's own
/// `manager-services` path (the same `register` the service host's
/// elevated helper calls). Runs as LocalService with no password,
/// so no credential handoff is involved. Must run elevated.
fn cmd_install_service(args: &[String]) -> u8 {
    let Some((data_dir, install_dir, receiver_exe)) = install_dirs(args) else {
        eprintln!("install-service: --data-dir <dir> is required");
        return 2;
    };
    let def = manager_services::log_receiver_definition(&data_dir, &receiver_exe);
    let lr_dir = manager_services::log_receiver_dir(&data_dir);
    if let Err(e) = std::fs::create_dir_all(lr_dir.join("logs")) {
        eprintln!("install-service: cannot create {}: {e}", lr_dir.display());
        return 1;
    }
    let def_path = match manager_services::write_definition(&data_dir, &def) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("install-service: {e}");
            return 1;
        }
    };
    let host_exe = arg_value(args, "--host-exe")
        .map(PathBuf::from)
        .unwrap_or_else(|| install_dir.join("llm-service-host.exe"));
    let image_path = def.image_path(&host_exe, &def_path);
    #[cfg(windows)]
    {
        use manager_services::ServiceControl;
        match manager_services::scm::platform_control().register(&def, &image_path, None) {
            Ok(()) => {
                println!(
                    "registered service '{}' (auto-start, NT AUTHORITY\\LocalService); definition at {}",
                    def.name,
                    def_path.display()
                );
                0
            }
            Err(e) => {
                eprintln!("install-service: {e}");
                1
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = image_path;
        eprintln!("install-service is only supported on Windows");
        2
    }
}

fn cmd_uninstall_service(_args: &[String]) -> u8 {
    #[cfg(windows)]
    {
        use manager_services::ServiceControl;
        match manager_services::scm::platform_control()
            .unregister(manager_services::LOG_RECEIVER_SERVICE_NAME)
        {
            Ok(()) => {
                println!(
                    "unregistered service '{}'",
                    manager_services::LOG_RECEIVER_SERVICE_NAME
                );
                0
            }
            Err(e) => {
                eprintln!("uninstall-service: {e}");
                1
            }
        }
    }
    #[cfg(not(windows))]
    {
        eprintln!("uninstall-service is only supported on Windows");
        2
    }
}

/// `definition`: write the service definition file without
/// registering (inspection / pre-staging; no elevation needed).
fn cmd_definition(args: &[String]) -> u8 {
    let Some((data_dir, _install_dir, receiver_exe)) = install_dirs(args) else {
        eprintln!("definition: --data-dir <dir> is required");
        return 2;
    };
    let def = manager_services::log_receiver_definition(&data_dir, &receiver_exe);
    match manager_services::write_definition(&data_dir, &def) {
        Ok(path) => {
            println!("{}", path.display());
            0
        }
        Err(e) => {
            eprintln!("definition: {e}");
            1
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("install-service") => cmd_install_service(&args[1..]),
        Some("uninstall-service") => cmd_uninstall_service(&args[1..]),
        Some("definition") => cmd_definition(&args[1..]),
        Some("run") | None => match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt.block_on(serve()),
            Err(e) => {
                eprintln!("llmdil-log-receiver: cannot start runtime: {e}");
                1
            }
        },
        _ => {
            eprintln!(
                "usage: llmdil-log-receiver [run]\n       llmdil-log-receiver install-service --data-dir <dir> [--install-dir <dir>]\n       llmdil-log-receiver uninstall-service\n       llmdil-log-receiver definition --data-dir <dir> [--install-dir <dir>]"
            );
            2
        }
    };
    ExitCode::from(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailnet_pick_uses_cgnat_range_only() {
        let addrs = vec![
            Ipv4Addr::new(192, 168, 1, 10),
            Ipv4Addr::new(100, 87, 180, 108),
            Ipv4Addr::new(10, 0, 0, 5),
        ];
        assert_eq!(
            pick_tailnet_ipv4(&addrs),
            Some(Ipv4Addr::new(100, 87, 180, 108))
        );
        // 100.128.x.x is outside 100.64.0.0/10; 100.63.x.x likewise.
        assert_eq!(pick_tailnet_ipv4(&[Ipv4Addr::new(100, 128, 0, 1)]), None);
        assert_eq!(pick_tailnet_ipv4(&[Ipv4Addr::new(100, 63, 255, 1)]), None);
        assert_eq!(pick_tailnet_ipv4(&[]), None);
        // Range edges are in.
        assert_eq!(
            pick_tailnet_ipv4(&[Ipv4Addr::new(100, 64, 0, 1)]),
            Some(Ipv4Addr::new(100, 64, 0, 1))
        );
        assert_eq!(
            pick_tailnet_ipv4(&[Ipv4Addr::new(100, 127, 255, 254)]),
            Some(Ipv4Addr::new(100, 127, 255, 254))
        );
    }
}
