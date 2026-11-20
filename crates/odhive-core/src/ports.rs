//! Binding loopback listeners with automatic fallback to the next free port.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use thiserror::Error;
use tokio::net::TcpListener;

/// Default port for the OdHive web UI.
pub const DEFAULT_UI_PORT: u16 = 9001;

/// How many consecutive ports are tried before giving up.
pub const DEFAULT_PORT_ATTEMPTS: u16 = 20;

/// Errors returned while binding loopback listeners.
#[derive(Debug, Error)]
pub enum PortError {
    /// Every port in the tried range was in use.
    #[error("no free port in range {first}..={last}")]
    NoFreePort {
        /// First port tried.
        first: u16,
        /// Last port tried.
        last: u16,
    },
    /// An unexpected I/O error occurred while binding.
    #[error("failed to bind {addr}: {source}")]
    Bind {
        /// Address that failed.
        addr: SocketAddr,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
}

/// Listeners bound to the loopback interfaces on a single port.
#[derive(Debug)]
pub struct LoopbackListeners {
    /// The port all listeners share.
    pub port: u16,
    /// IPv4 `127.0.0.1` listener (always present).
    pub v4: TcpListener,
    /// IPv6 `::1` listener, if IPv6 loopback is available on this machine.
    pub v6: Option<TcpListener>,
}

impl LoopbackListeners {
    /// The URL users should open in their browser.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://localhost:{}", self.port)
    }
}

/// Binds `127.0.0.1` (and `::1` when available) on `preferred`, or on the next free port.
///
/// Only loopback addresses are ever bound. A port counts as free only when both
/// IPv4 and (if supported) IPv6 loopback can be bound on it.
///
/// # Errors
/// Returns [`PortError::NoFreePort`] when `attempts` consecutive ports are busy, or
/// [`PortError::Bind`] for unexpected bind errors.
pub async fn bind_loopback(preferred: u16, attempts: u16) -> Result<LoopbackListeners, PortError> {
    let attempts = attempts.max(1);
    let last = preferred.saturating_add(attempts - 1);
    for port in preferred..=last {
        if port == 0 {
            continue;
        }
        let v4_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let v4 = match TcpListener::bind(v4_addr).await {
            Ok(l) => l,
            Err(e) if is_busy(&e) => continue,
            Err(source) => {
                return Err(PortError::Bind {
                    addr: v4_addr,
                    source,
                });
            }
        };
        let v6_addr = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
        let v6 = match TcpListener::bind(v6_addr).await {
            Ok(l) => Some(l),
            Err(e) if is_busy(&e) => continue,
            // IPv6 loopback unavailable (e.g. disabled): serve IPv4 only.
            Err(_) => None,
        };
        return Ok(LoopbackListeners { port, v4, v6 });
    }
    Err(PortError::NoFreePort {
        first: preferred,
        last,
    })
}

fn is_busy(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Finds a port whose IPv4 and IPv6 loopback are both free right now.
    async fn free_port() -> u16 {
        let l = bind_loopback(20_000, 5_000).await.expect("free port");
        l.port
    }

    #[tokio::test]
    async fn binds_preferred_port_when_free() {
        let port = free_port().await;
        let l = bind_loopback(port, 1).await.expect("bind");
        assert_eq!(l.port, port);
        assert!(l.v4.local_addr().expect("addr").ip().is_loopback());
        if let Some(v6) = &l.v6 {
            assert!(v6.local_addr().expect("addr").ip().is_loopback());
        }
    }

    #[tokio::test]
    async fn falls_back_to_next_port_when_busy() {
        let taken = bind_loopback(30_000, 5_000).await.expect("first bind");
        let busy = taken.port;
        let next = bind_loopback(busy, 10).await.expect("fallback bind");
        assert!(
            next.port > busy,
            "expected a port after {busy}, got {}",
            next.port
        );
        assert_eq!(next.url(), format!("http://localhost:{}", next.port));
    }

    #[tokio::test]
    async fn errors_when_every_port_is_busy() {
        let taken = bind_loopback(40_000, 5_000).await.expect("first bind");
        let err = bind_loopback(taken.port, 1).await.expect_err("should fail");
        assert!(matches!(err, PortError::NoFreePort { .. }));
    }

    #[tokio::test]
    async fn only_loopback_is_bound() {
        let l = bind_loopback(free_port().await, 1).await.expect("bind");
        assert_eq!(
            l.v4.local_addr().expect("addr").ip(),
            std::net::IpAddr::from(Ipv4Addr::LOCALHOST)
        );
    }
}
