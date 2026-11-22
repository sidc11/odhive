use std::sync::Arc;

use super::*;
use crate::logs::parse::parse_all;
use crate::logs::store::LogBuffer;

#[test]
fn publishing_without_subscribers_is_fine() {
    let topic = Topic::<u32>::new(4);
    assert_eq!(topic.publish(1), 0);
    assert_eq!(topic.subscribers(), 0);
}

#[test]
fn subscribers_get_events_published_after_they_subscribe() {
    let topic = Topic::new(4);
    topic.publish(1);
    let mut a = topic.subscribe();
    let mut b = topic.subscribe();
    assert_eq!(topic.publish(2), 2);
    assert_eq!(a.try_recv(), Ok(Delivery::Event(2)));
    assert_eq!(b.try_recv(), Ok(Delivery::Event(2)));
    assert_eq!(a.try_recv(), Err(Empty::Empty));
}

#[test]
fn zero_capacity_is_raised_to_one() {
    let topic = Topic::new(0);
    let mut rx = topic.subscribe();
    topic.publish("a");
    assert_eq!(rx.try_recv(), Ok(Delivery::Event("a")));
}

#[test]
fn a_lagging_subscriber_is_told_once_per_gap_and_continues() {
    let topic = Topic::new(4);
    let mut slow = topic.subscribe();
    let mut fast = topic.subscribe();
    for i in 0..10 {
        topic.publish(i);
        // The fast one keeps up and never lags.
        assert_eq!(fast.try_recv(), Ok(Delivery::Event(i)));
    }
    assert_eq!(slow.try_recv(), Ok(Delivery::Lagged { missed: 6 }));
    let rest: Vec<_> = std::iter::from_fn(|| slow.try_recv().ok()).collect();
    assert_eq!(
        rest,
        [6, 7, 8, 9].map(Delivery::Event).to_vec(),
        "continues with the oldest held event"
    );
    assert_eq!(slow.missed(), 6);
    assert_eq!(fast.missed(), 0);

    // A second gap is reported on its own.
    for i in 10..20 {
        topic.publish(i);
    }
    assert_eq!(slow.try_recv(), Ok(Delivery::Lagged { missed: 6 }));
    assert_eq!(slow.missed(), 12);
    assert_eq!(slow.drain(), vec![16, 17, 18, 19]);
}

#[test]
fn a_lagging_subscriber_never_blocks_the_publisher() {
    let topic = Topic::new(2);
    let _never_reads = topic.subscribe();
    for i in 0..100_000 {
        topic.publish(i);
    }
}

#[tokio::test]
async fn recv_reports_lag_and_closes_when_publishers_are_gone() {
    let topic = Topic::new(2);
    let mut rx = topic.subscribe();
    for i in 0..5 {
        topic.publish(i);
    }
    let copy = topic.clone();
    drop(topic);
    copy.publish(5);
    drop(copy);
    assert_eq!(rx.recv().await, Some(Delivery::Lagged { missed: 4 }));
    assert_eq!(rx.recv().await, Some(Delivery::Event(4)));
    assert_eq!(rx.recv().await, Some(Delivery::Event(5)));
    assert_eq!(rx.recv().await, None);
    assert_eq!(rx.try_recv(), Err(Empty::Closed));
}

#[tokio::test]
async fn next_event_skips_lag_notices_but_counts_them() {
    let topic = Topic::new(1);
    let mut rx = topic.subscribe();
    topic.publish(1);
    topic.publish(2);
    assert_eq!(rx.next_event().await, Some(2));
    assert_eq!(rx.missed(), 1);
    assert!(rx.is_empty());
}

#[test]
fn bus_topics_are_independent() {
    let bus = EventBus::new(BusOptions {
        log_capacity: 2,
        instance_capacity: 8,
    });
    let mut logs = bus.logs().subscribe();
    let mut instances = bus.instances().subscribe();
    let owner = PortOwner::new("shop", "main");
    let mut buffer = LogBuffer::default();
    let text: String = (0..4)
        .map(|i| format!("2026-11-22 06:27:41,016 4242 INFO ? odoo: line {i}\n"))
        .collect();
    let entries = parse_all(&text);
    assert_eq!(entries.len(), 4);
    for e in entries {
        let record = buffer.push(e);
        bus.logs().publish(LogEvent {
            owner: owner.clone(),
            record,
        });
    }
    // A log flood does not cost the instance topic anything.
    assert!(instances.is_empty());
    assert_eq!(logs.try_recv(), Ok(Delivery::Lagged { missed: 2 }));
    let seqs: Vec<u64> = logs.drain().iter().map(|e| e.record.seq).collect();
    assert_eq!(seqs, [3, 4]);
    assert!(instances.try_recv().is_err());
}

#[test]
fn clones_share_topics() {
    let bus = EventBus::default();
    let mut rx = bus.logs().subscribe();
    let other = bus.clone();
    let mut buffer = LogBuffer::default();
    let record = buffer.push(parse_all("hello").remove(0));
    other.logs().publish(LogEvent {
        owner: PortOwner::new("shop", "main"),
        record: Arc::clone(&record),
    });
    let Ok(Delivery::Event(e)) = rx.try_recv() else {
        panic!("expected the event");
    };
    assert!(Arc::ptr_eq(&e.record, &record), "shared, not copied");
}

#[test]
fn log_events_serialize_flat() {
    let mut buffer = LogBuffer::default();
    let record = buffer
        .push(parse_all("2026-11-22 06:27:41,016 4242 WARNING shop odoo.http: careful").remove(0));
    let json = serde_json::to_value(LogEvent {
        owner: PortOwner::new("shop", "main"),
        record,
    })
    .expect("json");
    assert_eq!(json["project"], "shop");
    assert_eq!(json["instance"], "main");
    assert_eq!(json["seq"], 1);
    assert_eq!(json["level"], "warning");
    assert_eq!(json["message"], "careful");
    assert_eq!(json["timestamp"], "2026-11-22T06:27:41.016Z");
}
