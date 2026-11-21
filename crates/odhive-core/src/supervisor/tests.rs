use std::collections::BTreeMap;

use super::*;

fn spec() -> LaunchSpec {
    LaunchSpec::new(
        "/venv/bin/python".into(),
        "/src/odoo-bin".into(),
        "/proj".into(),
    )
}

#[test]
fn argv_puts_managed_options_first_and_extra_args_last() {
    let mut s = spec();
    s.conf = Some("/proj/odoo.conf".into());
    s.db = Some("shop_main".into());
    s.workers = 2;
    s.extra_args = vec!["--dev".into(), "xml,reload".into()];
    let argv: Vec<String> = s
        .argv(8070, 8073)
        .into_iter()
        .map(|a| a.into_string().expect("utf-8"))
        .collect();
    assert_eq!(
        argv,
        [
            "/venv/bin/python",
            "/src/odoo-bin",
            "-c",
            "/proj/odoo.conf",
            "--http-port",
            "8070",
            "--gevent-port",
            "8073",
            "-d",
            "shop_main",
            "--workers",
            "2",
            "--dev",
            "xml,reload",
        ]
    );
}

#[test]
fn minimal_argv_has_ports_and_workers_only() {
    let args: Vec<OsString> = spec().odoo_args(8069, 8072);
    assert_eq!(
        args,
        [
            "--http-port",
            "8069",
            "--gevent-port",
            "8072",
            "--workers",
            "0"
        ]
        .map(OsString::from)
        .to_vec()
    );
}

#[test]
fn validate_rejects_managed_extra_args() {
    for bad in ["--http-port=9000", "-p9000", "-d", "--database=x", "-cfoo"] {
        let mut s = spec();
        s.extra_args = vec![bad.into()];
        assert!(
            matches!(s.validate(), Err(SupervisorError::ManagedOption(a)) if a == bad),
            "{bad}"
        );
    }
    let mut ok = spec();
    ok.extra_args = vec!["--dev=all".into(), "--log-level=debug".into()];
    ok.validate().expect("valid");
}

#[test]
fn validate_rejects_bad_db_workers_and_env() {
    let mut s = spec();
    s.db = Some("db; DROP TABLE x".into());
    assert!(matches!(s.validate(), Err(SupervisorError::Name(_))));

    let mut s = spec();
    s.db = Some("-oops".into());
    assert!(matches!(s.validate(), Err(SupervisorError::Name(_))));

    let mut s = spec();
    s.workers = MAX_WORKERS + 1;
    assert!(matches!(
        s.validate(),
        Err(SupervisorError::TooManyWorkers(_))
    ));

    let mut s = spec();
    s.env = EnvVars(BTreeMap::from([("BAD=KEY".into(), "v".into())]));
    assert!(matches!(s.validate(), Err(SupervisorError::Name(_))));
}

#[test]
fn port_in_use_is_detected_on_linux_and_macos_messages() {
    assert!(is_port_in_use("OSError: [Errno 98] Address already in use"));
    assert!(is_port_in_use("OSError: [Errno 48] Address already in use"));
    assert!(!is_port_in_use(
        "HTTP service (werkzeug) running on host:8069"
    ));
}

#[test]
fn tail_keeps_the_last_lines() {
    let mut tail = Tail::default();
    for i in 0..TAIL_LINES + 5 {
        tail.push(&format!("line {i}"));
    }
    let joined = tail.joined();
    assert_eq!(joined.lines().count(), TAIL_LINES);
    assert!(joined.starts_with("line 5\n"));
    assert!(!tail.port_in_use);
}

#[test]
fn status_serializes_with_a_state_tag() {
    let s = Status::Crashed {
        exit: ExitInfo {
            code: Some(1),
            signal: None,
            port_in_use: false,
            tail: String::new(),
        },
    };
    let json = serde_json::to_value(&s).expect("json");
    assert_eq!(json["state"], "crashed");
    assert_eq!(json["exit"]["code"], 1);
    assert!(s.is_terminal());
    assert_eq!(s.pid(), None);
    assert_eq!(Status::Running { pid: 7 }.pid(), Some(7));

    let e = Event::Status {
        status: Status::Running { pid: 7 },
    };
    let json = serde_json::to_value(&e).expect("json");
    assert_eq!(json["event"], "status");
    assert_eq!(json["status"]["state"], "running");
}
