use std::path::{Path, PathBuf};

use super::*;
use crate::project::Instance;
use crate::supervisor::Stream;

fn project() -> ProjectConfig {
    let mut p = ProjectConfig::new("shop", "odoo-19.0");
    p.env.0.insert("PGHOST".into(), "localhost".into());
    let mut test = Instance::new("test");
    test.http_port = Some(9069);
    test.db = Some("shop_tests".into());
    test.workers = 2;
    test.extra_args = vec!["--dev=xml".into()];
    p.instances.push(test);
    p
}

fn plan(p: &ProjectConfig, id: &str) -> Result<InstancePlan, ManagerError> {
    InstancePlan::for_instance(
        p,
        Path::new("/w/projects/shop"),
        id,
        PathBuf::from("/w/projects/shop/.venv/bin/python"),
        PathBuf::from("/w/sources/odoo-19.0/odoo-bin"),
        Some(PathBuf::from("/w/projects/shop/odoo.conf")),
    )
}

#[test]
fn plan_takes_ports_db_and_args_from_the_instance() {
    let p = project();
    let main = plan(&p, "main").expect("main");
    assert_eq!(main.owner, PortOwner::new("shop", "main"));
    assert_eq!(main.spec.db.as_deref(), Some("shop_main"));
    assert!(main.ports_automatic());
    assert_eq!(main.spec.workers, 0);
    assert!(main.spec.extra_args.is_empty());

    let test = plan(&p, "test").expect("test");
    assert_eq!(test.http_port, Some(9069));
    assert!(!test.ports_automatic());
    assert_eq!(test.spec.db.as_deref(), Some("shop_tests"));
    assert_eq!(test.spec.workers, 2);
    assert_eq!(test.spec.extra_args, ["--dev=xml"]);
    assert_eq!(test.spec.cwd, Path::new("/w/projects/shop"));
    assert_eq!(
        test.spec.conf.as_deref(),
        Some(Path::new("/w/projects/shop/odoo.conf"))
    );
    assert_eq!(
        test.spec.env.0.get("PGHOST").map(String::as_str),
        Some("localhost")
    );
}

#[test]
fn plan_rejects_unknown_instances_and_invalid_projects() {
    let p = project();
    assert!(matches!(
        plan(&p, "nope"),
        Err(ManagerError::UnknownInstance { instance, .. }) if instance == "nope"
    ));

    let mut bad = project();
    bad.instances[1].extra_args = vec!["--http-port=1234".into()];
    assert!(matches!(
        plan(&bad, "test"),
        Err(ManagerError::Project(ProjectError::ManagedOption { .. }))
    ));

    let mut clash = project();
    clash.instances[1].db = Some("shop_main".into());
    assert!(matches!(
        plan(&clash, "main"),
        Err(ManagerError::Project(ProjectError::DatabaseClash(_)))
    ));
}

#[test]
fn only_fully_automatic_ports_are_retried() {
    let p = project();
    let mut one = plan(&p, "main").expect("main");
    one.gevent_port = Some(9072);
    assert!(!one.ports_automatic());
}

#[test]
fn events_serialize_flat_with_the_instance() {
    let owner = PortOwner::new("shop", "main");
    let output = ManagerEvent {
        owner: owner.clone(),
        event: InstanceEvent::Supervisor(Event::Output {
            stream: Stream::Stderr,
            line: "hello".into(),
        }),
    };
    let json = serde_json::to_value(&output).expect("json");
    assert_eq!(json["project"], "shop");
    assert_eq!(json["instance"], "main");
    assert_eq!(json["event"], "output");
    assert_eq!(json["line"], "hello");

    let retry = ManagerEvent {
        owner: owner.clone(),
        event: InstanceEvent::Manager(Notice::PortRetry {
            http_port: 8069,
            gevent_port: 8072,
            exit: ExitInfo {
                code: Some(1),
                signal: None,
                port_in_use: true,
                tail: String::new(),
            },
        }),
    };
    let json = serde_json::to_value(&retry).expect("json");
    assert_eq!(json["event"], "port_retry");
    assert_eq!(json["http_port"], 8069);
    assert_eq!(json["exit"]["code"], 1);

    let status = ManagerEvent {
        owner,
        event: InstanceEvent::Supervisor(Event::Status {
            status: Status::Running { pid: 42 },
        }),
    };
    let json = serde_json::to_value(&status).expect("json");
    assert_eq!(json["event"], "status");
    assert_eq!(json["status"]["state"], "running");
}

#[test]
fn info_and_recovery_serialize() {
    let info = InstanceInfo {
        owner: PortOwner::new("shop", "main"),
        http_port: Some(8069),
        gevent_port: Some(8072),
        db: Some("shop_main".into()),
        supervised: false,
        port_retried: false,
        status: Status::Running { pid: 7 },
    };
    let json = serde_json::to_value(&info).expect("json");
    assert_eq!(json["project"], "shop");
    assert_eq!(json["supervised"], false);
    assert_eq!(json["status"]["state"], "running");

    let rec = Recovery::Stale {
        owner: PortOwner::new("shop", "main"),
        pid: 7,
        killed_leftovers: true,
    };
    let json = serde_json::to_value(&rec).expect("json");
    assert_eq!(json["result"], "stale");
    assert_eq!(json["instance"], "main");

    let stop = StopOutcome::Attached {
        stop: AttachedStop {
            already_gone: false,
            killed: true,
        },
    };
    let json = serde_json::to_value(&stop).expect("json");
    assert_eq!(json["kind"], "attached");
    assert_eq!(json["stop"]["killed"], true);
}

#[tokio::test]
async fn unknown_instances_are_reported() {
    let m = Manager::new(ManagerOptions::default());
    let owner = PortOwner::new("shop", "main");
    assert!(m.get(&owner).is_none());
    assert!(m.list().is_empty());
    assert!(matches!(
        m.stop(&owner).await,
        Err(ManagerError::NotRunning(_))
    ));
    assert!(matches!(
        m.restart(&owner).await,
        Err(ManagerError::NotRunning(_))
    ));
    assert!(matches!(
        m.wait_for(&owner, |_| true).await,
        Err(ManagerError::NotRunning(_))
    ));
    assert!(m.recover().expect("no run dir").is_empty());
    assert!(m.shutdown().await.is_empty());
}
