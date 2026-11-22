//! WebSocket tests: the protocol, batching, lag recovery, new logs, the
//! Origin check, shutdown and slow clients, over real TCP connections.
//!
//! Instances use pinned ports from 19200–19299 (below the ephemeral range,
//! away from the other test files' ranges); each test has its own block.

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures_util::{SinkExt, StreamExt};
use odhive_core::events::BusOptions;
use odhive_core::manager::ManagerOptions;
use odhive_core::ports::PortOwner;
use odhive_server::api::ws::ServerMessage;
use odhive_server::{ServiceOptions, WsLimits};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

mod common;
use common::{Api, Env, split};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const WAIT: Duration = Duration::from_secs(30);

/// Opens `/api/v1/ws` on `port`, with `credential` (a `Cookie` header, or
/// an `Authorization` header when it starts with `Bearer `), `origin` and
/// `host` headers when given.
async fn try_connect(
    port: u16,
    credential: Option<&str>,
    origin: Option<&str>,
    host: Option<&str>,
) -> Result<Ws, tungstenite::Error> {
    let mut req = format!("ws://127.0.0.1:{port}/api/v1/ws")
        .into_client_request()
        .expect("request");
    if let Some(c) = credential {
        let name = if c.starts_with("Bearer ") {
            header::AUTHORIZATION
        } else {
            header::COOKIE
        };
        req.headers_mut()
            .insert(name, c.parse().expect("credential"));
    }
    if let Some(o) = origin {
        req.headers_mut()
            .insert(header::ORIGIN, o.parse().expect("origin"));
    }
    if let Some(h) = host {
        req.headers_mut()
            .insert(header::HOST, h.parse().expect("host"));
    }
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(ws, _)| ws)
}

/// A connection as the signed-in web UI opens it; its hello is checked and
/// consumed.
async fn connect(api: &common::Api, port: u16) -> Ws {
    let origin = format!("http://localhost:{port}");
    let mut ws = try_connect(port, Some(&api.cookie), Some(&origin), None)
        .await
        .expect("connect");
    let hello = recv(&mut ws).await;
    assert_eq!(hello["type"], "hello", "{hello}");
    assert_eq!(hello["version"], env!("CARGO_PKG_VERSION"));
    ws
}

async fn send(ws: &mut Ws, msg: Value) {
    ws.send(Message::text(msg.to_string())).await.expect("send");
}

/// The next server message (or `{"type": "closed", ...}`).
async fn recv(ws: &mut Ws) -> Value {
    loop {
        let msg = tokio::time::timeout(WAIT, ws.next())
            .await
            .expect("timed out waiting for a message");
        match msg {
            Some(Ok(Message::Text(t))) => return serde_json::from_str(&t).expect("json"),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            Some(Ok(Message::Close(frame))) => {
                return json!({
                    "type": "closed",
                    "code": frame.as_ref().map(|f| u16::from(f.code)),
                    "reason": frame.as_ref().map(|f| f.reason.to_string()),
                });
            }
            Some(Ok(other)) => panic!("unexpected frame {other:?}"),
            Some(Err(e)) => return json!({ "type": "closed", "error": e.to_string() }),
            None => return json!({ "type": "closed" }),
        }
    }
}

/// Messages until one satisfies `done` (included).
async fn recv_until(ws: &mut Ws, what: &str, done: impl Fn(&Value) -> bool) -> Vec<Value> {
    let deadline = Instant::now() + WAIT;
    let mut out = Vec::new();
    loop {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        let msg = recv(ws).await;
        assert_ne!(
            msg["type"], "closed",
            "closed while waiting for {what}: {msg}"
        );
        let stop = done(&msg);
        out.push(msg);
        if stop {
            return out;
        }
    }
}

fn has_message(msg: &Value, needle: &str) -> bool {
    msg["type"] == "logs"
        && msg["records"]
            .as_array()
            .is_some_and(|r| r.iter().any(|r| r["message"] == needle))
}

fn seqs(msgs: &[Value]) -> Vec<u64> {
    msgs.iter()
        .filter(|m| m["type"] == "logs")
        .flat_map(|m| m["records"].as_array().expect("records").clone())
        .map(|r| r["seq"].as_u64().expect("seq"))
        .collect()
}

/// A served app with project `name` (instance `main` on `http`/`http+1`,
/// fake `odoo-bin` run with `args`).
async fn setup(
    env: &Env,
    name: &str,
    http: u16,
    args: &[&str],
    manager: ManagerOptions,
    ws: WsLimits,
) -> (Api, u16) {
    let (api, port) = env
        .serve_with(
            manager,
            ServiceOptions {
                ws,
                ..ServiceOptions::default()
            },
        )
        .await;
    let source = api.add_source(&env.source_dir()).await;
    api.create_project(name, &source).await;
    env.make_runnable_with(name, http, http + 1, args);
    (api, port)
}

async fn start(api: &Api, name: &str) {
    let (status, body) = api
        .post(
            &format!("/api/v1/projects/{name}/instances/main/start"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn stop(api: &Api, name: &str) {
    let (status, body) = api
        .post(
            &format!("/api/v1/projects/{name}/instances/main/stop"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

fn subscribe(name: &str) -> Value {
    json!({ "op": "subscribe_logs", "project": name, "instance": "main" })
}

#[tokio::test]
async fn bad_messages_get_errors_and_the_connection_stays() {
    let env = Env::new();
    let (api, port) = env
        .serve_with(ManagerOptions::default(), ServiceOptions::default())
        .await;
    let mut ws = connect(&api, port).await;

    ws.send(Message::text("not json")).await.expect("send");
    let m = recv(&mut ws).await;
    assert_eq!(
        (&m["type"], &m["error"]),
        (&json!("error"), &json!("bad_message")),
        "{m}"
    );

    send(
        &mut ws,
        json!({ "op": "subscribe_logs", "project": "x", "instance": "main", "extra": 1 }),
    )
    .await;
    assert_eq!(recv(&mut ws).await["error"], "bad_message");

    ws.send(Message::binary(vec![1, 2, 3])).await.expect("send");
    assert_eq!(recv(&mut ws).await["error"], "bad_message");

    send(&mut ws, subscribe("nope")).await;
    let m = recv(&mut ws).await;
    assert_eq!(m["error"], "unknown_project", "{m}");

    send(
        &mut ws,
        json!({ "op": "unsubscribe_logs", "project": "nope", "instance": "main" }),
    )
    .await;
    assert_eq!(recv(&mut ws).await["error"], "not_subscribed");

    // Still open.
    send(&mut ws, json!({ "op": "subscribe_events" })).await;
    let m = recv(&mut ws).await;
    assert_eq!(m["type"], "snapshot", "{m}");
    assert_eq!(m["instances"], json!([]));
}

#[tokio::test]
async fn oversized_client_messages_end_the_connection() {
    let env = Env::new();
    let limits = WsLimits {
        max_message_size: 1024,
        ..WsLimits::default()
    };
    let (api, port) = env
        .serve_with(
            ManagerOptions::default(),
            ServiceOptions {
                ws: limits,
                ..ServiceOptions::default()
            },
        )
        .await;
    let mut ws = connect(&api, port).await;
    let big = json!({ "op": "subscribe_logs", "project": "x".repeat(2000), "instance": "main" });
    send(&mut ws, big).await;
    let m = recv(&mut ws).await;
    assert_eq!(m["type"], "closed", "{m}");
}

#[tokio::test]
async fn foreign_origins_and_hosts_are_refused() {
    let env = Env::new();
    let (api, port) = env
        .serve_with(ManagerOptions::default(), ServiceOptions::default())
        .await;
    let refused = |r: Result<Ws, tungstenite::Error>| match r {
        Err(tungstenite::Error::Http(resp)) => resp.status(),
        Err(e) => panic!("unexpected error {e}"),
        Ok(_) => panic!("connected"),
    };
    for origin in [
        "http://evil.com".to_owned(),
        format!("http://evil.com:{port}"),
        format!("https://localhost:{port}"),
        "http://localhost:1".to_owned(),
        "null".to_owned(),
    ] {
        let status = refused(try_connect(port, Some(&api.cookie), Some(&origin), None).await);
        assert_eq!(status, StatusCode::FORBIDDEN, "{origin}");
    }
    let status = refused(
        try_connect(
            port,
            Some(&api.cookie),
            None,
            Some(&format!("evil.com:{port}")),
        )
        .await,
    );
    assert_eq!(status, StatusCode::FORBIDDEN, "foreign Host");

    // A browser always sends Origin: a cookie without one is refused.
    let status = refused(try_connect(port, Some(&api.cookie), None, None).await);
    assert_eq!(status, StatusCode::FORBIDDEN, "cookie without Origin");

    // The UI's own origins and non-browser clients (bearer, no Origin) get in.
    for (credential, origin) in [
        (&api.cookie, Some(format!("http://localhost:{port}"))),
        (&api.cookie, Some(format!("http://127.0.0.1:{port}"))),
        (&api.bearer, None),
    ] {
        let mut ws = try_connect(port, Some(credential), origin.as_deref(), None)
            .await
            .expect("connect");
        assert_eq!(recv(&mut ws).await["type"], "hello");
    }
}

#[tokio::test]
async fn a_plain_get_is_not_an_upgrade() {
    let env = Env::new();
    let api = env.app();
    let (status, body) = api.get("/api/v1/ws").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "not_websocket");

    // The Origin check also covers upgrades the router never sees.
    let req = Request::get("/api/v1/ws")
        .header(header::HOST, common::HOST)
        .header(header::UPGRADE, "websocket")
        .header(header::ORIGIN, "http://evil.com")
        .body(Body::empty())
        .expect("request");
    let (status, body) = split(api.send(req).await).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "forbidden_origin");
}

#[tokio::test]
async fn logs_arrive_in_order_in_batches_even_when_the_bus_lags() {
    let env = Env::new();
    // A tiny bus: the connection lags on it all the time.
    let manager = ManagerOptions {
        bus: BusOptions {
            log_capacity: 4,
            ..BusOptions::default()
        },
        ..ManagerOptions::default()
    };
    let (api, port) = setup(
        &env,
        "shop",
        19_200,
        &["--flood", "3000"],
        manager,
        WsLimits::default(),
    )
    .await;
    let mut ws = connect(&api, port).await;

    // Subscribed before it runs: everything from its first entry.
    send(&mut ws, subscribe("shop")).await;
    let m = recv(&mut ws).await;
    assert_eq!(m["type"], "subscribed", "{m}");
    assert_eq!((&m["next"], &m["running"]), (&json!(1), &json!(false)));

    start(&api, "shop").await;
    let msgs = recv_until(&mut ws, "the last flood line", |m| {
        has_message(m, "flood line 3000")
    })
    .await;
    assert!(
        msgs.iter().all(|m| m["type"] == "logs"),
        "only logs: no gap or reset"
    );
    let seqs = seqs(&msgs);
    assert_eq!(
        seqs,
        (1..=seqs.len() as u64).collect::<Vec<_>>(),
        "contiguous from 1, no duplicates"
    );
    assert!(seqs.len() > 3000);
    let frames = msgs.len();
    assert!(
        frames < seqs.len() / 50,
        "batched: {frames} frames for {} entries",
        seqs.len()
    );
    for m in &msgs {
        let n = m["records"].as_array().expect("records").len();
        assert!((1..=500).contains(&n), "{n} entries in one frame");
        assert_eq!(
            m["next"],
            m["records"][n - 1]["seq"].as_u64().expect("seq") + 1
        );
    }
    stop(&api, "shop").await;
}

#[tokio::test]
async fn subscribing_with_from_or_tail_and_unsubscribing() {
    let env = Env::new();
    let limits = WsLimits {
        max_subscriptions: 1,
        ..WsLimits::default()
    };
    let (api, port) = setup(&env, "shop", 19_210, &[], ManagerOptions::default(), limits).await;
    let (_, sources) = api.get("/api/v1/sources").await;
    let source = sources[0]["id"].as_str().expect("id").to_owned();
    api.create_project("other", &source).await;
    start(&api, "shop").await;
    let owner = PortOwner::new("shop", "main");
    let deadline = Instant::now() + WAIT;
    while !api.service.manager().tail_log(&owner, 1).is_some_and(|t| {
        t.first()
            .is_some_and(|r| r.entry.message.starts_with("Registry"))
    }) {
        assert!(Instant::now() < deadline, "no registry line");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let live = api.service.manager().log_next_seq(&owner).expect("log");
    let mut ws = connect(&api, port).await;

    send(
        &mut ws,
        json!({ "op": "subscribe_logs", "project": "shop", "instance": "main", "tail": 2 }),
    )
    .await;
    let m = recv(&mut ws).await;
    assert_eq!(
        (&m["type"], &m["next"], &m["running"]),
        (&json!("subscribed"), &json!(live - 2), &json!(true)),
        "{m}"
    );
    let m = recv(&mut ws).await;
    assert_eq!(seqs(&[m]), [live - 2, live - 1]);

    // Again with `from` replaces the subscription (still one).
    send(
        &mut ws,
        json!({ "op": "subscribe_logs", "project": "shop", "instance": "main", "from": 1 }),
    )
    .await;
    assert_eq!(recv(&mut ws).await["next"], 1);
    let msgs = recv_until(&mut ws, "the registry line", |m| {
        has_message(m, "Registry loaded in 0.734s")
    })
    .await;
    assert_eq!(seqs(&msgs), (1..live).collect::<Vec<_>>());

    send(&mut ws, subscribe("other")).await;
    assert_eq!(recv(&mut ws).await["error"], "too_many_subscriptions");
    send(&mut ws, json!({ "op": "subscribe_logs", "project": "shop", "instance": "main", "from": 1, "tail": 1 })).await;
    assert_eq!(recv(&mut ws).await["error"], "bad_message");
    send(
        &mut ws,
        json!({ "op": "subscribe_logs", "project": "shop", "instance": "main", "tail": 5001 }),
    )
    .await;
    assert_eq!(recv(&mut ws).await["error"], "bad_message");
    send(
        &mut ws,
        json!({ "op": "subscribe_logs", "project": "shop", "instance": "nope" }),
    )
    .await;
    assert_eq!(recv(&mut ws).await["error"], "unknown_instance");

    send(
        &mut ws,
        json!({ "op": "unsubscribe_logs", "project": "shop", "instance": "main" }),
    )
    .await;
    let m = recv(&mut ws).await;
    assert_eq!(
        (&m["type"], &m["project"]),
        (&json!("unsubscribed"), &json!("shop")),
        "{m}"
    );
    // A free slot again.
    send(&mut ws, subscribe("other")).await;
    let m = recv(&mut ws).await;
    assert_eq!(
        (&m["type"], &m["running"]),
        (&json!("subscribed"), &json!(false)),
        "{m}"
    );

    // No more entries of shop arrive after unsubscribing.
    stop(&api, "shop").await;
    send(&mut ws, json!({ "op": "subscribe_events" })).await;
    let m = recv(&mut ws).await;
    assert_eq!(m["type"], "snapshot", "nothing queued before it: {m}");
}

#[tokio::test]
async fn a_new_log_after_a_stop_is_announced_with_a_reset() {
    let env = Env::new();
    let (api, port) = setup(
        &env,
        "shop",
        19_220,
        &[],
        ManagerOptions::default(),
        WsLimits::default(),
    )
    .await;
    let mut ws = connect(&api, port).await;
    send(&mut ws, subscribe("shop")).await;
    assert_eq!(recv(&mut ws).await["type"], "subscribed");

    start(&api, "shop").await;
    let first = recv_until(&mut ws, "the registry line", |m| {
        has_message(m, "Registry loaded in 0.734s")
    })
    .await;
    let first = seqs(&first);
    stop(&api, "shop").await;
    // The same number of entries again: only the log id tells them apart.
    start(&api, "shop").await;
    let msgs = recv_until(&mut ws, "the second registry line", |m| {
        has_message(m, "Registry loaded in 0.734s")
    })
    .await;
    let resets: Vec<usize> = msgs
        .iter()
        .enumerate()
        .filter(|(_, m)| m["type"] == "reset")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(resets.len(), 1, "{msgs:?}");
    let after = seqs(&msgs[resets[0]..]);
    assert_eq!(after.first(), Some(&1), "starts over at 1");
    assert_eq!(after.len(), first.len(), "{after:?} vs {first:?}");
    stop(&api, "shop").await;
}

#[tokio::test]
async fn the_last_entries_of_a_stopped_instance_still_arrive() {
    let env = Env::new();
    let (api, port) = setup(
        &env,
        "shop",
        19_270,
        &[],
        ManagerOptions::default(),
        WsLimits::default(),
    )
    .await;
    let mut ws = connect(&api, port).await;
    send(&mut ws, subscribe("shop")).await;
    assert_eq!(recv(&mut ws).await["type"], "subscribed");
    send(&mut ws, json!({ "op": "subscribe_events" })).await;
    start(&api, "shop").await;
    let mut all = recv_until(&mut ws, "the registry line", |m| {
        has_message(m, "Registry loaded in 0.734s")
    })
    .await;
    // Stopping removes the instance, and its log, from the manager before
    // the batch with the shutdown lines is due.
    stop(&api, "shop").await;
    all.extend(
        recv_until(&mut ws, "the shutdown line", |m| {
            has_message(m, "Initiating shutdown")
        })
        .await,
    );

    // The next run's log starts over and is announced.
    start(&api, "shop").await;
    let msgs = recv_until(&mut ws, "the registry line", |m| {
        has_message(m, "Registry loaded in 0.734s")
    })
    .await;
    assert_eq!(
        msgs.iter().filter(|m| m["type"] == "reset").count(),
        1,
        "{msgs:?}"
    );
    stop(&api, "shop").await;

    // Clients (the CLI) read the messages back into the same types.
    all.extend(msgs);
    let mut seen_records = 0;
    let kinds: std::collections::BTreeSet<String> = all
        .iter()
        .map(|m| m["type"].as_str().unwrap_or_default().to_owned())
        .collect();
    for msg in all {
        let back: ServerMessage = serde_json::from_value(msg.clone()).expect("decode");
        if let ServerMessage::Logs { records, .. } = &back {
            seen_records += records.len();
        }
        assert_eq!(serde_json::to_value(&back).expect("encode"), msg);
    }
    assert!(seen_records > 10, "{seen_records}");
    assert!(
        kinds.contains("snapshot") && kinds.contains("events"),
        "{kinds:?}"
    );
}

#[tokio::test]
async fn instance_events_follow_a_snapshot() {
    let env = Env::new();
    let (api, port) = setup(
        &env,
        "shop",
        19_230,
        &[],
        ManagerOptions::default(),
        WsLimits::default(),
    )
    .await;
    let mut ws = connect(&api, port).await;
    send(&mut ws, json!({ "op": "subscribe_events" })).await;
    assert_eq!(recv(&mut ws).await["type"], "snapshot");

    start(&api, "shop").await;
    let msgs = recv_until(&mut ws, "running", |m| {
        m["type"] == "events"
            && m["events"]
                .as_array()
                .is_some_and(|e| e.iter().any(|e| e["status"]["state"] == "running"))
    })
    .await;
    let events: Vec<&Value> = msgs
        .iter()
        .flat_map(|m| m["events"].as_array().expect("events").iter())
        .collect();
    let spawned = events
        .iter()
        .find(|e| e["event"] == "spawned")
        .expect("spawned");
    assert_eq!(spawned["project"], "shop");
    assert_eq!(spawned["instance"], "main");
    assert_eq!(spawned["http_port"], 19_230);

    send(&mut ws, json!({ "op": "unsubscribe_events" })).await;
    stop(&api, "shop").await;
    send(&mut ws, subscribe("shop")).await;
    assert_eq!(
        recv(&mut ws).await["type"],
        "subscribed",
        "no events after unsubscribing"
    );
}

#[tokio::test]
async fn too_many_queued_events_become_a_snapshot() {
    let env = Env::new();
    let limits = WsLimits {
        batch_delay: Duration::from_millis(500),
        max_pending_events: 1,
        ..WsLimits::default()
    };
    let (api, port) = setup(&env, "shop", 19_240, &[], ManagerOptions::default(), limits).await;
    let mut ws = connect(&api, port).await;
    send(&mut ws, json!({ "op": "subscribe_events" })).await;
    assert_eq!(recv(&mut ws).await["instances"], json!([]));

    // Spawned and starting (and more) arrive within one batch delay.
    start(&api, "shop").await;
    let m = recv(&mut ws).await;
    assert_eq!(m["type"], "snapshot", "{m}");
    let instances = m["instances"].as_array().expect("instances");
    assert_eq!(instances.len(), 1);
    assert_eq!(instances[0]["project"], "shop");
    stop(&api, "shop").await;
}

#[tokio::test]
async fn shutdown_closes_connections() {
    let env = Env::new();
    let (api, port) = env
        .serve_with(ManagerOptions::default(), ServiceOptions::default())
        .await;
    let mut ws = connect(&api, port).await;
    api.service.begin_shutdown();
    let m = recv(&mut ws).await;
    assert_eq!(
        (&m["type"], &m["code"]),
        (&json!("closed"), &json!(1001)),
        "{m}"
    );
    // New connections are closed at once.
    let mut ws = connect(&api, port).await;
    assert_eq!(recv(&mut ws).await["code"], 1001);
}

#[tokio::test]
async fn a_client_that_stops_reading_is_dropped() {
    let env = Env::new();
    let limits = WsLimits {
        send_timeout: Duration::from_millis(300),
        ..WsLimits::default()
    };
    let flood = 100_000;
    let (api, port) = setup(
        &env,
        "shop",
        19_250,
        &["--flood", &flood.to_string()],
        ManagerOptions::default(),
        limits,
    )
    .await;
    let mut ws = connect(&api, port).await;
    send(&mut ws, subscribe("shop")).await;
    start(&api, "shop").await;

    // Read nothing until the whole flood is logged, and a bit longer.
    let owner = PortOwner::new("shop", "main");
    let deadline = Instant::now() + Duration::from_secs(60);
    while api.service.manager().log_next_seq(&owner).unwrap_or(0) < flood {
        assert!(Instant::now() < deadline, "flood not logged in time");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;

    // What was buffered arrives, then the connection ends: not every entry.
    let mut entries = 0;
    let end = loop {
        let m = recv(&mut ws).await;
        if m["type"] == "closed" {
            break m;
        }
        if m["type"] == "logs" {
            entries += m["records"].as_array().expect("records").len();
        }
    };
    assert!(
        entries < flood as usize,
        "dropped before the end ({entries} entries): {end}"
    );
    stop(&api, "shop").await;
}

#[tokio::test]
async fn job_progress_is_streamed_to_job_subscribers() {
    let env = Env::new();
    let (api, port) = env
        .serve_with(
            ManagerOptions::default(),
            common::job_options("file:///none"),
        )
        .await;
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;
    let mut ws = connect(&api, port).await;

    send(&mut ws, json!({ "op": "subscribe_jobs" })).await;
    let snap = recv(&mut ws).await;
    assert_eq!(snap["type"], "job_snapshot", "{snap}");
    assert_eq!(snap["jobs"], json!([]));

    let python = common::python3();
    let (status, job) = api
        .post(
            "/api/v1/projects/shop/setup",
            Some(json!({ "python": python.to_str().expect("utf8"), "skip_requirements": true })),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let id = job["id"].clone();

    let msgs = recv_until(&mut ws, "the job to finish", |m| {
        m["type"] == "job_events"
            && m["events"]
                .as_array()
                .is_some_and(|e| e.iter().any(|e| e["event"] == "finished"))
    })
    .await;
    let events: Vec<&Value> = msgs
        .iter()
        .flat_map(|m| m["events"].as_array().expect("events"))
        .collect();
    assert!(events.iter().all(|e| e["id"] == id || e["job"]["id"] == id));
    assert_eq!(events.first().map(|e| &e["event"]), Some(&json!("started")));
    assert!(
        events.iter().any(|e| e["event"] == "step"
            && e["step"]["job"] == "setup"
            && e["step"]["step"] == "python"),
        "{events:?}"
    );
    let last = events.last().expect("last");
    assert_eq!(last["event"], "finished");
    assert!(
        matches!(last["job"]["state"].as_str(), Some("succeeded" | "failed")),
        "{last}"
    );

    // Unsubscribed: the next job is not sent.
    send(&mut ws, json!({ "op": "unsubscribe_jobs" })).await;
    send(&mut ws, json!({ "op": "subscribe_events" })).await;
    let snap = recv(&mut ws).await;
    assert_eq!(snap["type"], "snapshot", "{snap}");
}
