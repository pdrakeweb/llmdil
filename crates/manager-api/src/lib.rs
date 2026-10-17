//! Admin REST API for the Local LLM Service Manager (LLMdil).
//!
//! Phase 1 surface: the router generated from manager-app's
//! operation registry, identity + read endpoints, the error
//! envelope, and a TLS (or loopback-plaintext dev) listener with an
//! explicit lifecycle. Authentication lands in phase 2 — until then
//! the only supported bind is loopback, enforced in [`ApiServer::start`]
//! for plaintext and by the caller's enablement flow for TLS.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{broadcast, oneshot};

use manager_app::{AppState, EventSink};

pub mod audit;
pub mod firewall;
pub mod auth_routes;
pub mod composites;
pub mod auth;
pub mod http;
pub mod tls;
pub mod users;

pub use audit::AuditLog;
pub use auth::{AuthService, CredentialStore, Principal};
pub use auth_routes::AUTH_ROUTES;
pub use http::{build_router, RouteRecord, DERIVED_ROUTES};
pub use tls::TlsMaterial;
pub use users::UserStore;

/// One event emitted by an operation, as published to API
/// subscribers (the phase-4 SSE stream consumes this channel).
#[derive(Debug, Clone)]
pub struct EventEnvelope {
    pub event: String,
    pub payload: Value,
}

/// The `EventSink` the API hands to operations: events go to an
/// in-process broadcast channel instead of the desktop webview.
#[derive(Debug, Clone)]
pub struct BroadcastSink {
    tx: broadcast::Sender<EventEnvelope>,
}

impl BroadcastSink {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self { tx }
    }

    /// Subscribe to the operation event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.tx.subscribe()
    }
}

impl Default for BroadcastSink {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSink for BroadcastSink {
    fn emit(&self, event: &str, payload: Value) {
        let _ = self.tx.send(EventEnvelope {
            event: event.to_string(),
            payload,
        });
    }
}

/// Shared state for every handler.
pub struct ApiCtx {
    pub app: Arc<AppState>,
    /// The sink handed to operations (trait-object view of `events`).
    pub sink: Arc<dyn EventSink>,
    /// The broadcast channel behind `sink` (SSE subscribes here).
    pub events: Arc<BroadcastSink>,
    pub deployment_id: String,
    pub data_dir: PathBuf,
    /// Users, sessions, and API tokens (`admin-users.json`).
    pub users: Arc<UserStore>,
    /// Authentication service over `users`.
    pub auth: Arc<AuthService>,
    /// The mutation/auth audit log.
    pub audit: Arc<AuditLog>,
    /// Serializes composite read-modify-write sequences (gateway
    /// group CRUD, gateway-config patch) so concurrent API writers
    /// cannot lose each other's updates; single operations are
    /// already serialized inside the app core.
    pub config_write_lock: tokio::sync::Mutex<()>,
}

impl ApiCtx {
    /// Build the context, loading the user store from the data dir
    /// and using the OS credential store for TOTP secrets.
    pub async fn new(app: Arc<AppState>, data_dir: PathBuf) -> std::io::Result<Arc<Self>> {
        let users = UserStore::load(&data_dir)
            .await
            .map_err(std::io::Error::other)?;
        let creds: Arc<dyn CredentialStore> = Arc::new(auth::WizardCredentialStore);
        Ok(Self::with_services(app, data_dir, users, creds))
    }

    /// Build the context from explicit services (tests inject
    /// in-memory services).
    pub fn with_services(
        app: Arc<AppState>,
        data_dir: PathBuf,
        users: Arc<UserStore>,
        creds: Arc<dyn CredentialStore>,
    ) -> Arc<Self> {
        let auth = AuthService::new(users, creds);
        Self::with_auth(app, data_dir, auth)
    }

    /// Build the context around a prepared auth service. The
    /// desktop shell uses this so its settings commands and the
    /// HTTP server share ONE auth service instance — enrollment
    /// codes, challenges, and lockout state live in memory per
    /// instance, so a second instance would silently split them.
    pub fn with_auth(app: Arc<AppState>, data_dir: PathBuf, auth: Arc<AuthService>) -> Arc<Self> {
        let users = auth.store().clone();
        let deployment_id = load_or_create_deployment_id(&data_dir)
            .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());
        let events = Arc::new(BroadcastSink::new());
        let sink: Arc<dyn EventSink> = events.clone();
        let audit = Arc::new(AuditLog::new(&data_dir));
        Arc::new(Self {
            app,
            sink,
            events,
            deployment_id,
            data_dir,
            users,
            auth,
            audit,
            config_write_lock: tokio::sync::Mutex::new(()),
        })
    }
}

/// The deployment's stable identity (auth plan: chosen at first
/// enable, shown in the desktop, matched by paired clients).
fn load_or_create_deployment_id(data_dir: &std::path::Path) -> std::io::Result<String> {
    let dir = data_dir.join("admin-api");
    let path = dir.join("deployment.id");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let id = existing.trim().to_string();
        if !id.is_empty() {
            return Ok(id);
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&path, &id)?;
    Ok(id)
}

/// How the listener should run.
#[derive(Debug, Clone)]
pub struct ApiOptions {
    pub bind: std::net::IpAddr,
    pub port: u16,
    /// TLS material; `None` is the plaintext loopback development
    /// opt-in (the bind is then forced to loopback).
    pub tls: Option<TlsMaterial>,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiStartError {
    #[error("plaintext API bind must be loopback (got {0}); use TLS for LAN binds")]
    PlaintextNotLoopback(std::net::IpAddr),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// The connect-info type for both listener kinds (a local wrapper:
/// `Connected` cannot be implemented for `SocketAddr` directly).
#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub SocketAddr);

impl axum::extract::connect_info::Connected<
    axum::serve::IncomingStream<'_, tokio::net::TcpListener>,
> for PeerAddr
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        PeerAddr(*stream.remote_addr())
    }
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, tls::TlsListener>>
    for PeerAddr
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, tls::TlsListener>) -> Self {
        PeerAddr(*stream.remote_addr())
    }
}

/// A running API listener with an explicit shutdown.
pub struct ApiServer {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
    ctx: Arc<ApiCtx>,
}

impl ApiServer {
    /// Bind and serve until [`ApiServer::shutdown`] is called.
    pub async fn start(
        app: Arc<AppState>,
        data_dir: PathBuf,
        options: ApiOptions,
    ) -> Result<Self, ApiStartError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        if options.tls.is_none() && !options.bind.is_loopback() {
            return Err(ApiStartError::PlaintextNotLoopback(options.bind));
        }
        let ctx = ApiCtx::new(app, data_dir).await?;
        Self::start_ctx(ctx, options).await
    }

    /// Start from a prepared context (shared user store with the
    /// desktop shell).
    pub async fn start_ctx(
        ctx: Arc<ApiCtx>,
        options: ApiOptions,
    ) -> Result<Self, ApiStartError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        if options.tls.is_none() && !options.bind.is_loopback() {
            return Err(ApiStartError::PlaintextNotLoopback(options.bind));
        }
        let (router, _records) = build_router(ctx.clone());
        let tcp =
            tokio::net::TcpListener::bind(SocketAddr::new(options.bind, options.port)).await?;
        let addr = tcp.local_addr()?;
        let (tx, rx) = oneshot::channel::<()>();
        let shutdown_fut = async move {
            let _ = rx.await;
        };
        let task = match &options.tls {
            Some(material) => {
                let config = tls::server_config(material)?;
                let listener = tls::TlsListener::new(tcp, config);
                tokio::spawn(async move {
                    let _ = axum::serve(
                        listener,
                        router.into_make_service_with_connect_info::<PeerAddr>(),
                    )
                    .with_graceful_shutdown(shutdown_fut)
                    .await;
                })
            }
            None => tokio::spawn(async move {
                let _ = axum::serve(
                    tcp,
                    router.into_make_service_with_connect_info::<PeerAddr>(),
                )
                .with_graceful_shutdown(shutdown_fut)
                .await;
            }),
        };
        Ok(Self {
            addr,
            shutdown: Some(tx),
            task,
            ctx,
        })
    }

    /// The server's context (the desktop shell reaches the shared
    /// auth service and user store through it).
    pub fn ctx(&self) -> &Arc<ApiCtx> {
        &self.ctx
    }

    /// The address the listener actually bound (port 0 resolves).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stop accepting and drain, then wait for the serve task.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = self.task.await;
    }
}
