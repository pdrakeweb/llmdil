//! Tauri 2 shell for the Local LLM Service Manager.
//!
//! All command logic lives in the `manager-app` crate (admin API
//! plan, phase 0). This file is only the desktop adapter:
//!
//! - [`TauriSink`] forwards operation events to the webview.
//! - The `#[tauri::command]` functions and the `generate_handler!`
//!   list are generated from manager-app's operation registry
//!   (`for_each_operation!`), so the desktop surface cannot drift
//!   from the registry — and therefore from the REST API, which is
//!   generated from the same declaration.
//! - `main()` owns startup: config load, state management, and the
//!   two background loops (wizard hydration, gateway metrics).
//! - The admin API lifecycle (phase 2): when `admin_api.enabled` is
//!   set, the API is hosted in-process on all interfaces with the
//!   scoped firewall rule reconciled on enable/disable. The
//!   hand-written `admin_api_*` commands at the bottom are the
//!   Remote Access settings page's controls — desktop-local server
//!   management, not app capabilities, so they live here rather
//!   than in the operation registry and are appended to the
//!   generated handler list explicitly.
//!
//! LOAD-BEARING CONFIG: `app.withGlobalTauri` in tauri.conf.json must stay
//! `true`. The UI invokes commands through the global
//! `window.__TAURI__.core.invoke` (see ui/src/api.ts) instead of the
//! `@tauri-apps/api` npm package, because npm registry access is
//! unavailable in the build environment. Setting it to `false` breaks
//! every command call from the UI.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State};

use manager_api::auth::{AuthService, CredentialStore, WizardCredentialStore};
use manager_api::users::RoleName;
use manager_app::{registry::Op, AppState, EventSink};

/// The running admin API server plus the last lifecycle note (e.g.
/// a firewall error) for the settings page.
pub struct ApiServerSlot {
    pub server: std::sync::Mutex<Option<manager_api::ApiServer>>,
    pub note: std::sync::Mutex<Option<String>>,
}

/// The auth service shared by the API server and the desktop's
/// admin commands (one instance: enrollment codes, challenges, and
/// lockout state are in-memory per instance).
pub struct AdminAuth(pub Arc<AuthService>);

/// Forwards operation events into the webview, preserving the event
/// names and payload shapes the frontend listens for.
pub struct TauriSink(AppHandle);

impl EventSink for TauriSink {
    fn emit(&self, event: &str, payload: Value) {
        let _ = self.0.emit(event, payload);
    }
}

// One #[tauri::command] adapter per registered operation, generated
// from the registry. Each adapter packs its typed arguments into the
// registry's argument object and flattens the typed error back to
// the string form the frontend has always received.
macro_rules! tauri_adapters {
    ($(($variant:ident, $name:ident, $class:ident, $op:ident, [$($pname:ident : $pty:ty),*], $route:expr)),*) => {
        $(
            #[tauri::command]
            async fn $name(
                app: AppHandle,
                state: State<'_, Arc<AppState>>,
                $($pname: $pty),*
            ) -> Result<Value, String> {
                let args = serde_json::json!({ $(stringify!($pname): $pname),* });
                let sink: Arc<dyn EventSink> = Arc::new(TauriSink(app));
                manager_app::registry::dispatch(state.inner(), &sink, Op::$variant, args)
                    .await
                    .map_err(|e| e.to_string())
            }
        )*
    };
}
manager_app::for_each_operation!(tauri_adapters);

// The invoke handler list, from the same declaration: every
// registered operation is exposed, and a new registry entry appears
// here (and as an adapter above) without a second edit. The
// admin_api_* commands are the shell's own (see the module docs).
macro_rules! tauri_handler {
    ($(($variant:ident, $name:ident, $class:ident, $op:ident, [$($pname:ident : $pty:ty),*], $route:expr)),*) => {
        tauri::generate_handler![
            $($name),*,
            admin_api_get_status,
            admin_api_set_enabled,
            admin_api_list_users,
            admin_api_create_user,
            admin_api_update_user,
            admin_api_delete_user,
            admin_api_reset_password,
            admin_api_get_pairing,
            admin_api_create_enrollment
        ]
    };
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            // Load the persisted config; `manager_config::load` falls back
            // to the `.bak` last-known-good copy on parse/validation
            // failure (spec §10). First run (or total corruption) falls
            // back to defaults with a stderr banner.
            let data_dir = manager_config::default_data_dir();
            let config_path = data_dir.join("config.json");
            let config = match manager_config::load(&config_path) {
                Ok(cfg) => cfg,
                Err(e) => {
                    eprintln!(
                        "config load failed ({e}); using defaults: {}",
                        config_path.display()
                    );
                    manager_config::AppConfig::default_config()
                }
            };
            let admin_api = config.admin_api.clone();
            let api_data_dir = data_dir.clone();
            let app_state = Arc::new(AppState::new(config, config_path, data_dir));
            app.manage(app_state.clone());
            app.manage(ApiServerSlot {
                server: std::sync::Mutex::new(None),
                note: std::sync::Mutex::new(None),
            });
            // The API user store + auth service, shared between the
            // HTTP server and this shell's admin commands.
            let users = match tauri::async_runtime::block_on(manager_api::UserStore::load(
                &api_data_dir,
            )) {
                Ok(store) => store,
                Err(e) => {
                    eprintln!("admin api: user store failed to load ({e}); using an empty store");
                    manager_api::UserStore::in_memory()
                }
            };
            let creds: Arc<dyn CredentialStore> = Arc::new(WizardCredentialStore);
            let auth = AuthService::new(users, creds);
            app.manage(AdminAuth(auth.clone()));
            // Hydrate the wizard checklist from the machine's actual
            // state in the background (audit checks adopted into the
            // install wizard), so a relaunched app does not present
            // setup as untouched when most of it is already in place.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<Arc<AppState>>().inner().clone();
                state.restore_wizard().await;
                state.hydrate_wizard().await;
                state.persist_wizard().await;
            });
            // Gateway metrics poller: scrape LiteLLM's Prometheus
            // endpoint every 15 s into the stats store that feeds
            // the Dashboard's request/error/throughput figures.
            // Cycles skip silently while the gateway is down.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<Arc<AppState>>().inner().clone();
                loop {
                    manager_app::poll_gateway_metrics_once(&state).await;
                    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                }
            });
            // Admin REST API: serve in-process while enabled (the
            // enable/disable flow itself lives in
            // admin_api_set_enabled).
            if admin_api.enabled {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let slot = handle.state::<ApiServerSlot>();
                    let auth = handle.state::<AdminAuth>();
                    let note = start_admin_api(
                        app_state,
                        api_data_dir,
                        auth.inner().0.clone(),
                        admin_api.port,
                    )
                    .await;
                    match note {
                        Ok(server) => {
                            *slot.inner().server.lock().unwrap() = Some(server);
                        }
                        Err(e) => {
                            eprintln!("admin api: failed to start: {e}");
                            *slot.inner().note.lock().unwrap() = Some(e);
                        }
                    }
                });
            }
            Ok(())
        })
        .invoke_handler(manager_app::for_each_operation!(tauri_handler))
        .run(tauri::generate_context!())
        .expect("failed to run Local LLM Service Manager");
}

/// Start the API on all interfaces with TLS and reconcile the
/// scoped firewall rule. Shared by app startup and the settings
/// page's enable action.
async fn start_admin_api(
    app_state: Arc<AppState>,
    data_dir: std::path::PathBuf,
    auth: Arc<AuthService>,
    port: u16,
) -> Result<manager_api::ApiServer, String> {
    let tls = manager_api::tls::ensure_cert(&data_dir, &cert_sans())
        .map_err(|e| format!("TLS setup failed: {e}"))?;
    let ctx = manager_api::ApiCtx::with_auth(app_state, data_dir, auth);
    let server = manager_api::ApiServer::start_ctx(
        ctx,
        manager_api::ApiOptions {
            bind: std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            port,
            tls: Some(tls),
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    if let Err(e) = manager_api::firewall::ensure(port).await {
        // The API still serves on the LAN interfaces the host
        // firewall already permits; the settings page shows the
        // note and offers the repair by toggling.
        eprintln!("admin api: firewall rule not applied: {e}");
    }
    Ok(server)
}

/// Certificate SANs for the LAN surface: the machine hostname, the
/// primary LAN address, the Tailscale address when the CLI reports
/// one, and loopback.
fn cert_sans() -> manager_api::tls::CertSans {
    let mut dns = vec!["localhost".to_string()];
    if let Ok(name) = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")) {
        if !name.is_empty() {
            dns.push(name.to_lowercase());
        }
    }
    let mut ips = vec![
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
    ];
    if let Some(ip) = primary_lan_ip() {
        ips.push(ip);
    }
    if let Some(ip) = tailscale_ip() {
        if !ips.contains(&ip) {
            ips.push(ip);
        }
    }
    manager_api::tls::CertSans { dns, ips }
}

/// The primary LAN IPv4, found by asking the OS which local address
/// a default-route packet would use (no traffic is sent).
fn primary_lan_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let addr = socket.local_addr().ok()?;
    if addr.ip().is_loopback() {
        None
    } else {
        Some(addr.ip())
    }
}

/// The machine's Tailscale IPv4, when the Tailscale CLI answers.
fn tailscale_ip() -> Option<std::net::IpAddr> {
    let out = std::process::Command::new("tailscale")
        .args(["ip", "-4"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().next()?.trim().parse().ok()
}


/// The current `admin_api` settings, read through the app's own
/// config operation.
async fn admin_api_settings(
    st: &Arc<AppState>,
    app: &AppHandle,
) -> Result<(bool, u16), String> {
    let sink: Arc<dyn EventSink> = Arc::new(TauriSink(app.clone()));
    let cfg = manager_app::registry::dispatch(st, &sink, Op::GetConfig, json!({}))
        .await
        .map_err(|e| e.to_string())?;
    let enabled = cfg
        .pointer("/admin_api/enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let port = cfg
        .pointer("/admin_api/port")
        .and_then(Value::as_u64)
        .unwrap_or(47821) as u16;
    Ok((enabled, port))
}

fn lan_base_url(port: u16) -> String {
    match primary_lan_ip() {
        Some(ip) => format!("https://{ip}:{port}"),
        None => format!("https://127.0.0.1:{port}"),
    }
}

/// Status for the Remote Access settings card.
#[tauri::command]
async fn admin_api_get_status(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    slot: State<'_, ApiServerSlot>,
    auth: State<'_, AdminAuth>,
) -> Result<Value, String> {
    let (enabled, port) = admin_api_settings(state.inner(), &app).await?;
    let data_dir = state.inner().data_dir().to_path_buf();
    let running = slot.inner().server.lock().unwrap().is_some();
    let note = slot.inner().note.lock().unwrap().clone();
    let (users, admins) = auth.inner().0.store().read(|f| {
        (
            f.users.len(),
            f.users
                .iter()
                .filter(|u| u.role == RoleName::Admin && !u.disabled)
                .count(),
        )
    }).await;
    let firewall = match manager_api::firewall::status(port).await {
        manager_api::firewall::FirewallStatus::Ok => "ok",
        manager_api::firewall::FirewallStatus::Missing => "missing",
        manager_api::firewall::FirewallStatus::Drifted => "drifted",
        manager_api::firewall::FirewallStatus::Unsupported => "unsupported",
        manager_api::firewall::FirewallStatus::Error(_) => "error",
    };
    Ok(json!({
        "enabled": enabled,
        "running": running,
        "port": port,
        "bind": "0.0.0.0",
        "base_url": if running { Value::String(lan_base_url(port)) } else { Value::Null },
        "cert_sha256": manager_api::tls::stored_fingerprint(&data_dir),
        "firewall": firewall,
        "note": note,
        "user_count": users,
        "admin_count": admins,
    }))
}

/// Enable or disable the API: persists the choice through the
/// app's own config-write path, then starts/stops the server and
/// reconciles the firewall rule.
#[tauri::command]
async fn admin_api_set_enabled(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    slot: State<'_, ApiServerSlot>,
    auth: State<'_, AdminAuth>,
    enabled: bool,
) -> Result<Value, String> {
    let st = state.inner().clone();
    let (_, port) = admin_api_settings(&st, &app).await?;
    let sink: Arc<dyn EventSink> = Arc::new(TauriSink(app.clone()));
    manager_app::registry::dispatch(
        &st,
        &sink,
        Op::UpdateConfig,
        json!({ "patch": { "admin_api": { "enabled": enabled, "port": port } } }),
    )
    .await
    .map_err(|e| e.to_string())?;
    if enabled {
        let data_dir = st.data_dir().to_path_buf();
        match start_admin_api(st.clone(), data_dir, auth.inner().0.clone(), port).await {
            Ok(server) => {
                *slot.inner().server.lock().unwrap() = Some(server);
                *slot.inner().note.lock().unwrap() = None;
            }
            Err(e) => {
                *slot.inner().note.lock().unwrap() = Some(e.clone());
                return Err(e);
            }
        }
    } else {
        let server = slot.inner().server.lock().unwrap().take();
        if let Some(server) = server {
            server.shutdown().await;
        }
        if let Err(e) = manager_api::firewall::remove(port).await {
            *slot.inner().note.lock().unwrap() =
                Some(format!("firewall rule removal failed: {e}"));
        }
    }
    admin_api_get_status(app, state, slot, auth).await
}

#[tauri::command]
async fn admin_api_list_users(
    auth: State<'_, AdminAuth>,
) -> Result<Value, String> {
    let users = auth
        .inner()
        .0
        .store()
        .read(|f| {
            f.users
                .iter()
                .map(|u| serde_json::to_value(manager_api::auth::UserView::from(u)).unwrap_or(Value::Null))
                .collect::<Vec<_>>()
        })
        .await;
    Ok(json!({ "users": users }))
}

#[tauri::command]
async fn admin_api_create_user(
    auth: State<'_, AdminAuth>,
    username: String,
    password: String,
    role: String,
) -> Result<Value, String> {
    let role = RoleName::parse(&role)
        .ok_or_else(|| format!("unknown role `{role}`"))?;
    let view = auth
        .inner()
        .0
        .create_user(&username, &password, role)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(view).map_err(|e| e.to_string())
}

#[tauri::command]
async fn admin_api_update_user(
    auth: State<'_, AdminAuth>,
    id: String,
    role: Option<String>,
    disabled: Option<bool>,
) -> Result<Value, String> {
    let role = match role {
        Some(r) => Some(
            RoleName::parse(&r).ok_or_else(|| format!("unknown role `{r}`"))?,
        ),
        None => None,
    };
    let view = auth
        .inner()
        .0
        .update_user(&id, role, disabled)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(view).map_err(|e| e.to_string())
}

#[tauri::command]
async fn admin_api_delete_user(
    auth: State<'_, AdminAuth>,
    id: String,
) -> Result<Value, String> {
    // The desktop operator is the local administrator; the actor id
    // is a sentinel that is never a user id, so the self-delete
    // guard cannot trip here — the last-admin guard still applies.
    auth.inner()
        .0
        .delete_user("desktop", &id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "ok": true }))
}

#[tauri::command]
async fn admin_api_reset_password(
    auth: State<'_, AdminAuth>,
    id: String,
) -> Result<Value, String> {
    let temp = auth
        .inner()
        .0
        .admin_reset_password(&id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "temporary_password": temp }))
}

/// The pairing payload for the enrollment QR (without a code):
/// base URL + certificate fingerprint.
#[tauri::command]
async fn admin_api_get_pairing(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
) -> Result<Value, String> {
    let (_, port) = admin_api_settings(state.inner(), &app).await?;
    let data_dir = state.inner().data_dir().to_path_buf();
    Ok(json!({
        "base_url": lan_base_url(port),
        "cert_sha256": manager_api::tls::stored_fingerprint(&data_dir),
    }))
}

/// Create a one-time enrollment code for a user and return the full
/// pairing payload the QR encodes.
#[tauri::command]
async fn admin_api_create_enrollment(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    auth: State<'_, AdminAuth>,
    user_id: String,
) -> Result<Value, String> {
    let (_, port) = admin_api_settings(state.inner(), &app).await?;
    let data_dir = state.inner().data_dir().to_path_buf();
    let code = auth
        .inner()
        .0
        .create_enrollment_code(&user_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "base_url": lan_base_url(port),
        "cert_sha256": manager_api::tls::stored_fingerprint(&data_dir),
        "enrollment_code": code,
    }))
}
