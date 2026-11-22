//! WebSocket `/ws`: live instance logs and events.
//!
//! The client sends JSON [`ClientMessage`]s to choose what it wants; the
//! server answers with JSON [`ServerMessage`]s, batched: entries and events
//! collected for [`WsLimits::batch_delay`] (or until
//! [`WsLimits::batch_entries`] are waiting) go out in one frame.
//!
//! Log entries are read from the instance's buffer at the client's cursor;
//! the event bus only wakes the connection up. A connection that falls
//! behind therefore loses nothing the buffer still holds, and is told with
//! [`ServerMessage::Gap`] about entries that were dropped from memory before
//! it read them. Instance events are queued per connection up to
//! [`WsLimits::max_pending_events`]; beyond that, or when the connection
//! lags on the event bus, the queue is replaced by a
//! [`ServerMessage::Snapshot`] of every instance.
//!
//! A client that does not take a frame within [`WsLimits::send_timeout`] is
//! dropped (with a best-effort close frame, code 1008).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade, close_code};
use axum::response::Response;
use odhive_core::events::{Delivery, LogEvent, Subscription};
use odhive_core::logs::store::Record;
use odhive_core::manager::{InstanceInfo, ManagerEvent};
use odhive_core::ports::PortOwner;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::time::{Instant, sleep_until, timeout};
use utoipa::ToSchema;

use super::error::{ApiError, ErrorBody};
use super::instances::MAX_LOG_LIMIT;
use crate::service::Service;

/// Limits of each WebSocket connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WsLimits {
    /// How long entries and events are collected before they are sent.
    pub batch_delay: Duration,
    /// Entries per [`ServerMessage::Logs`] frame; this many waiting entries
    /// are sent without waiting for [`WsLimits::batch_delay`].
    pub batch_entries: usize,
    /// Log frames sent per instance before the connection looks at the
    /// client's messages again.
    pub frames_per_flush: usize,
    /// Instance events queued before they are replaced by a snapshot.
    pub max_pending_events: usize,
    /// Log subscriptions per connection.
    pub max_subscriptions: usize,
    /// Largest client message, in bytes.
    pub max_message_size: usize,
    /// A frame the client does not take within this time drops it.
    pub send_timeout: Duration,
}

impl Default for WsLimits {
    fn default() -> Self {
        Self {
            batch_delay: Duration::from_millis(50),
            batch_entries: 500,
            frames_per_flush: 8,
            max_pending_events: 1000,
            max_subscriptions: 32,
            max_message_size: 16 * 1024,
            send_timeout: Duration::from_secs(10),
        }
    }
}

/// A message from the client.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientMessage {
    /// Follow the log of one instance. Answered with
    /// [`ServerMessage::Subscribed`]. Without `from` or `tail` only new
    /// entries are sent. The instance does not have to run: its entries
    /// arrive once it does.
    SubscribeLogs {
        /// Project name.
        project: String,
        /// Instance id.
        instance: String,
        /// First sequence number wanted, e.g. `next` of a REST log page.
        #[serde(default)]
        from: Option<u64>,
        /// Start with up to this many of the newest entries (at most 5000).
        #[serde(default)]
        tail: Option<usize>,
    },
    /// Stop following the log of one instance.
    UnsubscribeLogs {
        /// Project name.
        project: String,
        /// Instance id.
        instance: String,
    },
    /// Receive every instance's events. Answered with a
    /// [`ServerMessage::Snapshot`].
    SubscribeEvents,
    /// Stop receiving instance events.
    UnsubscribeEvents,
}

/// A message from the server.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// First message of every connection.
    Hello {
        /// OdHive version.
        version: String,
    },
    /// A log subscription is in place.
    Subscribed {
        /// Project name.
        project: String,
        /// Instance id.
        instance: String,
        /// Sequence number of the first entry that will be sent.
        next: u64,
        /// This service runs the instance (it has a log in memory).
        running: bool,
    },
    /// A log subscription ended.
    Unsubscribed {
        /// Project name.
        project: String,
        /// Instance id.
        instance: String,
    },
    /// New entries of one instance's log, in order.
    Logs {
        /// Project name.
        project: String,
        /// Instance id.
        instance: String,
        /// The entries.
        records: Vec<Arc<Record>>,
        /// Sequence number of the next entry.
        next: u64,
    },
    /// Entries were dropped from memory before the client got them (the
    /// log file still has them).
    Gap {
        /// Project name.
        project: String,
        /// Instance id.
        instance: String,
        /// Entries lost.
        skipped: u64,
    },
    /// The instance's log started over (it was stopped and started again
    /// with a new log); sequence numbers begin again at 1.
    Reset {
        /// Project name.
        project: String,
        /// Instance id.
        instance: String,
    },
    /// Instance events, oldest first.
    Events {
        /// The events.
        events: Vec<ManagerEvent>,
    },
    /// Every instance as it is now. Sent on [`ClientMessage::SubscribeEvents`]
    /// and in place of events the connection could not keep up with.
    Snapshot {
        /// The instances.
        instances: Vec<InstanceInfo>,
    },
    /// A client message was refused; the connection stays open.
    Error(ErrorBody),
}

/// Opens the WebSocket. The connection first gets a
/// [`ServerMessage::Hello`]; see [`ClientMessage`] and [`ServerMessage`]
/// for the protocol.
#[utoipa::path(operation_id = "websocket", get, path = "/ws", tag = "service",
    responses(
        (status = SWITCHING_PROTOCOLS, description = "WebSocket opened; messages are ServerMessage (from the server) and ClientMessage (from the client)"),
        (status = BAD_REQUEST, description = "Not a WebSocket upgrade request"),
        (status = FORBIDDEN, description = "Foreign Origin", body = ErrorBody),
    ))]
pub(super) async fn upgrade(
    State(svc): State<Service>,
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Result<Response, ApiError> {
    let ws = ws.map_err(|e| ApiError::bad_request("not_websocket", e.body_text()))?;
    let limits = svc.ws_limits();
    Ok(ws
        .max_message_size(limits.max_message_size)
        .max_frame_size(limits.max_message_size)
        .on_upgrade(move |socket| async move {
            Connection::new(svc, socket).run().await;
        }))
}

/// Where a log subscription stands.
#[derive(Debug)]
struct Cursor {
    /// The log `next` belongs to; `None` until one was seen.
    log: Option<u64>,
    /// Sequence number of the next entry to send.
    next: u64,
    /// There may be entries at `next` to send.
    dirty: bool,
}

/// The connection is over.
struct Closed;

struct Connection {
    svc: Service,
    limits: WsLimits,
    socket: WebSocket,
    shutdown: watch::Receiver<bool>,
    logs: Subscription<LogEvent>,
    /// The log bus still has publishers.
    logs_open: bool,
    cursors: BTreeMap<PortOwner, Cursor>,
    /// Log events seen since the last flush for subscribed instances.
    unread: usize,
    events: Option<Subscription<ManagerEvent>>,
    pending: Vec<ManagerEvent>,
    snapshot: bool,
    flush_at: Option<Instant>,
}

impl Connection {
    fn new(svc: Service, socket: WebSocket) -> Self {
        Self {
            limits: svc.ws_limits(),
            shutdown: svc.shutdown_signal(),
            logs: svc.manager().subscribe_logs(),
            logs_open: true,
            svc,
            socket,
            cursors: BTreeMap::new(),
            unread: 0,
            events: None,
            pending: Vec::new(),
            snapshot: false,
            flush_at: None,
        }
    }

    async fn run(mut self) {
        let hello = ServerMessage::Hello {
            version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        if self.send(&hello).await.is_ok() {
            let close = self.serve().await;
            if let Some((code, reason)) = close {
                self.close(code, reason).await;
            }
        }
    }

    /// Runs until the client leaves (`None`) or the server ends the
    /// connection (the close frame to send).
    async fn serve(&mut self) -> Option<(u16, &'static str)> {
        loop {
            if *self.shutdown.borrow() {
                return Some((close_code::AWAY, "OdHive is shutting down"));
            }
            let flush_at = self.flush_at;
            tokio::select! {
                biased;
                _ = self.shutdown.changed() => {
                    return Some((close_code::AWAY, "OdHive is shutting down"));
                }
                msg = self.socket.recv() => match msg {
                    Some(Ok(Message::Text(text))) => {
                        if self.handle(&text).await.is_err() {
                            return None;
                        }
                    }
                    Some(Ok(Message::Binary(_))) => {
                        let err = error("bad_message", "messages must be JSON text");
                        if self.send(&err).await.is_err() {
                            return None;
                        }
                    }
                    // Pings are answered by the socket itself.
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_)) | Err(_)) | None => return None,
                },
                () = wait_until(flush_at) => {
                    self.flush_at = None;
                    if self.flush().await.is_err() {
                        return None;
                    }
                }
                d = self.logs.recv(), if self.logs_open => {
                    self.on_log(d);
                }
                d = next_event(self.events.as_mut()) => {
                    self.on_event(d);
                }
            }
            if self.unread >= self.limits.batch_entries {
                self.flush_at = Some(Instant::now());
            }
        }
    }

    fn on_log(&mut self, d: Option<Delivery<LogEvent>>) {
        match d {
            Some(Delivery::Event(e)) => {
                if let Some(c) = self.cursors.get_mut(&e.owner) {
                    c.dirty = true;
                    self.unread += 1;
                    self.schedule();
                }
            }
            // Missed wake-ups: the buffers have the entries.
            Some(Delivery::Lagged { .. }) => {
                for c in self.cursors.values_mut() {
                    c.dirty = true;
                }
                self.unread = self.limits.batch_entries;
            }
            // The manager is gone; nothing more will be logged.
            None => self.logs_open = false,
        }
    }

    fn on_event(&mut self, d: Option<Delivery<ManagerEvent>>) {
        match d {
            Some(Delivery::Event(e)) if !self.snapshot => {
                if self.pending.len() >= self.limits.max_pending_events {
                    self.pending.clear();
                    self.snapshot = true;
                } else {
                    self.pending.push(e);
                }
            }
            Some(Delivery::Event(_)) => {}
            Some(Delivery::Lagged { .. }) => {
                self.pending.clear();
                self.snapshot = true;
            }
            None => {
                self.events = None;
                return;
            }
        }
        self.schedule();
    }

    fn schedule(&mut self) {
        if self.flush_at.is_none() {
            self.flush_at = Some(Instant::now() + self.limits.batch_delay);
        }
    }

    async fn handle(&mut self, text: &str) -> Result<(), Closed> {
        let msg = match serde_json::from_str::<ClientMessage>(text) {
            Ok(m) => m,
            Err(e) => return self.send(&error("bad_message", e.to_string())).await,
        };
        match msg {
            ClientMessage::SubscribeLogs {
                project,
                instance,
                from,
                tail,
            } => {
                let reply = self.subscribe_logs(project, instance, from, tail);
                self.send(&reply).await
            }
            ClientMessage::UnsubscribeLogs { project, instance } => {
                let owner = PortOwner::new(project, instance);
                if self.cursors.remove(&owner).is_none() {
                    let msg = format!("not subscribed to the log of {owner}");
                    return self.send(&error("not_subscribed", msg)).await;
                }
                let reply = ServerMessage::Unsubscribed {
                    project: owner.project,
                    instance: owner.instance,
                };
                self.send(&reply).await
            }
            ClientMessage::SubscribeEvents => {
                // Subscribe first: events after the snapshot are not missed.
                self.events = Some(self.svc.manager().subscribe());
                self.pending.clear();
                self.snapshot = false;
                let instances = self.svc.manager().list();
                self.send(&ServerMessage::Snapshot { instances }).await
            }
            ClientMessage::UnsubscribeEvents => {
                self.events = None;
                self.pending.clear();
                self.snapshot = false;
                Ok(())
            }
        }
    }

    fn subscribe_logs(
        &mut self,
        project: String,
        instance: String,
        from: Option<u64>,
        tail: Option<usize>,
    ) -> ServerMessage {
        if from.is_some() && tail.is_some() {
            return error("bad_message", "give either from or tail, not both");
        }
        if tail.is_some_and(|n| n > MAX_LOG_LIMIT) {
            return error(
                "bad_message",
                format!("tail must be at most {MAX_LOG_LIMIT}"),
            );
        }
        if let Err(e) = super::instances::instance(&self.svc, &project, &instance) {
            return ServerMessage::Error(e.body());
        }
        let owner = PortOwner::new(project, instance);
        if !self.cursors.contains_key(&owner) && self.cursors.len() >= self.limits.max_subscriptions
        {
            let max = self.limits.max_subscriptions;
            return error(
                "too_many_subscriptions",
                format!("at most {max} log subscriptions per connection"),
            );
        }
        let position = self.svc.manager().log_position(&owner);
        let live = position.map(|(_, next)| next);
        let next = match (from, tail, live) {
            (Some(from), _, _) => from.max(1),
            (None, Some(n), Some(live)) => live.saturating_sub(n as u64).max(1),
            (None, None, Some(live)) => live,
            // Not running here: everything its next log holds.
            (None, _, None) => 1,
        };
        let cursor = Cursor {
            log: position.map(|(id, _)| id),
            next,
            dirty: true,
        };
        self.cursors.insert(owner.clone(), cursor);
        self.schedule();
        ServerMessage::Subscribed {
            project: owner.project,
            instance: owner.instance,
            next,
            running: position.is_some(),
        }
    }

    /// Sends what is waiting: the snapshot or queued events, then each
    /// subscribed log from its cursor.
    async fn flush(&mut self) -> Result<(), Closed> {
        self.unread = 0;
        if self.snapshot {
            self.snapshot = false;
            let instances = self.svc.manager().list();
            self.send(&ServerMessage::Snapshot { instances }).await?;
        } else if !self.pending.is_empty() {
            let events = std::mem::take(&mut self.pending);
            self.send(&ServerMessage::Events { events }).await?;
        }
        let dirty: Vec<PortOwner> = self
            .cursors
            .iter()
            .filter(|(_, c)| c.dirty)
            .map(|(o, _)| o.clone())
            .collect();
        let mut more = false;
        for owner in dirty {
            more |= self.flush_log(&owner).await?;
        }
        if more {
            // Let client messages in before the rest.
            self.flush_at = Some(Instant::now());
        }
        Ok(())
    }

    /// Sends up to [`WsLimits::frames_per_flush`] frames of `owner`'s log;
    /// returns whether entries are left.
    async fn flush_log(&mut self, owner: &PortOwner) -> Result<bool, Closed> {
        let manager = self.svc.manager().clone();
        let mut frames = 0;
        loop {
            let Some(cursor) = self.cursors.get_mut(owner) else {
                return Ok(false);
            };
            let Some((log, page)) =
                manager.read_log_with_id(owner, cursor.next, self.limits.batch_entries)
            else {
                // Not running here (yet); its entries come once it runs.
                cursor.dirty = false;
                return Ok(false);
            };
            if cursor.log.is_some_and(|seen| seen != log) {
                // A new log: its sequence numbers start over.
                cursor.log = Some(log);
                cursor.next = 1;
                let reset = ServerMessage::Reset {
                    project: owner.project.clone(),
                    instance: owner.instance.clone(),
                };
                self.send(&reset).await?;
                continue;
            }
            cursor.log = Some(log);
            if page.records.is_empty() && page.skipped == 0 {
                cursor.dirty = false;
                return Ok(false);
            }
            cursor.next = page.next;
            if page.skipped > 0 {
                let gap = ServerMessage::Gap {
                    project: owner.project.clone(),
                    instance: owner.instance.clone(),
                    skipped: page.skipped,
                };
                self.send(&gap).await?;
            }
            if !page.records.is_empty() {
                let logs = ServerMessage::Logs {
                    project: owner.project.clone(),
                    instance: owner.instance.clone(),
                    records: page.records,
                    next: page.next,
                };
                self.send(&logs).await?;
                frames += 1;
            }
            if frames >= self.limits.frames_per_flush {
                return Ok(true);
            }
        }
    }

    async fn send(&mut self, msg: &ServerMessage) -> Result<(), Closed> {
        let text = match serde_json::to_string(msg) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("cannot encode a WebSocket message: {e}");
                return Err(Closed);
            }
        };
        match timeout(
            self.limits.send_timeout,
            self.socket.send(Message::Text(text.into())),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(Closed),
            Err(_) => {
                tracing::warn!("dropping a WebSocket client that does not keep up");
                self.close(close_code::POLICY, "client too slow").await;
                Err(Closed)
            }
        }
    }

    /// Sends a close frame if the client takes it within a second.
    async fn close(&mut self, code: u16, reason: &'static str) {
        let frame = CloseFrame {
            code,
            reason: Utf8Bytes::from_static(reason),
        };
        let _ = timeout(
            Duration::from_secs(1),
            self.socket.send(Message::Close(Some(frame))),
        )
        .await;
    }
}

fn error(code: &str, message: impl Into<String>) -> ServerMessage {
    ServerMessage::Error(ErrorBody {
        error: code.to_owned(),
        message: message.into(),
    })
}

async fn wait_until(at: Option<Instant>) {
    match at {
        Some(at) => sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn next_event(
    sub: Option<&mut Subscription<ManagerEvent>>,
) -> Option<Delivery<ManagerEvent>> {
    match sub {
        Some(sub) => sub.recv().await,
        None => std::future::pending().await,
    }
}
