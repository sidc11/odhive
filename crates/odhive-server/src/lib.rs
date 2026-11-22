//! OdHive HTTP server: axum app with the security layer and the embedded web UI.
#![forbid(unsafe_code)]

pub mod api;
mod assets;
pub mod auth;
pub mod jobs;
mod listen;
pub mod security;
pub mod service;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware;
use odhive_core::ports::LoopbackListeners;
use tracing::info;

pub use api::ws::WsLimits;
pub use auth::Auth;
pub use listen::ConnectionLimits;
pub use security::RequestLimits;
pub use service::{Service, ServiceOptions};

/// Runtime configuration for the HTTP app.
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// The port the UI is served on; only `Host` headers with this port are accepted.
    pub port: u16,
    /// Body size and time limit of each request.
    pub limits: RequestLimits,
}

impl AppConfig {
    /// The app serving on `port`, with default limits.
    #[must_use]
    pub fn new(port: u16) -> Self {
        Self {
            port,
            limits: RequestLimits::default(),
        }
    }
}

/// Limits of a running server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    /// Per request.
    pub requests: RequestLimits,
    /// Per server.
    pub connections: ConnectionLimits,
}

/// Builds the router (API and web UI) with every security middleware applied.
pub fn app(config: AppConfig, service: Service) -> Router {
    let host_guard = security::HostGuard::new(config.port);
    let login = axum::Router::new()
        .route(auth::LOGIN_PATH, axum::routing::get(auth::login))
        .with_state(service.auth().clone());
    api::router(service)
        .merge(login)
        .fallback(assets::serve)
        // Cuts bodies without a length when an extractor reads them.
        .layer(DefaultBodyLimit::max(config.limits.max_body))
        .layer(middleware::from_fn_with_state(
            config.limits,
            security::limit_requests,
        ))
        .layer(middleware::from_fn_with_state(
            host_guard,
            security::require_same_origin,
        ))
        .layer(middleware::from_fn(security::security_headers))
        .layer(middleware::from_fn_with_state(
            host_guard,
            security::require_allowed_host,
        ))
}

/// Serves the app on the given loopback listeners until `shutdown` resolves,
/// with default [`Limits`].
///
/// # Errors
/// Never at the moment; kept for listener failures to come.
pub async fn serve(
    listeners: LoopbackListeners,
    service: Service,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    serve_with(listeners, service, Limits::default(), shutdown).await
}

/// Like [`serve`] with other limits.
///
/// # Errors
/// Never at the moment; kept for listener failures to come.
pub async fn serve_with(
    listeners: LoopbackListeners,
    service: Service,
    limits: Limits,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let router = app(
        AppConfig {
            port: listeners.port,
            limits: limits.requests,
        },
        service.clone(),
    );
    info!("OdHive is running at {}", listeners.url());

    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown.await;
        // WebSockets are not connections of the accept loop any more: close them.
        service.begin_shutdown();
        let _ = tx.send(true);
    });

    let slots = listen::Slots::new(limits.connections);
    let v4 = listen::run(
        listeners.v4,
        router.clone(),
        limits.connections,
        slots.clone(),
        rx.clone(),
    );
    match listeners.v6 {
        Some(v6) => {
            let v6 = listen::run(v6, router, limits.connections, slots.clone(), rx);
            tokio::join!(v4, v6);
        }
        None => v4.await,
    }
    slots.drain(limits.connections.shutdown_grace).await;
    Ok(())
}
