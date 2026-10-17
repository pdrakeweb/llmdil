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

pub mod http;
pub mod tls;

pub use http::{build_router, RouteRecord, DERIVED_ROUTES};
pub use tls::TlsMaterial;

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
}

impl ApiCtx {
    pub fn new(app: Arc<AppState>, data_dir: PathBuf) -> std::io::Result<Arc<Self>> {
        let deployment_id = load_or_create_deployment_id(&data_dir)?;
        let events = Arc::new(BroadcastSink::new());
        let sink: Arc<dyn EventSink> = events.clone();
        Ok(Arc::new(Self {
            app,
            sink,
            events,
            deployment_id,
            data_dir,
        }))
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

/// A running API listener with an explicit shutdown.
pub struct ApiServer {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
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
        let ctx = ApiCtx::new(app, data_dir)?;
        let (router, _records) = build_router(ctx);
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
                    let _ = axum::serve(listener, router)
                        .with_graceful_shutdown(shutdown_fut)
                        .await;
                })
            }
            None => tokio::spawn(async move {
                let _ = axum::serve(tcp, router)
                    .with_graceful_shutdown(shutdown_fut)
                    .await;
            }),
        };
        Ok(Self {
            addr,
            shutdown: Some(tx),
            task,
        })
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
