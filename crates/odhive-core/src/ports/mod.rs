//! Ports: binding OdHive's own loopback listeners ([`bind_loopback`]) and allocating
//! ports for Odoo instances ([`PortRegistry`]).

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use thiserror::Error;
use tokio::net::TcpListener;

mod alloc;

pub use alloc::{
    AllocError, AllocOptions, DEFAULT_GEVENT_PORT, DEFAULT_HTTP_PORT, DEFAULT_SCAN, PortChange,
    PortKind, PortOwner, PortRegistry, Reservation, Unavailable, is_free,
};

/// Default port for the OdHive web UI.
///
/// 7069 is unassigned in the IANA registry and has no well-known unofficial use. It
/// sits away from Odoo's own ports (8069/8072 and the range OdHive hands out to
/// instances) and from common dev ports such as 9001 (Supervisor, MinIO console).
pub const DEFAULT_UI_PORT: u16 = 7069;

/// Ports the UI must never default to: Odoo, PostgreSQL and common developer tools.
pub const AVOIDED_PORTS: &[u16] = &[
    80, 443, 1433, 3000, 3001, 3306, 4000, 4200, 5000, 5173, 5432, 5433, 5672, 6379, 6969, 8000,
    8001, 8008, 8069, 8070, 8071, 8072, 8080, 8081, 8088, 8443, 8888, 9000, 9001, 9002, 9090, 9200,
    9229, 9300, 11211, 15672, 27017,
];

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

    #[test]
    fn default_ui_port_range_avoids_known_ports() {
        let range = DEFAULT_UI_PORT..DEFAULT_UI_PORT + DEFAULT_PORT_ATTEMPTS;
        for p in AVOIDED_PORTS {
            assert!(!range.contains(p), "fallback range {range:?} contains {p}");
        }
        // Below the Linux (32768+) and macOS (49152+) ephemeral ranges.
        assert!(range.end < 32_768);
        // Unprivileged: no root needed to bind.
        assert!(range.start >= 1024);
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
