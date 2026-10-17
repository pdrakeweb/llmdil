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
//!
//! LOAD-BEARING CONFIG: `app.withGlobalTauri` in tauri.conf.json must stay
//! `true`. The UI invokes commands through the global
//! `window.__TAURI__.core.invoke` (see ui/src/api.ts) instead of the
//! `@tauri-apps/api` npm package, because npm registry access is
//! unavailable in the build environment. Setting it to `false` breaks
//! every command call from the UI.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;

use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager, State};

use manager_app::{registry::Op, AppState, EventSink};

/// Holds the running admin API server, if enabled, for the app's
/// lifetime (dropping it does not stop the server; the Phase-2
/// settings flow owns start/stop through this slot).
pub struct ApiServerSlot(pub std::sync::Mutex<Option<manager_api::ApiServer>>);

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
// here (and as an adapter above) without a second edit.
macro_rules! tauri_handler {
    ($(($variant:ident, $name:ident, $class:ident, $op:ident, [$($pname:ident : $pty:ty),*], $route:expr)),*) => {
        tauri::generate_handler![$($name),*]
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
            app.manage(ApiServerSlot(std::sync::Mutex::new(None)));
            // Admin REST API (phase 1 wiring): when enabled in the
            // config, serve on loopback with the self-signed
            // certificate. The LAN bind, firewall rule, and auth
            // arrive with the phase-2 enablement flow; until then
            // this is the loopback development surface.
            if admin_api.enabled {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let tls = match manager_api::tls::load_or_generate(&api_data_dir) {
                        Ok(m) => m,
                        Err(e) => {
                            eprintln!("admin api: TLS setup failed: {e}");
                            return;
                        }
                    };
                    match manager_api::ApiServer::start(
                        app_state,
                        api_data_dir,
                        manager_api::ApiOptions {
                            bind: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                            port: admin_api.port,
                            tls: Some(tls),
                        },
                    )
                    .await
                    {
                        Ok(server) => {
                            let slot = handle.state::<ApiServerSlot>();
                            *slot.inner().0.lock().unwrap() = Some(server);
                        }
                        Err(e) => eprintln!("admin api: failed to start: {e}"),
                    }
                });
            }
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
            Ok(())
        })
        .invoke_handler(manager_app::for_each_operation!(tauri_handler))
        .run(tauri::generate_context!())
        .expect("failed to run Local LLM Service Manager");
}
