//! The accept loop: HTTP/1.1 connections with a cap on how many are open at
//! once and a deadline for each request's headers (slow clients cannot hold
//! connections open forever), plus graceful shutdown.

use std::io;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};
use tower::ServiceExt as _;
use tracing::{debug, warn};

/// Limits of the connections themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionLimits {
    /// Open HTTP connections across all listeners. While all are taken, new
    /// connections wait in the listen backlog until one closes. An open
    /// WebSocket no longer counts here (see `WsLimits::max_connections`).
    pub max_connections: usize,
    /// Time a client has to send a request's headers, also on a kept-alive
    /// connection; then the connection is closed.
    pub header_read_timeout: Duration,
    /// How long shutdown waits for open connections to finish.
    pub shutdown_grace: Duration,
}

impl Default for ConnectionLimits {
    fn default() -> Self {
        Self {
            max_connections: 256,
            header_read_timeout: Duration::from_secs(10),
            shutdown_grace: Duration::from_secs(10),
        }
    }
}

/// Open connections across the listeners of one server.
#[derive(Debug, Clone)]
pub(crate) struct Slots {
    sem: Arc<Semaphore>,
    max: u32,
}

impl Slots {
    pub(crate) fn new(limits: ConnectionLimits) -> Self {
        let max = u32::try_from(limits.max_connections.max(1)).unwrap_or(u32::MAX);
        Self {
            sem: Arc::new(Semaphore::new(max as usize)),
            max,
        }
    }

    /// Waits until every connection has ended, at most `grace`.
    pub(crate) async fn drain(&self, grace: Duration) {
        if tokio::time::timeout(grace, self.sem.acquire_many(self.max))
            .await
            .is_err()
        {
            warn!("connections still open after {} s", grace.as_secs());
        }
    }
}

/// Accepts connections on `listener` and serves `router` on them until
/// `stop` turns `true`; then open connections are asked to finish.
pub(crate) async fn run(
    listener: TcpListener,
    router: Router,
    limits: ConnectionLimits,
    slots: Slots,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        let permit = tokio::select! {
            p = Arc::clone(&slots.sem).acquire_owned() => match p {
                Ok(p) => p,
                Err(_) => return,
            },
            () = stopped(&mut stop) => return,
        };
        let stream = tokio::select! {
            r = listener.accept() => match r {
                Ok((stream, _)) => stream,
                Err(e) => {
                    accept_error(&e).await;
                    continue;
                }
            },
            () = stopped(&mut stop) => return,
        };
        if let Err(e) = stream.set_nodelay(true) {
            debug!("set_nodelay: {e}");
        }
        let service = TowerToHyperService::new(
            router
                .clone()
                .map_request(|req: Request<Incoming>| req.map(Body::new)),
        );
        let mut stop = stop.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let mut builder = http1::Builder::new();
            builder
                .timer(TokioTimer::new())
                .header_read_timeout(limits.header_read_timeout);
            let mut conn = pin!(
                builder
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
            );
            tokio::select! {
                r = conn.as_mut() => {
                    if let Err(e) = r {
                        debug!("connection: {e}");
                    }
                    return;
                }
                () = stopped(&mut stop) => conn.as_mut().graceful_shutdown(),
            }
            if let Err(e) = conn.await {
                debug!("connection: {e}");
            }
        });
    }
}

/// Resolves once `stop` is `true` (or its sender is gone).
async fn stopped(stop: &mut watch::Receiver<bool>) {
    // The guard is dropped here, so the future stays `Send`.
    let _ = stop.wait_for(|s| *s).await.map(|_| ());
}

/// Connection-level errors only concern that one connection; others (like
/// running out of file descriptors) get a pause before the next try.
async fn accept_error(e: &io::Error) {
    use io::ErrorKind::{ConnectionAborted, ConnectionRefused, ConnectionReset};
    if matches!(
        e.kind(),
        ConnectionRefused | ConnectionAborted | ConnectionReset
    ) {
        return;
    }
    warn!("accept failed: {e}");
    tokio::time::sleep(Duration::from_millis(500)).await;
}
