//! Port allocation for Odoo instances.
//!
//! Each instance needs an HTTP port and a gevent (websocket) port. Ports left unset
//! in `odhive.toml` are picked automatically, starting at Odoo's defaults (8069 and
//! 8072) and moving up past busy ones. Every move is reported as a [`PortChange`]
//! ("http port 8069 busy → using 8070"). Ports set explicitly are pinned: if a pinned
//! port is taken, allocation fails instead of silently moving it.
//!
//! A port is free when it is not reserved by another OdHive instance in the
//! [`PortRegistry`] and can be bound on `127.0.0.1`, `0.0.0.0` and (when the machine
//! has IPv6) `::1` and `::`, see [`is_free`].
//!
//! The probe and Odoo's own bind are not atomic: another program can still take a
//! port in between. The registry only rules out clashes between OdHive's instances.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

#[cfg(not(target_vendor = "apple"))]
use rustix::net::SocketFlags;
use rustix::net::{self, AddressFamily, SocketType, sockopt};
use serde::Serialize;
use thiserror::Error;

use crate::project::{Instance, MIN_PORT};

/// Odoo's default HTTP port.
pub const DEFAULT_HTTP_PORT: u16 = 8069;
/// Odoo's default gevent (websocket) port.
pub const DEFAULT_GEVENT_PORT: u16 = 8072;
/// How many ports are tried per kind before giving up.
pub const DEFAULT_SCAN: u16 = 100;

/// The OdHive instance a port belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct PortOwner {
    /// Project name.
    pub project: String,
    /// Instance id within the project.
    pub instance: String,
}

impl PortOwner {
    /// Owner for `project#instance`.
    #[must_use]
    pub fn new(project: impl Into<String>, instance: impl Into<String>) -> Self {
        Self {
            project: project.into(),
            instance: instance.into(),
        }
    }
}

impl fmt::Display for PortOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.project, self.instance)
    }
}

/// Which of an instance's ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PortKind {
    /// `--http-port`.
    Http,
    /// `--gevent-port`.
    Gevent,
}

impl fmt::Display for PortKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Http => "http",
            Self::Gevent => "gevent",
        })
    }
}

/// Why a port could not be used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "reason", content = "owner")]
pub enum Unavailable {
    /// Another program is listening on it.
    InUse,
    /// Reserved by another OdHive instance.
    ReservedBy(PortOwner),
    /// Already taken by the other port of the same instance.
    OtherPort,
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InUse => f.write_str("busy"),
            Self::ReservedBy(owner) => write!(f, "used by {owner}"),
            Self::OtherPort => f.write_str("used by the other port of this instance"),
        }
    }
}

/// An automatic port that moved away from its starting point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PortChange {
    /// Which port.
    pub kind: PortKind,
    /// Port tried first.
    pub requested: u16,
    /// Port assigned instead.
    pub assigned: u16,
    /// Why `requested` was not used.
    pub reason: Unavailable,
}

impl fmt::Display for PortChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} port {} {} → using {}",
            self.kind, self.requested, self.reason, self.assigned
        )
    }
}

/// Errors from [`PortRegistry`].
#[derive(Debug, Error)]
pub enum AllocError {
    /// The instance already holds a reservation.
    #[error("{0} already has ports reserved")]
    AlreadyReserved(PortOwner),
    /// A pinned port is below [`MIN_PORT`].
    #[error("{kind} port {port} is privileged; use {MIN_PORT} or higher")]
    Privileged {
        /// Which port.
        kind: PortKind,
        /// Offending port.
        port: u16,
    },
    /// A pinned port cannot be used.
    #[error("{kind} port {port} is {reason}; free it or remove the pinned port from odhive.toml")]
    PinnedUnavailable {
        /// Which port.
        kind: PortKind,
        /// Pinned port.
        port: u16,
        /// Why it is unavailable.
        reason: Unavailable,
    },
    /// No free port in the scanned range.
    #[error("no free {kind} port in {first}..={last}")]
    NoFreePort {
        /// Which port.
        kind: PortKind,
        /// First port tried.
        first: u16,
        /// Last port tried.
        last: u16,
    },
    /// Probing failed with an unexpected error.
    #[error("failed to probe {addr}: {source}")]
    Probe {
        /// Address probed.
        addr: SocketAddr,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
}

/// Where automatic ports start and how far they may move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocOptions {
    /// First HTTP port tried.
    pub http_start: u16,
    /// First gevent port tried.
    pub gevent_start: u16,
    /// Ports tried per kind (at least 1).
    pub scan: u16,
}

impl Default for AllocOptions {
    fn default() -> Self {
        Self {
            http_start: DEFAULT_HTTP_PORT,
            gevent_start: DEFAULT_GEVENT_PORT,
            scan: DEFAULT_SCAN,
        }
    }
}

type Table = BTreeMap<u16, PortOwner>;

/// Ports reserved by OdHive's running instances. Cheap to clone; clones share state.
///
/// Unset ports are picked from Odoo's defaults (8069 / 8072) upward, and every move
/// is reported as a [`PortChange`]. Pinned ports never move: a taken pinned port is
/// an error. A port is free when no other instance holds it and [`is_free`] says so.
/// The probe and Odoo's own bind are not atomic, so another program can still take
/// a port in between; the registry only rules out clashes between OdHive instances.
#[derive(Debug, Clone, Default)]
pub struct PortRegistry {
    table: Arc<Mutex<Table>>,
}

impl PortRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocates ports for `instance` of `project`, see [`PortRegistry::allocate`].
    ///
    /// # Errors
    /// See [`PortRegistry::allocate`].
    pub fn allocate_instance(
        &self,
        project: &str,
        instance: &Instance,
        opts: &AllocOptions,
    ) -> Result<Reservation, AllocError> {
        self.allocate(
            PortOwner::new(project, &instance.id),
            instance.http_port,
            instance.gevent_port,
            opts,
        )
    }

    /// Reserves an HTTP and a gevent port for `owner`. `Some` ports are pinned,
    /// `None` ports are picked from `opts`.
    ///
    /// # Errors
    /// [`AllocError::AlreadyReserved`] if `owner` holds ports,
    /// [`AllocError::Privileged`] / [`AllocError::PinnedUnavailable`] for an unusable
    /// pinned port, [`AllocError::NoFreePort`] when the scan finds nothing, and
    /// [`AllocError::Probe`] for unexpected bind errors.
    pub fn allocate(
        &self,
        owner: PortOwner,
        http: Option<u16>,
        gevent: Option<u16>,
        opts: &AllocOptions,
    ) -> Result<Reservation, AllocError> {
        let mut table = self.lock();
        ensure_unreserved(&table, &owner)?;

        // Pinned ports first, so automatic ones can avoid them.
        let mut http_port = http
            .map(|p| pinned(&table, PortKind::Http, p, None, true))
            .transpose()?;
        let mut gevent_port = gevent
            .map(|p| pinned(&table, PortKind::Gevent, p, http_port, true))
            .transpose()?;

        let mut changes = Vec::new();
        if http_port.is_none() {
            let (port, change) = scan(&table, PortKind::Http, opts.http_start, opts, gevent_port)?;
            http_port = Some(port);
            changes.extend(change);
        }
        if gevent_port.is_none() {
            let (port, change) =
                scan(&table, PortKind::Gevent, opts.gevent_start, opts, http_port)?;
            gevent_port = Some(port);
            changes.extend(change);
        }
        let (http, gevent) = (
            http_port.unwrap_or_default(),
            gevent_port.unwrap_or_default(),
        );
        Ok(self.insert(&mut table, owner, http, gevent, changes))
    }

    /// Reserves known ports without probing them, for instances that are already
    /// running (re-attached from a PID file): their own process holds the ports.
    ///
    /// # Errors
    /// [`AllocError::AlreadyReserved`], [`AllocError::Privileged`], or
    /// [`AllocError::PinnedUnavailable`] when another instance holds a port or both
    /// ports are the same.
    pub fn reserve(
        &self,
        owner: PortOwner,
        http: u16,
        gevent: u16,
    ) -> Result<Reservation, AllocError> {
        let mut table = self.lock();
        ensure_unreserved(&table, &owner)?;
        pinned(&table, PortKind::Http, http, None, false)?;
        pinned(&table, PortKind::Gevent, gevent, Some(http), false)?;
        Ok(self.insert(&mut table, owner, http, gevent, Vec::new()))
    }

    /// The instance holding `port`, if any.
    #[must_use]
    pub fn owner_of(&self, port: u16) -> Option<PortOwner> {
        self.lock().get(&port).cloned()
    }

    /// All reserved ports, ascending.
    #[must_use]
    pub fn reserved(&self) -> Vec<(u16, PortOwner)> {
        self.lock().iter().map(|(p, o)| (*p, o.clone())).collect()
    }

    fn lock(&self) -> MutexGuard<'_, Table> {
        // The table is valid after any panic: entries are inserted and removed whole.
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn insert(
        &self,
        table: &mut Table,
        owner: PortOwner,
        http: u16,
        gevent: u16,
        changes: Vec<PortChange>,
    ) -> Reservation {
        table.insert(http, owner.clone());
        table.insert(gevent, owner.clone());
        Reservation {
            registry: self.clone(),
            owner,
            http,
            gevent,
            changes,
        }
    }
}

/// Ports held for one instance. Dropping it releases them.
#[derive(Debug)]
#[must_use = "dropping a reservation releases its ports"]
pub struct Reservation {
    registry: PortRegistry,
    owner: PortOwner,
    http: u16,
    gevent: u16,
    changes: Vec<PortChange>,
}

impl Reservation {
    /// The instance holding the ports.
    #[must_use]
    pub fn owner(&self) -> &PortOwner {
        &self.owner
    }

    /// HTTP port.
    #[must_use]
    pub fn http(&self) -> u16 {
        self.http
    }

    /// Gevent port.
    #[must_use]
    pub fn gevent(&self) -> u16 {
        self.gevent
    }

    /// Automatic ports that moved away from their starting point.
    #[must_use]
    pub fn changes(&self) -> &[PortChange] {
        &self.changes
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut table = self.registry.lock();
        for port in [self.http, self.gevent] {
            if table.get(&port) == Some(&self.owner) {
                table.remove(&port);
            }
        }
    }
}

fn ensure_unreserved(table: &Table, owner: &PortOwner) -> Result<(), AllocError> {
    if table.values().any(|o| o == owner) {
        return Err(AllocError::AlreadyReserved(owner.clone()));
    }
    Ok(())
}

/// Checks a pinned port; `probe` = also check that nothing else listens on it.
fn pinned(
    table: &Table,
    kind: PortKind,
    port: u16,
    other: Option<u16>,
    probe: bool,
) -> Result<u16, AllocError> {
    if port < MIN_PORT {
        return Err(AllocError::Privileged { kind, port });
    }
    match availability(table, port, other, probe)? {
        None => Ok(port),
        Some(reason) => Err(AllocError::PinnedUnavailable { kind, port, reason }),
    }
}

/// Finds the first usable port from `start`, reporting a move away from it.
fn scan(
    table: &Table,
    kind: PortKind,
    start: u16,
    opts: &AllocOptions,
    other: Option<u16>,
) -> Result<(u16, Option<PortChange>), AllocError> {
    let first = start.max(MIN_PORT);
    let last = first.saturating_add(opts.scan.max(1) - 1);
    let mut first_reason = None;
    for port in first..=last {
        match availability(table, port, other, true)? {
            None => {
                let change = first_reason.map(|reason| PortChange {
                    kind,
                    requested: first,
                    assigned: port,
                    reason,
                });
                return Ok((port, change));
            }
            Some(reason) => {
                first_reason.get_or_insert(reason);
            }
        }
    }
    Err(AllocError::NoFreePort { kind, first, last })
}

fn availability(
    table: &Table,
    port: u16,
    other: Option<u16>,
    probe: bool,
) -> Result<Option<Unavailable>, AllocError> {
    if other == Some(port) {
        return Ok(Some(Unavailable::OtherPort));
    }
    if let Some(owner) = table.get(&port) {
        return Ok(Some(Unavailable::ReservedBy(owner.clone())));
    }
    if probe && !is_free(port)? {
        return Ok(Some(Unavailable::InUse));
    }
    Ok(None)
}

/// Whether `port` can be bound on `127.0.0.1` and `0.0.0.0`, and on `::1` and `::`
/// when the machine has IPv6. The wildcard probes catch servers listening on all
/// interfaces, which a loopback-only probe misses on macOS. Probes set
/// `SO_REUSEADDR` like Odoo does, so `TIME_WAIT` sockets left by a stopped instance
/// do not count as busy. Probe sockets are only bound, never put into the listening
/// state, so nothing is ever reachable on a non-loopback address.
///
/// # Errors
/// [`AllocError::Probe`] for IPv4 errors other than "in use".
pub fn is_free(port: u16) -> Result<bool, AllocError> {
    for ip in [Ipv4Addr::LOCALHOST, Ipv4Addr::UNSPECIFIED] {
        let addr = SocketAddr::from((ip, port));
        match probe_bind(addr) {
            Ok(()) => {}
            Err(e) if super::is_busy(&e) => return Ok(false),
            Err(source) => return Err(AllocError::Probe { addr, source }),
        }
    }
    for ip in [Ipv6Addr::LOCALHOST, Ipv6Addr::UNSPECIFIED] {
        match probe_bind(SocketAddr::from((ip, port))) {
            Err(e) if super::is_busy(&e) => return Ok(false),
            // Bound, or no IPv6 on this machine.
            _ => {}
        }
    }
    Ok(true)
}

/// Binds a TCP socket on `addr` without listening, then closes it.
fn probe_bind(addr: SocketAddr) -> io::Result<()> {
    let family = if addr.is_ipv4() {
        AddressFamily::INET
    } else {
        AddressFamily::INET6
    };
    #[cfg(not(target_vendor = "apple"))]
    let fd = net::socket_with(family, SocketType::STREAM, SocketFlags::CLOEXEC, None)?;
    #[cfg(target_vendor = "apple")]
    let fd = {
        let fd = net::socket(family, SocketType::STREAM, None)?;
        rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC)?;
        fd
    };
    sockopt::set_socket_reuseaddr(&fd, true)?;
    net::bind(&fd, &addr)?;
    Ok(())
}

#[cfg(test)]
mod tests;
