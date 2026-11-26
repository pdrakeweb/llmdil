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
//!   set, the API is hosted in-process on the configured bind choice
//!   (`admin_api.bind_mode`: loopback, all interfaces — the default —
//!   or one interface address) with the scoped firewall rule
//!   reconciled on enable/disable when the bind needs one. The
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
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};

use manager_api::auth::{AuthService, CredentialStore, WizardCredentialStore};
use manager_api::users::RoleName;
use manager_app::{registry::Op, AppState, EventSink};

/// The running admin API server plus the last lifecycle note (e.g.
/// a firewall error) for the settings page.
pub struct ApiServerSlot {
    pub server: std::sync::Mutex<Option<manager_api::ApiServer>>,
    pub note: std::sync::Mutex<Option<String>>,
    /// Why the listener's certificate is the one it is (SEC-08):
    /// `CertSource::as_str()` plus the regeneration detail, if any.
    pub cert: std::sync::Mutex<Option<(&'static str, Option<String>)>>,
    /// What the running listener bound, and why when it fell back to
    /// loopback from the configured choice (auth-plan §5).
    pub bind: std::sync::Mutex<Option<manager_api::bind::BindPlan>>,
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

    fn toast(&self, title: &str, body: &str) -> Result<(), String> {
        use tauri_plugin_notification::NotificationExt;
        self.0
            .notification()
            .builder()
            .title(title)
            .body(body)
            .show()
            .map_err(|e| format!("toast send failed: {e}"))
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
            #[allow(clippy::too_many_arguments)] // argument list is the registry contract
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
            admin_api_create_enrollment,
            admin_api_regenerate_cert,
            admin_api_apply_firewall_rule,
            admin_api_get_audit
        ]
    };
}

/// Bring the main window to the front, hidden or minimized included.
fn show_main_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn main() {
    // Apply-helper mode (app self-update): a process spawned by the
    // Restart action with --llmdil-apply-update never starts the
    // shell — it waits for the app to exit, runs the staged
    // installer, records the outcome, and relaunches. Handled here,
    // before any Tauri state exists, so the helper holds no locks
    // the exiting app needs released.
    {
        let args: Vec<String> = std::env::args().collect();
        if manager_app::app_update::is_helper_invocation(&args) {
            std::process::exit(manager_app::app_update::run_apply_helper(&args));
        }
    }
    // One manager per data directory, decided before anything below
    // reads or writes a file there: a second launch shows the first
    // one's window and exits.
    let instance = match tauri::async_runtime::block_on(manager_app::single_instance::acquire(
        &manager_config::default_data_dir(),
    )) {
        manager_app::single_instance::Launch::First(guard) => guard,
        manager_app::single_instance::Launch::AlreadyRunning => return,
    };
    // The one logging pipeline (T-05): stderr plus the JSON Lines
    // `<data_dir>/logs/app.log` the diagnostics bundle carries.
    // Installed before anything logs; RUST_LOG overrides the level.
    manager_app::logging::init(&manager_config::default_data_dir());
    tauri::Builder::default()
        // Toast notifications (Settings → Notifications). The plugin
        // is driven from Rust via the EventSink toast capability —
        // the events that matter occur in background tasks, not the
        // webview.
        .plugin(tauri_plugin_notification::init())
        // Close-to-tray while the API serves (plan decision 13):
        // closing the window hides it and the API keeps serving;
        // Quit lives on the tray menu. With the API disabled the
        // window closes (and the app exits) as before.
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                let slot = app.state::<ApiServerSlot>();
                if slot
                    .inner()
                    .server
                    .lock()
                    .map(|s| s.is_some())
                    .unwrap_or(false)
                {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .setup(move |app| {
            // Load the persisted config (finding F2,
            // docs/code-review-v0.1.60.md). `load_for_startup` falls
            // back to the `.bak` last-known-good copy on parse or
            // validation failure (spec §10), and to built-in defaults
            // when both fail — and in both fallback cases it first copies
            // the unreadable file(s) to `*.corrupt-<unix-ms>`, because
            // startup itself saves (catalog seeding, setup
            // reconciliation) and every save rotates the primary over
            // `.bak`. The outcome is recorded on AppState so `get_status`
            // reports it to the UI (a log line alone lands only in
            // stderr and app.log, which the operator never sees in the
            // windowless release build).
            let data_dir = manager_config::default_data_dir();
            let config_path = data_dir.join("config.json");
            let startup = manager_config::load_for_startup(&config_path);
            if let Some(e) = &startup.error {
                tracing::warn!(
                    "config load fell back to {:?} ({e}): {}",
                    startup.source,
                    config_path.display()
                );
            }
            let config = startup.config;
            let admin_api = config.admin_api.clone();
            let api_data_dir = data_dir.clone();
            let app_state = Arc::new(AppState::new(config, config_path, data_dir));
            app_state.set_config_load_state(startup.source, startup.error, &startup.preserved);
            app.manage(app_state.clone());
            app.manage(ApiServerSlot {
                server: std::sync::Mutex::new(None),
                note: std::sync::Mutex::new(None),
                cert: std::sync::Mutex::new(None),
                bind: std::sync::Mutex::new(None),
            });
            // A second launch shows this window (hidden by close-to-tray,
            // minimized or covered), unless this process is exiting.
            let handle = app.handle().clone();
            let exiting = app_state.clone();
            tauri::async_runtime::spawn(instance.serve(
                move || !exiting.lifecycle().app_exiting(),
                move || show_main_window(&handle),
            ));
            // The API user store + auth service, shared between the
            // HTTP server and this shell's admin commands.
            let users =
                match tauri::async_runtime::block_on(manager_api::UserStore::load(&api_data_dir)) {
                    Ok(store) => store,
                    Err(e) => {
                        tracing::warn!(
                            "admin api: user store failed to load ({e}); using an empty store"
                        );
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
                // Heal the launch-routing flag for installs that
                // completed setup without the Finish button (see
                // AppState::reconcile_setup_completed).
                state.reconcile_setup_completed().await;
            });
            // Optimize experiment startup sweep (Phase 5e):
            // terminate leftover temp backends from an app crash
            // mid-experiment and clear their journal records.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<Arc<AppState>>().inner().clone();
                manager_app::optimize_experiments::startup_sweep(&state).await;
            });
            // Bench startup sweep: the same treatment for
            // leftover bench-<n> temp backends and for sweep
            // records a crash left marked running.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<Arc<AppState>>().inner().clone();
                manager_app::bench::startup_sweep(&state).await;
            });
            // Tiered model storage (docs/tiered-model-storage.md):
            // crash recovery (interrupted moves, leftover .partial
            // copies), then the reconcile loop that runs at most one
            // background-priority copy at a time, parked while any
            // backend is loading.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<Arc<AppState>>().inner().clone();
                // Not dispatched through the registry, so its events
                // are offered to the log shipper explicitly.
                let sink: Arc<dyn EventSink> =
                    manager_app::logship::tap_sink(&state, Arc::new(TauriSink(handle)));
                manager_app::model_storage::startup(&state).await;
                loop {
                    manager_app::model_storage::tick(&state, &sink).await;
                    tokio::time::sleep(std::time::Duration::from_secs(
                        manager_app::model_storage::TICK_SECS,
                    ))
                    .await;
                }
            });
            // Lifecycle monitor: first reconcile the registry with
            // what is already running (adopt orphaned backends, seed
            // intent, write "observed running at app start" markers),
            // then every 3 s detect crashes of every managed service
            // against operator intent and fire the toasts (see
            // manager_app::lifecycle_monitor and notify::monitor_tick).
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<Arc<AppState>>().inner().clone();
                // Not dispatched through the registry, so its events
                // are offered to the log shipper explicitly.
                let sink: Arc<dyn EventSink> =
                    manager_app::logship::tap_sink(&state, Arc::new(TauriSink(handle)));
                state.set_desktop_sink(Arc::clone(&sink));
                manager_app::lifecycle::startup_reconcile(&state, sink.as_ref()).await;
                loop {
                    manager_app::notify::monitor_tick(&state, sink.as_ref()).await;
                    manager_app::lifecycle_monitor::run_due_restarts(&state, &sink).await;
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
            });
            // Gateway metrics poller: scrape LiteLLM's Prometheus
            // endpoint every 15 s into the stats store that feeds
            // the Dashboard's request/error figures and live
            // throughput rate. The same loop also polls each
            // backend's llama-server `/slots` directly: the
            // gateway counters move only on completion, so a
            // backend mid-generation needs its own live rate
            // (Nov 3, 2026 throughput fix). Cycles skip silently
            // while the gateway (or a backend's `/slots`) is down.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<Arc<AppState>>().inner().clone();
                loop {
                    manager_app::poll_gateway_metrics_once(&state).await;
                    manager_app::poll_direct_slots_once(&state).await;
                    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                }
            });
            // GPU history recorder: sample the telemetry snapshot
            // every 15 s into the persistent per-GPU history that
            // backs the GPUs page history charts (temperature,
            // utilization, throughput, power over 1 h / 24 h / 7 d).
            // Best-effort: a skipped tick is a chart gap, never an
            // error surface.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<Arc<AppState>>().inner().clone();
                loop {
                    manager_app::gpu_history::record_gpu_history_once(&state).await;
                    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                }
            });
            // App self-update checker: one check shortly after
            // startup, then every CHECK_INTERVAL (6 h), each gated
            // on the operator's update-checking switch at the time.
            // Findings land in the app-update state file; the shell
            // banner and Settings derive from it.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<Arc<AppState>>().inner().clone();
                // Not dispatched through the registry, so its events
                // are offered to the log shipper explicitly.
                let sink: Arc<dyn EventSink> =
                    manager_app::logship::tap_sink(&state, Arc::new(TauriSink(handle)));
                manager_app::app_update::checker_loop(state, sink).await;
            });
            // Log streaming shipper (log-streaming design): starts
            // only when the deployment opted in via the
            // `log_streaming` config section; a no-op otherwise.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<Arc<AppState>>().inner().clone();
                manager_app::logship::start_log_streaming(&state).await;
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
                        &admin_api,
                        slot.inner(),
                    )
                    .await;
                    match note {
                        Ok(server) => {
                            *slot.inner().server.lock().unwrap() = Some(server);
                        }
                        Err(e) => {
                            tracing::error!("admin api: failed to start: {e}");
                            *slot.inner().note.lock().unwrap() = Some(e);
                        }
                    }
                });
            }
            // System tray: the way back in (and the way out) when
            // the window is hidden by close-to-tray.
            {
                let show = MenuItem::with_id(app, "tray-show", "Show LLMdil", true, None::<&str>);
                let quit = MenuItem::with_id(app, "tray-quit", "Quit", true, None::<&str>);
                if let (Ok(show), Ok(quit)) = (show, quit) {
                    if let Ok(menu) = Menu::with_items(app, &[&show, &quit]) {
                        let mut tray = TrayIconBuilder::with_id("main-tray")
                            .menu(&menu)
                            .tooltip("LLMdil")
                            .on_menu_event(|app, event| match event.id().as_ref() {
                                "tray-show" => show_main_window(app),
                                "tray-quit" => app.exit(0),
                                _ => {}
                            });
                        if let Some(icon) = app.handle().default_window_icon() {
                            tray = tray.icon(icon.clone());
                        }
                        if let Err(e) = tray.build(app) {
                            tracing::warn!("tray icon failed: {e}");
                        }
                    }
                }
            }
            Ok(())
        })
        .invoke_handler(manager_app::for_each_operation!(tauri_handler))
        .build(tauri::generate_context!())
        .expect("failed to build Local LLM Service Manager")
        .run(|app, event| {
            // App exit (tray Quit, last window closed): the managed
            // services are left running on purpose; record that in
            // each one's own log, release the supervised backends and
            // stop the log shipper (OS-9).
            if let tauri::RunEvent::Exit = event {
                if let Some(state) = app.try_state::<Arc<AppState>>() {
                    let state = state.inner().clone();
                    tauri::async_runtime::block_on(async move {
                        manager_app::lifecycle::on_app_exit(&state, "exit").await;
                    });
                }
            }
        });
}

/// Start the API with TLS on the configured bind choice (auth-plan
/// §5: loopback, all interfaces, or one interface address — a
/// missing interface address falls back to loopback, never wider)
/// and reconcile the scoped firewall rule when the bind needs one.
/// Shared by app startup and the settings page's enable action. The
/// stored certificate is reused (SEC-08); why it is the one in use is
/// recorded in `slot.cert`, and the effective bind in `slot.bind`.
async fn start_admin_api(
    app_state: Arc<AppState>,
    data_dir: std::path::PathBuf,
    auth: Arc<AuthService>,
    api: &manager_config::AdminApiConfig,
    slot: &ApiServerSlot,
) -> Result<manager_api::ApiServer, String> {
    let port = api.port;
    let load = manager_api::tls::ensure_cert(&data_dir, &cert_sans())
        .map_err(|e| format!("TLS setup failed: {e}"))?;
    *slot.cert.lock().unwrap() = Some((load.source.as_str(), load.detail.clone()));
    let tls = load.material;
    let plan = manager_api::bind::resolve_now(api);
    if let Some(why) = &plan.fallback {
        eprintln!("admin api: {why}");
    }
    let ctx = manager_api::ApiCtx::with_auth(app_state, data_dir, auth);
    let server = manager_api::ApiServer::start_ctx(
        ctx,
        manager_api::ApiOptions {
            bind: plan.addr,
            port,
            tls: Some(tls),
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    let rule_needed = manager_api::firewall::rule_needed(plan.addr);
    *slot.bind.lock().unwrap() = Some(plan);
    if rule_needed {
        if let Err(e) = manager_api::firewall::ensure(port).await {
            // The API still serves on the LAN interfaces the host
            // firewall already permits; the settings page shows the
            // note and offers the repair by toggling.
            tracing::warn!("admin api: firewall rule not applied: {e}");
        }
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

/// The machine's Tailscale IPv4, when the Tailscale CLI answers. The
/// CLI waits for its service, so it gets two seconds: a stuck one must
/// not keep the API from starting.
fn tailscale_ip() -> Option<std::net::IpAddr> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(
            std::process::Command::new("tailscale")
                .args(["ip", "-4"])
                .output(),
        );
    });
    let out = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().next()?.trim().parse().ok()
}

/// The current `admin_api` settings, read through the app's own
/// config operation (bind choice included); a missing or unreadable
/// section is the defaults.
async fn admin_api_config(
    st: &Arc<AppState>,
    app: &AppHandle,
) -> Result<manager_config::AdminApiConfig, String> {
    let sink: Arc<dyn EventSink> = Arc::new(TauriSink(app.clone()));
    let cfg = manager_app::registry::dispatch(st, &sink, Op::GetConfig, json!({}))
        .await
        .map_err(|e| e.to_string())?;
    Ok(cfg
        .get("admin_api")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default())
}

/// The bind the listener has (while running) or would get if started
/// now from `api` (auth-plan §5 resolution, loopback fallback).
fn effective_bind(
    slot: &ApiServerSlot,
    api: &manager_config::AdminApiConfig,
) -> manager_api::bind::BindPlan {
    let running = slot.server.lock().unwrap().is_some();
    let current = slot.bind.lock().unwrap().clone();
    match current {
        Some(plan) if running => plan,
        _ => manager_api::bind::resolve_now(api),
    }
}

/// The base URL clients pair against for the effective bind.
fn api_base_url(slot: &ApiServerSlot, api: &manager_config::AdminApiConfig) -> String {
    let bind = effective_bind(slot, api).addr;
    let lan = if bind.is_unspecified() {
        primary_lan_ip()
    } else {
        None
    };
    manager_api::bind::base_url(bind, api.port, lan)
}

/// Status for the Remote Access settings card.
#[tauri::command]
async fn admin_api_get_status(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    slot: State<'_, ApiServerSlot>,
    auth: State<'_, AdminAuth>,
) -> Result<Value, String> {
    let api = admin_api_config(state.inner(), &app).await?;
    let (enabled, port) = (api.enabled, api.port);
    let data_dir = state.inner().data_dir().to_path_buf();
    let running = slot.inner().server.lock().unwrap().is_some();
    let note = slot.inner().note.lock().unwrap().clone();
    // Bind choice (auth-plan §5): what is bound (or would be, if
    // started now), the loopback-fallback reason when the configured
    // interface address is absent, and the addresses the operator can
    // pick from.
    let bind = effective_bind(slot.inner(), &api);
    let bind_candidates: Vec<String> =
        tauri::async_runtime::spawn_blocking(manager_api::bind::interface_addresses)
            .await
            .map_err(|e| e.to_string())?
            .iter()
            .map(|ip| ip.to_string())
            .collect();
    let firewall_needed = manager_api::firewall::rule_needed(bind.addr);
    let (users, admins) = auth
        .inner()
        .0
        .store()
        .read(|f| {
            (
                f.users.len(),
                f.users
                    .iter()
                    .filter(|u| u.role == RoleName::Admin && !u.disabled)
                    .count(),
            )
        })
        .await;
    let firewall = match manager_api::firewall::status(port).await {
        manager_api::firewall::FirewallStatus::Ok => "ok",
        manager_api::firewall::FirewallStatus::Missing => "missing",
        manager_api::firewall::FirewallStatus::Drifted => "drifted",
        manager_api::firewall::FirewallStatus::Unsupported => "unsupported",
        manager_api::firewall::FirewallStatus::Error(_) => "error",
    };
    // SEC-08: the stored certificate is reused across starts; report
    // why it is the one in use and (informational, non-blocking) which
    // current addresses it does not list.
    let (cert_source, cert_detail) = slot
        .inner()
        .cert
        .lock()
        .unwrap()
        .clone()
        .map(|(s, d)| (Some(s), d))
        .unwrap_or((None, None));
    let cert_not_after_ms = manager_api::tls::stored_cert(&data_dir).map(|c| c.not_after_ms);
    let san_drift: Vec<String> = if cert_not_after_ms.is_some() {
        let current = tauri::async_runtime::spawn_blocking(cert_sans)
            .await
            .map_err(|e| e.to_string())?;
        manager_api::tls::san_drift(&data_dir, &current)
            .iter()
            .map(|ip| ip.to_string())
            .collect()
    } else {
        Vec::new()
    };
    Ok(json!({
        "enabled": enabled,
        "running": running,
        "port": port,
        "bind": bind.addr.to_string(),
        "bind_mode": api.bind_mode,
        "bind_ip": api.bind_ip,
        "bind_fallback": bind.fallback,
        "bind_candidates": bind_candidates,
        "firewall_needed": firewall_needed,
        "base_url": if running { Value::String(api_base_url(slot.inner(), &api)) } else { Value::Null },
        "cert_sha256": manager_api::tls::stored_fingerprint(&data_dir),
        "cert_source": cert_source,
        "cert_detail": cert_detail,
        "cert_not_after_ms": cert_not_after_ms,
        // SEC-N3: where the private key lives ("credential_manager" |
        // "file"; null before any certificate exists).
        "key_storage": manager_api::tls::key_storage(&data_dir).map(|k| k.as_str()),
        "san_drift": !san_drift.is_empty(),
        "san_drift_ips": san_drift,
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
    let outcome: Result<(), String> = async {
        let mut api = admin_api_config(&st, &app).await?;
        let port = api.port;
        let sink: Arc<dyn EventSink> = Arc::new(TauriSink(app.clone()));
        manager_app::registry::dispatch(
            &st,
            &sink,
            Op::UpdateConfig,
            json!({ "patch": { "admin_api": { "enabled": enabled, "port": port } } }),
        )
        .await
        .map_err(|e| e.to_string())?;
        api.enabled = enabled;
        if enabled {
            let data_dir = st.data_dir().to_path_buf();
            match start_admin_api(
                st.clone(),
                data_dir,
                auth.inner().0.clone(),
                &api,
                slot.inner(),
            )
            .await
            {
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
            let removed = manager_api::firewall::remove(port).await;
            // SEC-13: the rule removal is its own audited change.
            manager_api::audit::record_local(st.data_dir(), "firewall_rule_remove", None, &removed);
            if let Err(e) = removed {
                *slot.inner().note.lock().unwrap() =
                    Some(format!("firewall rule removal failed: {e}"));
            }
        }
        Ok(())
    }
    .await;
    let action = if enabled {
        "admin_api_enable"
    } else {
        "admin_api_disable"
    };
    manager_api::audit::record_local(st.data_dir(), action, None, &outcome);
    outcome?;
    admin_api_get_status(app, state, slot, auth).await
}

#[tauri::command]
async fn admin_api_list_users(auth: State<'_, AdminAuth>) -> Result<Value, String> {
    let users = auth
        .inner()
        .0
        .store()
        .read(|f| {
            f.users
                .iter()
                .map(|u| {
                    serde_json::to_value(manager_api::auth::UserView::from(u))
                        .unwrap_or(Value::Null)
                })
                .collect::<Vec<_>>()
        })
        .await;
    Ok(json!({ "users": users }))
}

#[tauri::command]
async fn admin_api_create_user(
    state: State<'_, Arc<AppState>>,
    auth: State<'_, AdminAuth>,
    username: String,
    password: String,
    role: String,
) -> Result<Value, String> {
    let r: Result<Value, String> = async {
        let role = RoleName::parse(&role).ok_or_else(|| format!("unknown role `{role}`"))?;
        let view = auth
            .inner()
            .0
            .create_user(&username, &password, role)
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_value(view).map_err(|e| e.to_string())
    }
    .await;
    // SEC-13: same action string as the API route; never the password.
    let data_dir = state.inner().data_dir();
    manager_api::audit::record_local(data_dir, "POST /api/v1/admin/users", Some(&username), &r);
    r
}

#[tauri::command]
async fn admin_api_update_user(
    state: State<'_, Arc<AppState>>,
    auth: State<'_, AdminAuth>,
    id: String,
    role: Option<String>,
    disabled: Option<bool>,
) -> Result<Value, String> {
    let r: Result<Value, String> = async {
        let role = match role {
            Some(r) => Some(RoleName::parse(&r).ok_or_else(|| format!("unknown role `{r}`"))?),
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
    .await;
    let data_dir = state.inner().data_dir();
    manager_api::audit::record_local(data_dir, "PATCH /api/v1/admin/users/{id}", Some(&id), &r);
    r
}

#[tauri::command]
async fn admin_api_delete_user(
    state: State<'_, Arc<AppState>>,
    auth: State<'_, AdminAuth>,
    id: String,
) -> Result<Value, String> {
    // The desktop operator is the local administrator; the actor id
    // is a sentinel that is never a user id, so the self-delete
    // guard cannot trip here — the last-admin guard still applies.
    let r = auth
        .inner()
        .0
        .delete_user("desktop", &id)
        .await
        .map_err(|e| e.to_string());
    let data_dir = state.inner().data_dir();
    manager_api::audit::record_local(data_dir, "DELETE /api/v1/admin/users/{id}", Some(&id), &r);
    r?;
    Ok(json!({ "ok": true }))
}

#[tauri::command]
async fn admin_api_reset_password(
    state: State<'_, Arc<AppState>>,
    auth: State<'_, AdminAuth>,
    id: String,
) -> Result<Value, String> {
    let r = auth
        .inner()
        .0
        .admin_reset_password(&id)
        .await
        .map_err(|e| e.to_string());
    // Records only the outcome: the temporary password never reaches the log.
    let data_dir = state.inner().data_dir();
    let action = "POST /api/v1/admin/users/{id}/reset-password";
    manager_api::audit::record_local(data_dir, action, Some(&id), &r);
    Ok(json!({ "temporary_password": r? }))
}

/// The pairing payload for the enrollment QR (without a code):
/// base URL + certificate fingerprint.
#[tauri::command]
async fn admin_api_get_pairing(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    slot: State<'_, ApiServerSlot>,
) -> Result<Value, String> {
    let api = admin_api_config(state.inner(), &app).await?;
    let data_dir = state.inner().data_dir().to_path_buf();
    let base_url = api_base_url(slot.inner(), &api);
    let cert = manager_api::tls::stored_fingerprint(&data_dir);
    // The QR encodes exactly the two values returned beside it.
    let qr_svg = manager_api::pairing::pairing_qr_svg(&base_url, cert.as_deref(), None)
        .ok()
        .flatten();
    Ok(json!({
        "base_url": base_url,
        "cert_sha256": cert,
        "qr_svg": qr_svg,
    }))
}

/// Create a one-time enrollment code for a user and return the full
/// pairing payload the QR encodes.
#[tauri::command]
async fn admin_api_create_enrollment(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    slot: State<'_, ApiServerSlot>,
    auth: State<'_, AdminAuth>,
    user_id: String,
) -> Result<Value, String> {
    let api = admin_api_config(state.inner(), &app).await?;
    let data_dir = state.inner().data_dir().to_path_buf();
    let code = auth
        .inner()
        .0
        .create_enrollment_code(&user_id)
        .await
        .map_err(|e| e.to_string());
    // SEC-13: the outcome only, never the code.
    let action = "POST /api/v1/admin/enrollment-codes";
    manager_api::audit::record_local(&data_dir, action, Some(&user_id), &code);
    let code = code?;
    let base_url = api_base_url(slot.inner(), &api);
    let cert = manager_api::tls::stored_fingerprint(&data_dir);
    // The QR (PLAN.md §3.8) encodes exactly the three values returned
    // beside it — the ones the card already shows as text. A render
    // failure leaves the text values, which pair just the same.
    let qr_svg = manager_api::pairing::pairing_qr_svg(&base_url, cert.as_deref(), Some(&code))
        .ok()
        .flatten();
    Ok(json!({
        "base_url": base_url,
        "cert_sha256": cert,
        "enrollment_code": code,
        "qr_svg": qr_svg,
    }))
}

/// Rotate the API's TLS certificate: generate a fresh one and,
/// when the server is running, restart it onto the new
/// certificate. Paired clients must re-pair (the fingerprint
/// changes) — the settings page confirms before calling this.
#[tauri::command]
async fn admin_api_regenerate_cert(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    slot: State<'_, ApiServerSlot>,
    auth: State<'_, AdminAuth>,
) -> Result<Value, String> {
    let st = state.inner().clone();
    let data_dir = st.data_dir().to_path_buf();
    let outcome: Result<(), String> = async {
        let api = admin_api_config(&st, &app).await?;
        // The only path that changes the key or the SANs (SEC-08): the
        // current addresses are taken here, at the operator's request.
        manager_api::tls::regenerate(&data_dir, &cert_sans())
            .map_err(|e| format!("certificate regeneration failed: {e}"))?;
        *slot.inner().cert.lock().unwrap() = Some((
            manager_api::tls::CertSource::RegeneratedByOperator.as_str(),
            None,
        ));
        let running = slot.inner().server.lock().unwrap().is_some();
        if running {
            let old = slot.inner().server.lock().unwrap().take();
            if let Some(old) = old {
                old.shutdown().await;
            }
            let restarted = start_admin_api(
                st.clone(),
                data_dir.clone(),
                auth.inner().0.clone(),
                &api,
                slot.inner(),
            )
            .await;
            match restarted {
                Ok(server) => {
                    *slot.inner().server.lock().unwrap() = Some(server);
                    // The restart loaded the material regenerated above;
                    // keep reporting why it changed.
                    *slot.inner().cert.lock().unwrap() = Some((
                        manager_api::tls::CertSource::RegeneratedByOperator.as_str(),
                        None,
                    ));
                }
                Err(e) => {
                    *slot.inner().note.lock().unwrap() = Some(e.clone());
                    return Err(e);
                }
            }
        }
        Ok(())
    }
    .await;
    // SEC-13: no key material, only the outcome.
    manager_api::audit::record_local(&data_dir, "tls_cert_regenerate", None, &outcome);
    outcome?;
    admin_api_get_status(app, state, slot, auth).await
}

/// Apply the Admin API's firewall rule on demand: create or repair
/// the expected inbound rule (one elevation prompt when a change is
/// actually needed; a no-op when the rule already matches). The
/// settings page offers this button only when the rule is missing
/// or differs from expected; when the rule is already correct the
/// button is hidden, so the no-op path never prompts.
#[tauri::command]
async fn admin_api_apply_firewall_rule(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    slot: State<'_, ApiServerSlot>,
    auth: State<'_, AdminAuth>,
) -> Result<Value, String> {
    let outcome: Result<(), String> = async {
        let api = admin_api_config(state.inner(), &app).await?;
        // Consistent with the bind choice: a loopback-only listener needs
        // no inbound rule, so none is created for it.
        if !manager_api::firewall::rule_needed(effective_bind(slot.inner(), &api).addr) {
            return Err(
                "no firewall rule is needed: the Admin API listens on loopback only".to_string(),
            );
        }
        manager_api::firewall::ensure(api.port)
            .await
            .map_err(|e| format!("firewall rule failed: {e}"))
    }
    .await;
    let data_dir = state.inner().data_dir();
    manager_api::audit::record_local(data_dir, "firewall_rule_apply", None, &outcome);
    outcome?;
    admin_api_get_status(app, state, slot, auth).await
}

/// The newest audit-log entries for the settings page's review
/// section (newest first).
#[tauri::command]
async fn admin_api_get_audit(
    state: State<'_, Arc<AppState>>,
    limit: Option<usize>,
) -> Result<Value, String> {
    let data_dir = state.inner().data_dir().to_path_buf();
    let audit = manager_api::AuditLog::shared(&data_dir);
    let entries = audit.read_recent(0, limit.unwrap_or(20).min(200));
    Ok(json!({ "entries": entries }))
}
