//! OdHive HTTP server: axum app with the security layer and the embedded web UI.
#![forbid(unsafe_code)]

pub mod api;
mod assets;
pub mod jobs;
pub mod security;
pub mod service;

use axum::Router;
use axum::middleware;
use odhive_core::ports::LoopbackListeners;
use tracing::info;

pub use api::ws::WsLimits;
pub use service::{Service, ServiceOptions};

/// Runtime configuration for the HTTP app.
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// The port the UI is served on; only `Host` headers with this port are accepted.
    pub port: u16,
}

/// Builds the router (API and web UI) with every security middleware applied.
pub fn app(config: AppConfig, service: Service) -> Router {
    let host_guard = security::HostGuard::new(config.port);
    api::router(service)
        .fallback(assets::serve)
        .layer(middleware::from_fn_with_state(
            host_guard,
            security::require_same_origin_upgrade,
        ))
        .layer(middleware::from_fn(security::security_headers))
        .layer(middleware::from_fn_with_state(
            host_guard,
            security::require_allowed_host,
        ))
}

/// Serves the app on the given loopback listeners until `shutdown` resolves.
///
/// # Errors
/// Returns an error if a listener fails while serving.
pub async fn serve(
    listeners: LoopbackListeners,
    service: Service,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let router = app(
        AppConfig {
            port: listeners.port,
        },
        service.clone(),
    );
    info!("OdHive is running at {}", listeners.url());

    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown.await;
        // WebSockets are not tracked by the graceful shutdown: close them.
        service.begin_shutdown();
        let _ = tx.send(true);
    });
    let wait = |mut rx: tokio::sync::watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|stop| *stop).await;
    };

    let v4 = axum::serve(listeners.v4, router.clone()).with_graceful_shutdown(wait(rx.clone()));
    match listeners.v6 {
        Some(v6) => {
            let v6 = axum::serve(v6, router).with_graceful_shutdown(wait(rx));
            tokio::try_join!(v4.into_future(), v6.into_future())?;
        }
        None => v4.await?,
    }
    Ok(())
}
