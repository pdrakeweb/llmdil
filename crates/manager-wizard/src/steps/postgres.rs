//! Step 5: app-managed PostgreSQL (install, start/stop, readiness, status).

use super::*;

// ---------------------------------------------------------------------------
// 5. App-managed PostgreSQL
// ---------------------------------------------------------------------------

/// The pinned PostgreSQL distribution (EnterpriseDB binaries zip).
/// The hash is self-computed from the downloaded artifact (Oct 15,
/// 2026): EDB publishes no official SHA-256 for these zips — the
/// same trust model as the llama.cpp pins.
#[derive(Debug, Clone)]
pub struct PostgresPin {
    pub version: String,
    pub asset: String,
    pub sha256: String,
    pub size: u64,
}

impl PostgresPin {
    pub fn default_pin() -> Self {
        Self {
            version: "18.6-1".to_string(),
            asset: "postgresql-18.6-1-windows-x64-binaries.zip".to_string(),
            sha256: "fbe23da234ee31547bf8a36d29dfd81e82b849df2d2b78d2eecb43d360252f8c".to_string(),
            size: 343_808_005,
        }
    }

    pub fn url(&self) -> String {
        format!("https://get.enterprisedb.com/postgresql/{}", self.asset)
    }
}

/// Root of the app-managed PostgreSQL install: `<data_dir>/postgres`.
pub fn postgres_root(data_dir: &Path) -> PathBuf {
    data_dir.join("postgres")
}

/// `<data_dir>/postgres/bin` — the extracted server binaries.
pub fn postgres_bin_dir(data_dir: &Path) -> PathBuf {
    postgres_root(data_dir).join("bin")
}

/// `<data_dir>/postgres/data` — the database cluster directory.
pub fn postgres_data_dir(data_dir: &Path) -> PathBuf {
    postgres_root(data_dir).join("data")
}

pub(crate) fn postgres_marker_path(data_dir: &Path) -> PathBuf {
    postgres_root(data_dir).join(".installed.json")
}

fn postgres_zip_path(data_dir: &Path, pin: &PostgresPin) -> PathBuf {
    postgres_root(data_dir).join("downloads").join(&pin.asset)
}

/// The server's own log file, in the app's logs directory alongside
/// the gateway logs (`pg_ctl -l` appends there).
pub fn postgres_log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("logs").join("postgres.log")
}

/// The database the gateway uses.
pub const POSTGRES_DATABASE: &str = "litellm";

/// Percent-encode a URL userinfo component (RFC 3986): everything
/// outside the unreserved set is `%XX`-escaped. App-generated
/// passwords are hex and pass through unchanged; a password a user
/// sets by hand may contain `@`, `:`, `/`, or spaces, which raw
/// would corrupt the URL's authority parsing (code-review 2.9).
fn encode_userinfo(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The `DATABASE_URL` the gateway receives: the password is
/// percent-encoded for the userinfo slot (app-generated passwords
/// are hex, hence unchanged), the user is the cluster superuser,
/// the database is [`POSTGRES_DATABASE`].
pub fn postgres_database_url(config: &AppConfig, password: &str) -> String {
    format!(
        "postgresql://postgres:{}@127.0.0.1:{}/{POSTGRES_DATABASE}",
        encode_userinfo(password),
        config.postgres.port
    )
}

/// Whether the install-postgres step has completed: the server
/// binaries in place, and either the install marker or an
/// initialized cluster. The marker is bookkeeping the step writes
/// last; a lost marker must not strand a working install (the step
/// rewrites it on its next run), so the binaries plus the cluster
/// are accepted as the ground truth.
pub async fn postgres_installed(ops: &dyn SystemOps, data_dir: &Path) -> bool {
    if !ops
        .file_exists(&postgres_bin_dir(data_dir).join("pg_ctl.exe"))
        .await
    {
        return false;
    }
    ops.file_exists(&postgres_marker_path(data_dir)).await
        || ops
            .file_exists(&postgres_data_dir(data_dir).join("PG_VERSION"))
            .await
}

/// Whether the app-managed server accepts connections on the
/// configured port (`pg_isready`). The start path's readiness probe;
/// status polling uses the cheaper TCP connect in [`postgres_status`].
pub async fn postgres_ready(ops: &dyn SystemOps, config: &AppConfig, data_dir: &Path) -> bool {
    let cmd = PlannedCommand::new(vec![
        postgres_bin_dir(data_dir)
            .join("pg_isready.exe")
            .display()
            .to_string(),
        "-h".to_string(),
        "127.0.0.1".to_string(),
        "-p".to_string(),
        config.postgres.port.to_string(),
    ]);
    matches!(ops.run_command(&cmd).await, Ok(out) if out.exit_code == 0)
}

async fn postgres_log_tail(ops: &dyn SystemOps, data_dir: &Path) -> String {
    match ops.read_file(&postgres_log_path(data_dir)).await {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text.lines().collect();
            let tail = &lines[lines.len().saturating_sub(15)..];
            tail.join("\n")
        }
        Err(_) => "(no server log yet)".to_string(),
    }
}

/// Runner budget for the pg_ctl launch invocation. `pg_ctl start
/// --no-wait` returns as soon as the postmaster process exists, so
/// this bounds a wedged launch only — never a starting postmaster
/// (see [`ensure_postgres_running`]): the readiness wait below is
/// a poll, not a command budget, and its expiry kills nothing.
const POSTGRES_LAUNCH_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);
/// Readiness poll cadence (the historical poll slept 500 ms).
const POSTGRES_READY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
/// How long to wait for the postmaster to write `postmaster.pid`
/// after a successful launch before concluding the launch did not
/// actually start a server. The pid file is one of the first things
/// the postmaster writes, so this is a grace for slow process
/// creation under load, not a second readiness window: once the pid
/// exists, the settle wait below takes over with its postmaster
/// requirement engaged.
const POSTGRES_PID_GRACE: std::time::Duration = std::time::Duration::from_secs(10);
/// Readiness window after launch. This restates the previous
/// success envelope exactly — the old 180 s runner budget plus the
/// old 120 s settle poll — as a single kill-free poll window:
/// expiry stops polling and reports with the log tail; it kills
/// nothing, so it cannot reintroduce the tracker #43 failure mode
/// (a budget kill taking down a recovering postmaster). Crash
/// recovery over a large WAL backlog is what needs this length.
pub(crate) const POSTGRES_SETTLE_WAIT: std::time::Duration = std::time::Duration::from_secs(120);
/// How long to wait for an already-running postmaster (found via
/// `postmaster.pid`) to reach readiness before reporting it stuck.
const POSTGRES_ADOPT_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// The `pg_ctl start` command for the app-managed server, shared
/// by the runtime start ([`ensure_postgres_running`]), the install
/// step's plan, and the start-gateway manual block, so all three
/// run and show the same command. `--no-wait`: pg_ctl returns as
/// soon as the postmaster process exists and the caller polls
/// `pg_isready` itself (PostgreSQL docs: with waiting disabled,
/// "the server log file or an external monitoring system would
/// have to be used to check the progress and success"). Blocking
/// on pg_ctl's own wait cost every cold start the full wait budget:
/// pg_ctl demonstrably survived its `-t 120` bound to the 180 s
/// runner kill on 2026-11-02 while the server was already ready
/// 177 ms after that kill — nothing probed readiness during the
/// wait, so the wait measured nothing and saved nothing.
pub(crate) fn postgres_start_command(data_dir: &Path, port: u16) -> PlannedCommand {
    let port_opt = format!("-p {port} -h 127.0.0.1");
    let log_arg = postgres_log_path(data_dir).display().to_string();
    pg_ctl_command(
        data_dir,
        &["-l", &log_arg, "-o", &port_opt, "--no-wait", "start"],
    )
}

/// `<data_dir>/postgres/data/postmaster.pid` — written by the
/// postmaster at startup and removed on a clean stop; its first
/// line is the postmaster's pid.
pub(crate) fn postmaster_pid_path(data_dir: &Path) -> PathBuf {
    postgres_data_dir(data_dir).join("postmaster.pid")
}

/// The pid recorded in `postmaster.pid`, when the file exists and
/// its first line parses. Absent or unparseable both read as "no
/// recorded postmaster".
async fn read_postmaster_pid(ops: &dyn SystemOps, data_dir: &Path) -> Option<u32> {
    read_postmaster_info(ops, data_dir).await.map(|i| i.pid)
}

/// What `postmaster.pid` records: line 1 the postmaster's pid, line 4
/// the port it listens on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PostmasterInfo {
    pub(crate) pid: u32,
    /// `None` when line 4 is missing or unparseable (a pid file
    /// still being written, or a truncated one).
    pub(crate) port: Option<u16>,
}

/// Parse `postmaster.pid` text (see [`PostmasterInfo`]).
pub(crate) fn parse_postmaster_pid(text: &str) -> Option<PostmasterInfo> {
    let mut lines = text.lines();
    let pid = lines.next()?.trim().parse().ok()?;
    let port = lines.nth(2).and_then(|l| l.trim().parse().ok());
    Some(PostmasterInfo { pid, port })
}

async fn read_postmaster_info(ops: &dyn SystemOps, data_dir: &Path) -> Option<PostmasterInfo> {
    let bytes = ops.read_file(&postmaster_pid_path(data_dir)).await.ok()?;
    parse_postmaster_pid(&String::from_utf8_lossy(&bytes))
}

/// A point-in-time view of the app-managed PostgreSQL server, cheap
/// enough for status polling: a pid-file read, a liveness check and
/// a 300 ms TCP connect — never `pg_isready`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PostgresStatus {
    /// The binaries and cluster are in place ([`postgres_installed`]).
    pub installed: bool,
    /// The postmaster pid `postmaster.pid` records.
    pub pid: Option<u32>,
    /// That pid is alive and not a recycled pid of another program.
    pub pid_alive: bool,
    /// The port `postmaster.pid` records (line 4).
    pub pid_port: Option<u16>,
    /// The configured port (`config.postgres.port`).
    pub configured_port: u16,
    /// 127.0.0.1:<configured port> accepted a TCP connection.
    pub accepting: bool,
}

impl PostgresStatus {
    /// A live postmaster listens on a port other than the configured
    /// one: the next gateway start restarts it on the new port.
    pub fn port_mismatch(&self) -> bool {
        self.pid_alive && self.pid_port.is_some_and(|p| p != self.configured_port)
    }
}

/// Status of the app-managed PostgreSQL server (see [`PostgresStatus`]).
pub async fn postgres_status(
    ops: &dyn SystemOps,
    config: &AppConfig,
    data_dir: &Path,
) -> PostgresStatus {
    let configured_port = config.postgres.port;
    if !postgres_installed(ops, data_dir).await {
        return PostgresStatus {
            installed: false,
            pid: None,
            pid_alive: false,
            pid_port: None,
            configured_port,
            accepting: false,
        };
    }
    let info = read_postmaster_info(ops, data_dir).await;
    let pid_alive = info.is_some_and(|i| {
        ops.process_alive(i.pid) && ops.process_image_is(i.pid, "postgres.exe") != Some(false)
    });
    let accepting = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        tokio::net::TcpStream::connect(("127.0.0.1", configured_port)),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false);
    PostgresStatus {
        installed: true,
        pid: info.map(|i| i.pid),
        pid_alive,
        pid_port: info.and_then(|i| i.port),
        configured_port,
        accepting,
    }
}

/// How [`ensure_postgres_running_detailed`] got a ready server.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PostgresReady {
    /// A server was already accepting connections and was adopted.
    Adopted { pid: Option<u32> },
    /// A postmaster that was still starting (e.g. crash recovery) was
    /// adopted and awaited.
    AdoptedStarting { pid: u32 },
    /// This call started the server.
    Started { pid: Option<u32>, secs: f64 },
}

impl PostgresReady {
    /// Marker/log phrasing ("adopted pid 812", "started pid 812 in 3.1 s").
    pub fn describe(&self) -> String {
        let pid = |p: &Option<u32>| p.map(|p| format!(" pid {p}")).unwrap_or_default();
        match self {
            PostgresReady::Adopted { pid: p } => format!("adopted{}", pid(p)),
            PostgresReady::AdoptedStarting { pid } => {
                format!("adopted pid {pid} (was still starting)")
            }
            PostgresReady::Started { pid: p, secs } => format!("started{} in {secs:.1} s", pid(p)),
        }
    }
}

/// Outcome of [`wait_for_postgres_ready`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PostgresWait {
    /// `pg_isready` answered success.
    Ready,
    /// The window elapsed without readiness.
    TimedOut,
    /// A postmaster was required to exist and none does (the pid
    /// file is gone, or its recorded pid is not running): nothing
    /// is coming up, so waiting longer cannot help.
    PostmasterGone,
}

/// How often a readiness wait reports itself: every this many
/// polls (~10 s at the 500 ms cadence) the wait streams the new
/// server-log lines into the step log and notes the elapsed time
/// on the live sink. A starting postmaster can spend minutes in
/// crash recovery; without the ticks the wait is a silent window
/// indistinguishable from a hang (field report 2026-10-19).
const POSTGRES_WAIT_TICK_POLLS: u32 = 20;

/// Poll `pg_isready` until it answers, the window elapses, or —
/// with `require_postmaster` — the postmaster disappears (a start
/// that already failed fast must not burn the whole window).
/// While waiting, the postmaster's own log lines stream into the
/// step log (see [`POSTGRES_WAIT_TICK_POLLS`]), so a failing
/// start's FATAL lines surface as they are written instead of
/// only as a tail after the wait has already failed.
pub(crate) async fn wait_for_postgres_ready(
    ctx: &WizardContext,
    data_dir: &Path,
    max: std::time::Duration,
    require_postmaster: bool,
    log: &mut Vec<String>,
) -> PostgresWait {
    wait_for_postgres_ready_ticked(
        ctx,
        data_dir,
        max,
        require_postmaster,
        log,
        POSTGRES_WAIT_TICK_POLLS,
    )
    .await
}

/// [`wait_for_postgres_ready`] with the tick cadence as a
/// parameter, so tests can tick every poll instead of every
/// ~10 s.
pub(crate) async fn wait_for_postgres_ready_ticked(
    ctx: &WizardContext,
    data_dir: &Path,
    max: std::time::Duration,
    require_postmaster: bool,
    log: &mut Vec<String>,
    tick_polls: u32,
) -> PostgresWait {
    let ops = ctx.ops.as_ref();
    let config = &ctx.config;
    let log_path = postgres_log_path(data_dir);
    let started = std::time::Instant::now();
    let mut streamer = FileLineStreamer::new();
    streamer.prime(ops, &log_path).await;
    let mut polls: u32 = 0;
    loop {
        if postgres_ready(ops, config, data_dir).await {
            streamer
                .stream_new(ctx, ops, &log_path, "postgres: ", log)
                .await;
            return PostgresWait::Ready;
        }
        if require_postmaster {
            match read_postmaster_pid(ops, data_dir).await {
                Some(pid) if ops.process_alive(pid) => {}
                _ => {
                    streamer
                        .stream_new(ctx, ops, &log_path, "postgres: ", log)
                        .await;
                    return PostgresWait::PostmasterGone;
                }
            }
        }
        if started.elapsed() >= max {
            return PostgresWait::TimedOut;
        }
        tokio::time::sleep(POSTGRES_READY_POLL_INTERVAL).await;
        polls += 1;
        if tick_polls > 0 && polls.is_multiple_of(tick_polls) {
            streamer
                .stream_new(ctx, ops, &log_path, "postgres: ", log)
                .await;
            ctx.emit_log(&format!(
                "waiting for PostgreSQL on 127.0.0.1:{} — {:.0}s elapsed",
                config.postgres.port,
                started.elapsed().as_secs_f64()
            ));
        }
    }
}

pub(crate) fn pg_ctl_command(data_dir: &Path, args: &[&str]) -> PlannedCommand {
    let mut argv = vec![
        postgres_bin_dir(data_dir)
            .join("pg_ctl.exe")
            .display()
            .to_string(),
        "-D".to_string(),
        postgres_data_dir(data_dir).display().to_string(),
    ];
    argv.extend(args.iter().map(|a| a.to_string()));
    PlannedCommand::new(argv)
}

/// Start the app-managed PostgreSQL server if it is not already
/// accepting connections, and return the gateway's `DATABASE_URL`.
/// The caller must have established [`postgres_installed`] first.
/// Failure carries the server log tail: the decisive case is
/// PostgreSQL's refusal to run under an administrative token, which
/// is recorded in that log verbatim.
pub async fn ensure_postgres_running(
    ctx: &WizardContext,
    log: &mut Vec<String>,
) -> Result<String, String> {
    ensure_postgres_running_detailed(ctx, log)
        .await
        .map(|(url, _)| url)
}

/// [`ensure_postgres_running`], also saying how the server became
/// ready (adopted vs started), with lifecycle markers in
/// `logs/postgres.lifecycle.log` for every adopt, start, ready, port
/// change and failure.
pub async fn ensure_postgres_running_detailed(
    ctx: &WizardContext,
    log: &mut Vec<String>,
) -> Result<(String, PostgresReady), String> {
    let result = ensure_postgres_running_inner(ctx, log).await;
    if let Err(e) = &result {
        postgres_mark(
            ctx.ops.as_ref(),
            &ctx.data_dir,
            "start failed",
            Some(first_line(e)),
        );
    }
    result
}

async fn ensure_postgres_running_inner(
    ctx: &WizardContext,
    log: &mut Vec<String>,
) -> Result<(String, PostgresReady), String> {
    let ops = ctx.ops.as_ref();
    let config = &ctx.config;
    let data_dir = ctx.data_dir.as_path();
    let password = crate::ensure_postgres_password(ops)
        .await
        .map_err(|e| format!("could not obtain the PostgreSQL password: {e}"))?;
    let url = postgres_database_url(config, &password);
    // Port change (OS-5): a live postmaster of this cluster listening
    // on a port other than the configured one cannot be adopted (the
    // gateway would connect to the new port and wait for nothing),
    // and starting a second server against the same data directory
    // is refused. Stop it, then start on the configured port below.
    // This is the only case in which a gateway (re)start restarts
    // PostgreSQL.
    if let Some(info) = read_postmaster_info(ops, data_dir).await {
        let ours = ops.process_alive(info.pid)
            && ops.process_image_is(info.pid, "postgres.exe") != Some(false);
        if let Some(old_port) = info.port.filter(|p| ours && *p != config.postgres.port) {
            let pid = info.pid;
            push_log(
                ctx,
                log,
                format!(
                    "PostgreSQL (pid {pid}) is running on port {old_port} but the configured port is {} — restarting it on the new port",
                    config.postgres.port
                ),
            );
            postgres_mark(
                ops,
                data_dir,
                "port changed",
                Some(&format!(
                    "pid {pid} listens on {old_port}, configured {} — restarting",
                    config.postgres.port
                )),
            );
            if let Err(e) = stop_postgres_server(ops, data_dir).await {
                return Err(format!(
                    "could not stop PostgreSQL (pid {pid}) on its old port {old_port}: {e}"
                ));
            }
            if ops.process_alive(pid) {
                return Err(format!(
                    "PostgreSQL (pid {pid}) is still running on its old port {old_port} after pg_ctl stop; it must stop before it can start on port {}",
                    config.postgres.port
                ));
            }
            push_log(
                ctx,
                log,
                format!("stopped PostgreSQL (pid {pid}) on its old port {old_port}"),
            );
        }
    }
    if postgres_ready(ops, config, data_dir).await {
        push_log(
            ctx,
            log,
            format!(
                "PostgreSQL already running on 127.0.0.1:{}",
                config.postgres.port
            ),
        );
        let pid = read_postmaster_pid(ops, data_dir).await;
        postgres_mark(
            ops,
            data_dir,
            "adopted running server",
            Some(&format!(
                "{}port {}",
                pid.map(|p| format!("pid {p}, ")).unwrap_or_default(),
                config.postgres.port
            )),
        );
        return Ok((url, PostgresReady::Adopted { pid }));
    }
    // A postmaster the readiness probe cannot see yet: one left
    // running by an earlier session (PostgreSQL stops with the
    // gateway stop, but an app crash, a killed process tree, or a
    // machine sleep can leave it behind) that is still starting —
    // crash recovery after an unclean shutdown runs before it
    // accepts connections, so `pg_isready` fails while it works.
    // Starting a second postmaster against the same data directory
    // cannot help (PostgreSQL refuses a second lock holder), so an
    // existing one is adopted and awaited. A pid file whose pid is
    // dead — or has been recycled by an unrelated process (Windows
    // pid reuse; the image check tells the two apart) — is stale:
    // remove it, so the fresh start below is not refused over a
    // lock file no live server owns.
    if let Some(pid) = read_postmaster_pid(ops, data_dir).await {
        if !ops.process_alive(pid) {
            push_log(
                ctx,
                log,
                format!("removing stale postmaster.pid: recorded pid {pid} is not running"),
            );
            let _ = ops.remove_file(&postmaster_pid_path(data_dir)).await;
        } else if ops.process_image_is(pid, "postgres.exe") == Some(false) {
            push_log(
                ctx,
                log,
                format!(
                    "removing stale postmaster.pid: recorded pid {pid} now belongs to a different program (pid reuse)"
                ),
            );
            let _ = ops.remove_file(&postmaster_pid_path(data_dir)).await;
        } else {
            push_log(
                ctx,
                log,
                format!(
                    "PostgreSQL postmaster (pid {pid}) is running but not accepting connections yet — waiting for it instead of starting a second server"
                ),
            );
            match wait_for_postgres_ready(ctx, data_dir, POSTGRES_ADOPT_WAIT, true, log).await {
                PostgresWait::Ready => {
                    push_log(
                        ctx,
                        log,
                        format!(
                            "PostgreSQL is accepting connections on 127.0.0.1:{} (existing postmaster, pid {pid})",
                            config.postgres.port
                        ),
                    );
                    postgres_mark(
                        ops,
                        data_dir,
                        "adopted starting server",
                        Some(&format!(
                            "pid {pid}, ready on port {}",
                            config.postgres.port
                        )),
                    );
                    return Ok((url, PostgresReady::AdoptedStarting { pid }));
                }
                PostgresWait::PostmasterGone => {
                    push_log(
                        ctx,
                        log,
                        "the existing postmaster exited while starting; starting PostgreSQL afresh"
                            .to_string(),
                    );
                    let _ = ops.remove_file(&postmaster_pid_path(data_dir)).await;
                }
                PostgresWait::TimedOut => {
                    let tail = postgres_log_tail(ops, data_dir).await;
                    return Err(format!(
                        "PostgreSQL postmaster (pid {pid}) is running but did not accept connections within {} s — it may be stuck in startup; server log tail:\n{tail}",
                        POSTGRES_ADOPT_WAIT.as_secs()
                    ));
                }
            }
        }
    }
    // Launch, don't wait: `pg_ctl start --no-wait` returns once the
    // postmaster process exists, and readiness is the pg_isready
    // poll below. The previous shape blocked on pg_ctl's own wait
    // (`-t` 120 under a 180 s runner budget) and paid the full
    // budget on every slow cold start — pg_ctl survived its `-t`
    // bound to the runner kill on 2026-11-02 while the server was
    // already ready 177 ms later, because nothing probed readiness
    // during the wait. The old shape also carried the tracker #43
    // hazard in miniature: a budget expiry tree-kills the waiter,
    // and only platform luck kept the postmaster alive. Here there
    // is no waiter to kill: the launch returns in milliseconds, the
    // poll kills nothing on expiry, and a fast FATAL still fails
    // fast via the postmaster requirement below.
    let launched = std::time::Instant::now();
    let start = postgres_start_command(data_dir, config.postgres.port);
    postgres_mark(
        ops,
        data_dir,
        "starting",
        Some(&format!("pg_ctl start on port {}", config.postgres.port)),
    );
    let start_failure = match run_command_logged(ctx, log, &start, Some(POSTGRES_LAUNCH_BUDGET))
        .await
    {
        Ok(out) if out.exit_code == 0 => {
            push_log(
                ctx,
                log,
                format!(
                    "PostgreSQL launch accepted on 127.0.0.1:{} (pg_ctl --no-wait, polling readiness; log: {})",
                    config.postgres.port,
                    postgres_log_path(data_dir).display()
                ),
            );
            None
        }
        Ok(out) => Some(format!(
            "pg_ctl exit {}: {}",
            out.exit_code,
            out.stderr.trim()
        )),
        Err(e) => Some(e.to_string()),
    };
    // After a successful launch the postmaster may not have written
    // `postmaster.pid` yet when the first poll runs; the postmaster
    // requirement below would misread that as a dead start. Allow
    // the start a short grace to show either signal of life — a
    // live pid file, or a ready probe (a server answering
    // `pg_isready` is up regardless of pid-file timing) — then
    // engage the requirement: a launch that produces neither
    // signal fails here, with the tail, instead of burning the
    // whole settle window.
    if start_failure.is_none() {
        let grace_started = std::time::Instant::now();
        loop {
            let pid_alive = matches!(
                read_postmaster_pid(ops, data_dir).await,
                Some(pid) if ops.process_alive(pid)
            );
            if pid_alive || postgres_ready(ops, config, data_dir).await {
                break;
            }
            if grace_started.elapsed() >= POSTGRES_PID_GRACE {
                let tail = postgres_log_tail(ops, data_dir).await;
                return Err(format!(
                    "PostgreSQL launch reported success but no postmaster appeared within {} s — server log tail:\n{tail}",
                    POSTGRES_PID_GRACE.as_secs()
                ));
            }
            tokio::time::sleep(POSTGRES_READY_POLL_INTERVAL).await;
        }
    }
    // The definitive signal, either way. After a launch failure
    // the postmaster may still be starting — a previous attempt's
    // server, or one this launch forked before failing — so poll
    // with the postmaster requirement: it adopts a live starter
    // and exits fast when no postmaster exists (a fast FATAL must
    // not burn the window).
    let wait = wait_for_postgres_ready(ctx, data_dir, POSTGRES_SETTLE_WAIT, true, log).await;
    match wait {
        PostgresWait::Ready => {
            let secs = launched.elapsed().as_secs_f64();
            let pid = read_postmaster_pid(ops, data_dir).await;
            postgres_mark(
                ops,
                data_dir,
                "ready",
                Some(&format!(
                    "{}port {} after {secs:.1} s",
                    pid.map(|p| format!("pid {p}, ")).unwrap_or_default(),
                    config.postgres.port
                )),
            );
            if start_failure.is_some() {
                push_log(
                    ctx,
                    log,
                    "PostgreSQL became ready after pg_ctl launch returned without confirming — startup completed in the background"
                        .to_string(),
                );
            } else {
                push_log(
                    ctx,
                    log,
                    format!(
                        "PostgreSQL ready on 127.0.0.1:{} after {:.0}s",
                        config.postgres.port,
                        launched.elapsed().as_secs_f64()
                    ),
                );
            }
            Ok((url, PostgresReady::Started { pid, secs }))
        }
        _ => {
            let tail = postgres_log_tail(ops, data_dir).await;
            match start_failure {
                Some(failure) => Err(format!(
                    "PostgreSQL failed to start ({failure}) — server log tail:\n{tail}"
                )),
                None => Err(format!(
                    "PostgreSQL did not become ready on 127.0.0.1:{} — server log tail:\n{tail}",
                    config.postgres.port
                )),
            }
        }
    }
}

/// Stop the app-managed PostgreSQL server (`pg_ctl stop -m fast`:
/// rolls back in-flight transactions, disconnects clients,
/// checkpoints, exits). Best-effort semantics for shutdown paths:
/// Ok(true) when a running server was stopped, Ok(false) when it was
/// not installed or not running.
pub async fn stop_postgres_server(
    ops: &dyn SystemOps,
    data_dir: &Path,
) -> Result<bool, WizardError> {
    if !postgres_installed(ops, data_dir).await {
        return Ok(false);
    }
    let stop = pg_ctl_command(data_dir, &["stop", "-m", "fast"]);
    let result = match ops.run_command(&stop).await {
        Ok(out) if out.exit_code == 0 => Ok(true),
        // "no server running" exits non-zero: the desired end state.
        Ok(_) => Ok(false),
        Err(e) => Err(e),
    };
    match &result {
        Ok(true) => postgres_mark(ops, data_dir, "stopped (pg_ctl stop -m fast)", None),
        Ok(false) => postgres_mark(ops, data_dir, "stop: no server was running", None),
        Err(e) => postgres_mark(ops, data_dir, "stop failed", Some(&e.to_string())),
    }
    result
}

/// Step 5: install the pinned PostgreSQL binaries under the app
/// data dir, initialize the database cluster, and start the server
/// the LiteLLM gateway uses as its database (Pete's decision,
/// 2026-10-15: the LiteLLM admin UI cannot be logged into at all
/// without one — its schema is PostgreSQL-only). Only `bin`, `lib`,
/// `share`, and `include` are extracted from the distribution zip;
/// the rest (pgAdmin alone is ~684 MB) is dead weight.
/// Remove-on-drop guard for the initdb password file: the file
/// holds the cluster superuser password in plaintext and must not
/// outlive the initdb run — including when the step future is
/// dropped mid-run (app closed, step timeout), which no awaited
/// cleanup line can cover. Removal is synchronous `std::fs`
/// because `Drop` cannot await the ops trait; the step also removes
/// the file through the ops on its normal paths (under the fake
/// ops the file exists only in the fake filesystem, where the
/// ops removal is the one that lands).
struct PwFileGuard(PathBuf);

impl Drop for PwFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub struct InstallPostgres {
    pub pin: Option<PostgresPin>,
}

impl InstallPostgres {
    pub fn unconfigured() -> Self {
        Self { pin: None }
    }
    pub fn with_pin(pin: PostgresPin) -> Self {
        Self { pin: Some(pin) }
    }

    async fn binaries_present(&self, ctx: &WizardContext) -> bool {
        let bin = postgres_bin_dir(&ctx.data_dir);
        for exe in [
            "pg_ctl.exe",
            "initdb.exe",
            "postgres.exe",
            "psql.exe",
            "createdb.exe",
        ] {
            if !ctx.ops.file_exists(&bin.join(exe)).await {
                return false;
            }
        }
        true
    }

    async fn cluster_initialized(&self, ctx: &WizardContext) -> bool {
        // PG_VERSION is written by initdb and identifies a cluster.
        ctx.ops
            .file_exists(&postgres_data_dir(&ctx.data_dir).join("PG_VERSION"))
            .await
    }

    fn initdb_command(&self, ctx: &WizardContext) -> PlannedCommand {
        PlannedCommand::new(vec![
            postgres_bin_dir(&ctx.data_dir)
                .join("initdb.exe")
                .display()
                .to_string(),
            "-D".to_string(),
            postgres_data_dir(&ctx.data_dir).display().to_string(),
            "-U".to_string(),
            "postgres".to_string(),
            "-A".to_string(),
            "scram-sha-256".to_string(),
            "--pwfile".to_string(),
            postgres_root(&ctx.data_dir)
                .join("pwfile.tmp")
                .display()
                .to_string(),
            "-E".to_string(),
            "UTF8".to_string(),
        ])
    }

    fn createdb_command(&self, ctx: &WizardContext) -> PlannedCommand {
        PlannedCommand::new(vec![
            postgres_bin_dir(&ctx.data_dir)
                .join("createdb.exe")
                .display()
                .to_string(),
            "-h".to_string(),
            "127.0.0.1".to_string(),
            "-p".to_string(),
            ctx.config.postgres.port.to_string(),
            "-U".to_string(),
            "postgres".to_string(),
            POSTGRES_DATABASE.to_string(),
        ])
    }
}

#[async_trait]
impl WizardStep for InstallPostgres {
    fn id(&self) -> &str {
        STEP_INSTALL_POSTGRES
    }
    fn name(&self) -> &str {
        "Install PostgreSQL (gateway database)"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![STEP_DETECT_HARDWARE.to_string()]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        let Some(pin) = self.pin.as_ref() else {
            return Vec::new();
        };
        vec![
            PlannedCommand::new(vec![
                "download".to_string(),
                pin.url(),
                postgres_zip_path(&ctx.data_dir, pin).display().to_string(),
            ]),
            self.initdb_command(ctx),
            postgres_start_command(&ctx.data_dir, ctx.config.postgres.port),
            self.createdb_command(ctx),
        ]
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("re-run install-postgres; if the server refuses to start, check the PostgreSQL log under the data dir's logs folder (running the manager itself elevated prevents PostgreSQL from starting)")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let pin = match &self.pin {
            Some(p) => p.clone(),
            None => {
                return Ok(StepOutcome::failed(
                    "no PostgreSQL build pinned",
                    vec![
                        "this build ships without a built-in PostgreSQL pin; that is a packaging error in the app, not a configuration problem".to_string(),
                        "failing closed: will not download an unpinned database server".to_string(),
                    ],
                ))
            }
        };
        let mut log = vec![format!(
            "pinned PostgreSQL: {} ({})",
            pin.version, pin.asset
        )];

        match ctx.mode {
            RunMode::Audit => {
                let mut problems = Vec::new();
                if !ctx
                    .ops
                    .file_exists(&postgres_marker_path(&ctx.data_dir))
                    .await
                {
                    problems.push(
                        "PostgreSQL not installed; run the install-postgres step".to_string(),
                    );
                }
                if !self.binaries_present(ctx).await {
                    problems.push("PostgreSQL binaries missing or incomplete".to_string());
                }
                if !self.cluster_initialized(ctx).await {
                    problems.push("database cluster not initialized".to_string());
                }
                if problems.is_empty() {
                    return Ok(StepOutcome::done(
                        format!(
                            "PostgreSQL {} verified (binaries and cluster present)",
                            pin.version
                        ),
                        log,
                    ));
                }
                return Ok(StepOutcome::failed(
                    format!("PostgreSQL setup incomplete: {}", problems.join("; ")),
                    log,
                ));
            }
            RunMode::Install => {}
        }

        // Binaries: download + verify + selective extraction.
        // Skipped when the binaries are in place and either the
        // marker or an initialized cluster proves the install
        // completed — re-extracting over an installed tree is what
        // locked-DLL failures come from (see the stop below).
        if self.binaries_present(ctx).await
            && (ctx
                .ops
                .file_exists(&postgres_marker_path(&ctx.data_dir))
                .await
                || self.cluster_initialized(ctx).await)
        {
            log.push("PostgreSQL binaries already installed".to_string());
        } else {
            let zip = postgres_zip_path(&ctx.data_dir, &pin);
            let zip_ok = ctx.ops.file_exists(&zip).await
                && matches!(ctx.ops.sha256_file(&zip).await, Ok(h) if checksums_match(&h, &pin.sha256));
            if !zip_ok {
                let url = pin.url();
                push_log(ctx, &mut log, format!("downloading {url}"));
                let progress = ProgressEmitter::new(ctx, pin.asset.clone());
                let dl_started = std::time::Instant::now();
                match ctx
                    .ops
                    .download(&url, &zip, &|d, t| progress.progress(d, t))
                    .await
                {
                    Ok(bytes) => push_log(
                        ctx,
                        &mut log,
                        format!("downloaded {bytes} bytes in {}", fmt_elapsed(dl_started)),
                    ),
                    Err(e) => {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                    }
                }
                let actual = match ctx.ops.sha256_file(&zip).await {
                    Ok(h) => h,
                    Err(e) => {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                    }
                };
                if !checksums_match(&actual, &pin.sha256) {
                    return Ok(StepOutcome::failed(
                        format!(
                            "checksum mismatch for {}: expected {}, got {}",
                            zip.display(),
                            pin.sha256,
                            actual
                        ),
                        vec!["distribution rejected; not installed".to_string()],
                    ));
                }
                log.push(format!("SHA-256 verified for {}", pin.asset));
            } else {
                log.push(format!(
                    "{} already present with matching checksum",
                    pin.asset
                ));
            }
            // A running server holds its DLLs open; extracting over
            // them fails with a sharing violation (os error 32).
            // pg_isready is the ground truth for "a server is up" —
            // stop it first; the flow below restarts it via
            // ensure_postgres_running.
            if postgres_ready(ctx.ops.as_ref(), &ctx.config, &ctx.data_dir).await {
                log.push(
                    "PostgreSQL is running; stopping it before replacing its binaries (it restarts below)"
                        .to_string(),
                );
                let stop = pg_ctl_command(&ctx.data_dir, &["stop", "-m", "fast"]);
                match ctx.ops.run_command(&stop).await {
                    Ok(out) if out.exit_code == 0 => {}
                    Ok(out) => {
                        return Ok(StepOutcome::failed(
                            format!(
                                "could not stop the running PostgreSQL server before replacing its binaries (pg_ctl exit {}): {} — stop the gateway and re-run this step",
                                out.exit_code,
                                out.stderr.trim()
                            ),
                            log,
                        ));
                    }
                    Err(e) => {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                    }
                }
            }
            push_log(
                ctx,
                &mut log,
                format!(
                    "extracting PostgreSQL binaries from {} to {}",
                    zip.display(),
                    postgres_root(&ctx.data_dir).display()
                ),
            );
            let extract_started = std::time::Instant::now();
            if let Err(e) = ctx
                .ops
                .extract_zip_prefixes(
                    &zip,
                    &postgres_root(&ctx.data_dir),
                    &["pgsql/bin/", "pgsql/lib/", "pgsql/share/", "pgsql/include/"],
                    "pgsql/",
                )
                .await
            {
                return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
            }
            push_log(
                ctx,
                &mut log,
                format!(
                    "extracted bin/lib/share/include to {} in {}",
                    postgres_root(&ctx.data_dir).display(),
                    fmt_elapsed(extract_started)
                ),
            );
            if !self.binaries_present(ctx).await {
                return Ok(StepOutcome::failed(
                    "extraction did not produce the pinned PostgreSQL binaries",
                    vec![
                        format!("expected pg_ctl.exe, initdb.exe, postgres.exe, psql.exe, createdb.exe under {}", postgres_bin_dir(&ctx.data_dir).display()),
                        "the distribution verified against its pin, so its layout differs from the pinned build's — failing closed".to_string(),
                    ],
                ));
            }
        }

        // The database password: generated once, stored in
        // Credential Manager, reused forever after (the cluster is
        // initialized with it; rotating it would lock the gateway
        // out of its own database).
        let password = match crate::ensure_postgres_password(ctx.ops.as_ref()).await {
            Ok(p) => p,
            Err(e) => {
                return Ok(StepOutcome::failed(
                    format!(
                    "could not store the PostgreSQL password in Windows Credential Manager: {e}"
                ),
                    vec![format!("{e:?}")],
                ))
            }
        };

        // Cluster initialization (once).
        if self.cluster_initialized(ctx).await {
            log.push("database cluster already initialized".to_string());
        } else {
            let pwfile = postgres_root(&ctx.data_dir).join("pwfile.tmp");
            if let Err(e) = ctx
                .ops
                .write_file(&pwfile, format!("{password}\n").as_bytes())
                .await
            {
                return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
            }
            // The password file holds the cluster superuser
            // password in plaintext; it must not outlive initdb.
            // The explicit removal below covers the normal paths —
            // this guard covers the step future being DROPPED
            // mid-initdb (app closed, step timeout), which skipped
            // the removal entirely (code-review 2.10). Synchronous
            // std::fs removal: Drop cannot await the ops trait.
            let _pwfile_guard = PwFileGuard(pwfile.clone());
            let initdb = self.initdb_command(ctx);
            let result = run_command_logged(ctx, &mut log, &initdb, None).await;
            // The password file is single-use: always remove it.
            let _ = ctx.ops.remove_file(&pwfile).await;
            match result {
                Ok(out) if out.exit_code == 0 => {
                    push_log(
                        ctx,
                        &mut log,
                        "database cluster initialized (superuser postgres, scram-sha-256, UTF8)"
                            .to_string(),
                    );
                }
                Ok(out) => {
                    return Ok(StepOutcome::failed(
                        format!(
                            "initdb failed (exit {}): {}",
                            out.exit_code,
                            out.stderr.trim()
                        ),
                        {
                            let mut l = log.clone();
                            l.push(out.stdout.clone());
                            l
                        },
                    ));
                }
                Err(e) => return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")])),
            }
        }

        // Start the server and create the gateway's database.
        if let Err(e) = ensure_postgres_running(ctx, &mut log).await {
            return Ok(StepOutcome::failed(e, log));
        }
        let mut createdb = self.createdb_command(ctx);
        createdb.env.insert("PGPASSWORD".to_string(), password);
        match run_command_logged(ctx, &mut log, &createdb, None).await {
            Ok(out) if out.exit_code == 0 => {
                push_log(
                    ctx,
                    &mut log,
                    format!("database {POSTGRES_DATABASE} created"),
                );
            }
            Ok(out) if out.stderr.contains("already exists") => {
                push_log(
                    ctx,
                    &mut log,
                    format!("database {POSTGRES_DATABASE} already exists"),
                );
            }
            Ok(out) => {
                return Ok(StepOutcome::failed(
                    format!(
                        "createdb failed (exit {}): {}",
                        out.exit_code,
                        out.stderr.trim()
                    ),
                    log,
                ));
            }
            Err(e) => return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")])),
        }

        let marker = serde_json::json!({
            "version": pin.version,
            "sha256": pin.sha256,
            "port": ctx.config.postgres.port,
            "database": POSTGRES_DATABASE,
            "at": chrono::Utc::now().to_rfc3339(),
        });
        if let Err(e) = ctx
            .ops
            .write_file(
                &postgres_marker_path(&ctx.data_dir),
                json_pretty(&marker).as_bytes(),
            )
            .await
        {
            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
        }
        Ok(StepOutcome::done(
            format!(
                "PostgreSQL {} running on 127.0.0.1:{} (database {POSTGRES_DATABASE})",
                pin.version, ctx.config.postgres.port
            ),
            log,
        ))
    }
}
