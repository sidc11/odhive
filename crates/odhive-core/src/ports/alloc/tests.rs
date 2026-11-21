use std::net::{IpAddr, TcpListener};
use std::thread;

use super::*;

// Tests run in parallel processes (nextest), so each test scans its own port area.

/// First port of `len` consecutive ports at or after `base` that are free right now.
fn free_block(base: u16, len: u16) -> u16 {
    let mut start = base;
    while start < base + 250 {
        match (start..start + len).find(|p| !is_free(*p).expect("probe")) {
            None => return start,
            Some(busy) => start = busy + 1,
        }
    }
    panic!("no {len} free ports near {base}");
}

fn opts(start: u16) -> AllocOptions {
    AllocOptions {
        http_start: start,
        gevent_start: start + 3,
        scan: 20,
    }
}

fn listen(ip: impl Into<IpAddr>, port: u16) -> TcpListener {
    TcpListener::bind(SocketAddr::from((ip.into(), port))).expect("test listener")
}

fn owner(instance: &str) -> PortOwner {
    PortOwner::new("acme", instance)
}

#[test]
fn defaults_are_odoo_ports() {
    let o = AllocOptions::default();
    assert_eq!((o.http_start, o.gevent_start), (8069, 8072));
    assert!(o.scan >= 10);
}

#[test]
fn free_defaults_are_used_unchanged() {
    let base = free_block(21_000, 30);
    let reg = PortRegistry::new();
    let r = reg
        .allocate(owner("main"), None, None, &opts(base))
        .expect("allocate");
    assert_eq!((r.http(), r.gevent()), (base, base + 3));
    assert!(r.changes().is_empty());
    assert_eq!(reg.owner_of(base), Some(owner("main")));
    assert_eq!(reg.owner_of(base + 3), Some(owner("main")));
}

#[test]
fn busy_http_port_moves_and_is_reported() {
    let base = free_block(21_300, 30);
    let _busy = listen(Ipv4Addr::LOCALHOST, base);
    let r = PortRegistry::new()
        .allocate(owner("main"), None, None, &opts(base))
        .expect("allocate");
    assert_eq!(r.http(), base + 1);
    assert_eq!(r.gevent(), base + 3);
    assert_eq!(
        r.changes(),
        [PortChange {
            kind: PortKind::Http,
            requested: base,
            assigned: base + 1,
            reason: Unavailable::InUse,
        }]
    );
    assert_eq!(
        r.changes()[0].to_string(),
        format!("http port {base} busy → using {}", base + 1)
    );
}

#[test]
fn wildcard_listener_counts_as_busy() {
    let base = free_block(21_600, 30);
    let _busy = listen(Ipv4Addr::UNSPECIFIED, base);
    assert!(!is_free(base).expect("probe"));
    let r = PortRegistry::new()
        .allocate(owner("main"), None, None, &opts(base))
        .expect("allocate");
    assert_ne!(r.http(), base);
}

#[test]
fn ipv6_loopback_listener_counts_as_busy() {
    let base = free_block(21_900, 30);
    let Ok(_busy) = TcpListener::bind(SocketAddr::from((Ipv6Addr::LOCALHOST, base))) else {
        return; // no IPv6 on this machine
    };
    assert!(!is_free(base).expect("probe"));
}

#[test]
fn closed_port_is_free_again() {
    let base = free_block(22_200, 30);
    let l = listen(Ipv4Addr::LOCALHOST, base);
    assert!(!is_free(base).expect("probe"));
    drop(l);
    assert!(is_free(base).expect("probe"));
}

#[test]
fn second_instance_skips_reserved_ports() {
    let base = free_block(22_500, 30);
    let reg = PortRegistry::new();
    let a = reg
        .allocate(owner("main"), None, None, &opts(base))
        .expect("first");
    let b = reg
        .allocate(owner("test"), None, None, &opts(base))
        .expect("second");
    assert_eq!((a.http(), a.gevent()), (base, base + 3));
    assert_eq!((b.http(), b.gevent()), (base + 1, base + 4));
    assert_eq!(
        b.changes()[0].to_string(),
        format!("http port {base} used by acme#main → using {}", base + 1)
    );
    assert_eq!(b.changes()[1].kind, PortKind::Gevent);
}

#[test]
fn many_instances_get_distinct_ports() {
    let base = free_block(22_800, 60);
    let reg = PortRegistry::new();
    let all: Vec<_> = (0..8)
        .map(|i| {
            reg.allocate(owner(&format!("i{i}")), None, None, &opts(base))
                .expect("allocate")
        })
        .collect();
    let mut ports: Vec<_> = all.iter().flat_map(|r| [r.http(), r.gevent()]).collect();
    ports.sort_unstable();
    ports.dedup();
    assert_eq!(ports.len(), 16);
    assert_eq!(reg.reserved().len(), 16);
}

#[test]
fn concurrent_allocations_never_share_a_port() {
    let base = free_block(23_200, 60);
    let reg = PortRegistry::new();
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let reg = reg.clone();
            thread::spawn(move || {
                let r = reg
                    .allocate(owner(&format!("t{i}")), None, None, &opts(base))
                    .expect("allocate");
                let ports = (r.http(), r.gevent());
                std::mem::forget(r); // keep reserved until the end of the test
                ports
            })
        })
        .collect();
    let mut ports: Vec<_> = handles
        .into_iter()
        .flat_map(|h| {
            let (a, b) = h.join().expect("thread");
            [a, b]
        })
        .collect();
    ports.sort_unstable();
    ports.dedup();
    assert_eq!(ports.len(), 16);
}

#[test]
fn drop_releases_ports() {
    let base = free_block(23_600, 30);
    let reg = PortRegistry::new();
    let r = reg
        .allocate(owner("main"), None, None, &opts(base))
        .expect("allocate");
    drop(r);
    assert!(reg.reserved().is_empty());
    let again = reg
        .allocate(owner("test"), None, None, &opts(base))
        .expect("allocate");
    assert_eq!(again.http(), base);
}

#[test]
fn owner_cannot_reserve_twice() {
    let base = free_block(23_900, 30);
    let reg = PortRegistry::new();
    let _r = reg
        .allocate(owner("main"), None, None, &opts(base))
        .expect("allocate");
    let err = reg
        .allocate(owner("main"), None, None, &opts(base))
        .expect_err("twice");
    assert!(matches!(err, AllocError::AlreadyReserved(o) if o == owner("main")));
    assert_eq!(reg.reserved().len(), 2);
}

#[test]
fn pinned_free_port_is_used() {
    let base = free_block(24_200, 30);
    let r = PortRegistry::new()
        .allocate(owner("main"), Some(base + 10), Some(base + 11), &opts(base))
        .expect("allocate");
    assert_eq!((r.http(), r.gevent()), (base + 10, base + 11));
    assert!(r.changes().is_empty());
}

#[test]
fn pinned_busy_port_is_an_error_not_a_move() {
    let base = free_block(24_500, 30);
    let _busy = listen(Ipv4Addr::LOCALHOST, base);
    let reg = PortRegistry::new();
    let err = reg
        .allocate(owner("main"), Some(base), None, &opts(base))
        .expect_err("pinned busy");
    assert!(matches!(
        err,
        AllocError::PinnedUnavailable {
            kind: PortKind::Http,
            reason: Unavailable::InUse,
            ..
        }
    ));
    assert!(err.to_string().contains("remove the pinned port"));
    assert!(reg.reserved().is_empty(), "nothing reserved on failure");
}

#[test]
fn pinned_port_reserved_by_other_instance_names_it() {
    let base = free_block(24_800, 30);
    let reg = PortRegistry::new();
    let _a = reg
        .allocate(owner("main"), None, None, &opts(base))
        .expect("first");
    let err = reg
        .allocate(owner("test"), None, Some(base), &opts(base))
        .expect_err("clash");
    assert!(
        err.to_string()
            .contains(&format!("gevent port {base} is used by acme#main")),
        "{err}"
    );
}

#[test]
fn pinned_port_below_min_is_rejected() {
    let err = PortRegistry::new()
        .allocate(owner("main"), Some(80), None, &AllocOptions::default())
        .expect_err("privileged");
    assert!(matches!(
        err,
        AllocError::Privileged {
            kind: PortKind::Http,
            port: 80
        }
    ));
}

#[test]
fn pinned_same_port_twice_is_rejected() {
    let base = free_block(25_100, 30);
    let err = PortRegistry::new()
        .allocate(owner("main"), Some(base), Some(base), &opts(base))
        .expect_err("same");
    assert!(matches!(
        err,
        AllocError::PinnedUnavailable {
            kind: PortKind::Gevent,
            reason: Unavailable::OtherPort,
            ..
        }
    ));
}

#[test]
fn automatic_port_avoids_pinned_partner() {
    let base = free_block(25_400, 30);
    // http pinned on the gevent start: gevent moves up.
    let r = PortRegistry::new()
        .allocate(owner("main"), Some(base + 3), None, &opts(base))
        .expect("allocate");
    assert_eq!((r.http(), r.gevent()), (base + 3, base + 4));
    assert_eq!(r.changes()[0].reason, Unavailable::OtherPort);
}

#[test]
fn no_free_port_in_scan() {
    let base = free_block(25_700, 30);
    let _busy: Vec<_> = (base..base + 3)
        .map(|p| listen(Ipv4Addr::LOCALHOST, p))
        .collect();
    let o = AllocOptions {
        http_start: base,
        gevent_start: base + 10,
        scan: 3,
    };
    let err = PortRegistry::new()
        .allocate(owner("main"), None, None, &o)
        .expect_err("full");
    assert!(matches!(
        err,
        AllocError::NoFreePort { kind: PortKind::Http, first, last } if first == base && last == base + 2
    ));
}

#[test]
fn scan_stops_at_the_last_port() {
    let o = AllocOptions {
        http_start: u16::MAX,
        gevent_start: u16::MAX,
        scan: 100,
    };
    let reg = PortRegistry::new();
    let _a = reg.reserve(owner("main"), u16::MAX, u16::MAX - 1);
    let err = reg
        .allocate(owner("test"), None, None, &o)
        .expect_err("full");
    assert!(matches!(
        err,
        AllocError::NoFreePort {
            first: u16::MAX,
            last: u16::MAX,
            ..
        }
    ));
}

#[test]
fn automatic_start_below_min_is_raised() {
    let r = PortRegistry::new().allocate(
        owner("main"),
        None,
        None,
        &AllocOptions {
            http_start: 0,
            gevent_start: 1,
            scan: 1,
        },
    );
    // Either free at MIN_PORT or reported against MIN_PORT; never a privileged port.
    match r {
        Ok(r) => assert!(r.http() >= MIN_PORT && r.gevent() >= MIN_PORT),
        Err(AllocError::NoFreePort { first, .. }) => assert_eq!(first, MIN_PORT),
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn reserve_skips_probe_for_running_instances() {
    let base = free_block(26_000, 30);
    // The re-attached instance's own process holds its ports.
    let _own = listen(Ipv4Addr::LOCALHOST, base);
    let reg = PortRegistry::new();
    let r = reg.reserve(owner("main"), base, base + 3).expect("reserve");
    assert_eq!((r.http(), r.gevent()), (base, base + 3));
    let next = reg
        .allocate(owner("test"), None, None, &opts(base))
        .expect("allocate");
    assert_eq!(next.http(), base + 1);
}

#[test]
fn reserve_rejects_clashes_and_bad_ports() {
    let reg = PortRegistry::new();
    let _a = reg.reserve(owner("main"), 30_001, 30_002).expect("reserve");
    assert!(matches!(
        reg.reserve(owner("test"), 30_002, 30_003),
        Err(AllocError::PinnedUnavailable {
            reason: Unavailable::ReservedBy(_),
            ..
        })
    ));
    assert!(matches!(
        reg.reserve(owner("test"), 30_005, 30_005),
        Err(AllocError::PinnedUnavailable {
            reason: Unavailable::OtherPort,
            ..
        })
    ));
    assert!(matches!(
        reg.reserve(owner("test"), 1023, 30_006),
        Err(AllocError::Privileged { .. })
    ));
    assert!(matches!(
        reg.reserve(owner("main"), 30_010, 30_011),
        Err(AllocError::AlreadyReserved(_))
    ));
    assert_eq!(reg.reserved().len(), 2, "failed reserves leave no entries");
}

#[test]
fn allocate_instance_uses_its_pins() {
    let base = free_block(26_300, 30);
    let mut inst = Instance::new("main");
    inst.http_port = Some(base + 7);
    let r = PortRegistry::new()
        .allocate_instance("acme", &inst, &opts(base))
        .expect("allocate");
    assert_eq!(r.owner(), &owner("main"));
    assert_eq!((r.http(), r.gevent()), (base + 7, base + 3));
}

#[test]
fn change_serializes_for_events() {
    let c = PortChange {
        kind: PortKind::Gevent,
        requested: 8072,
        assigned: 8073,
        reason: Unavailable::ReservedBy(owner("main")),
    };
    let json = serde_json::to_value(&c).expect("json");
    assert_eq!(
        json,
        serde_json::json!({
            "kind": "gevent",
            "requested": 8072,
            "assigned": 8073,
            "reason": {"reason": "reserved_by", "owner": {"project": "acme", "instance": "main"}}
        })
    );
}

#[test]
fn time_wait_after_restart_is_not_busy() {
    use std::io::Read;
    use std::net::TcpStream;

    let base = free_block(26_600, 30);
    let l = listen(Ipv4Addr::LOCALHOST, base);
    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, base)).expect("connect");
    let (server_side, _) = l.accept().expect("accept");
    // The server closes first, so its end of the connection enters TIME_WAIT.
    drop(server_side);
    let _ = client.read(&mut [0; 1]);
    drop(client);
    drop(l);
    assert!(is_free(base).expect("probe"));
}
