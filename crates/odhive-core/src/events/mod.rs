//! Event bus: how core work reaches the CLI, the server and the UI.
//!
//! An [`EventBus`] has one bounded [`Topic`] per kind of event, so a flood
//! of one kind cannot push out another:
//!
//! - **logs** ([`LogEvent`]): every parsed log entry of every instance. High
//!   volume; capacity [`DEFAULT_LOG_CAPACITY`].
//! - **instances** ([`ManagerEvent`]): spawns, status changes and manager
//!   notices. Low volume; capacity [`DEFAULT_INSTANCE_CAPACITY`].
//!
//! Publishing never blocks and never fails: with no subscriber the event is
//! dropped. Each subscriber has its own window of `capacity` events. One that
//! falls further behind loses the oldest ones and is told so once per gap
//! with [`Delivery::Lagged`] before it continues with the oldest event still
//! held. A slow subscriber never slows the publisher or other subscribers.
//!
//! Lag is recoverable: log entries carry sequence numbers, so a log reader
//! re-reads the gap from the instance's buffer
//! ([`crate::manager::Manager::read_log`]); an instance reader refreshes its
//! view from [`crate::manager::Manager::list`].

use std::sync::Arc;

use serde::Serialize;
use tokio::sync::broadcast::{
    self,
    error::{RecvError, TryRecvError},
};

use crate::logs::store::Record;
use crate::manager::ManagerEvent;
use crate::ports::PortOwner;

#[cfg(test)]
mod tests;

/// Log events held per subscriber by default.
pub const DEFAULT_LOG_CAPACITY: usize = 16_384;

/// Instance events held per subscriber by default.
pub const DEFAULT_INSTANCE_CAPACITY: usize = 1024;

/// One bounded broadcast channel. Cheap to clone; clones publish to the same
/// subscribers.
#[derive(Debug)]
pub struct Topic<T> {
    tx: broadcast::Sender<T>,
}

impl<T> Clone for Topic<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

impl<T: Clone> Topic<T> {
    /// A topic holding up to `capacity` events per subscriber (at least 1,
    /// rounded up to a power of two).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
        Self { tx }
    }

    /// Sends `event` to every current subscriber; returns how many there
    /// are. Never blocks.
    pub fn publish(&self, event: T) -> usize {
        // No subscriber is fine: nobody is listening.
        self.tx.send(event).unwrap_or(0)
    }

    /// A subscription to the events published from now on.
    #[must_use]
    pub fn subscribe(&self) -> Subscription<T> {
        Subscription {
            rx: self.tx.subscribe(),
            missed: 0,
        }
    }

    /// Current subscribers.
    #[must_use]
    pub fn subscribers(&self) -> usize {
        self.tx.receiver_count()
    }
}

/// What a [`Subscription`] receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery<T> {
    /// The next event.
    Event(T),
    /// The subscriber fell behind and `missed` events were dropped for it;
    /// the next delivery is the oldest event still held.
    Lagged {
        /// Events lost in this gap.
        missed: u64,
    },
}

/// Why [`Subscription::try_recv`] returned nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Empty {
    /// Nothing is waiting right now.
    Empty,
    /// Every publisher is gone and everything was received.
    Closed,
}

/// One subscriber's view of a [`Topic`].
#[derive(Debug)]
pub struct Subscription<T> {
    rx: broadcast::Receiver<T>,
    missed: u64,
}

impl<T: Clone> Subscription<T> {
    /// The next event or lag notice; `None` once every publisher is gone and
    /// everything was received.
    pub async fn recv(&mut self) -> Option<Delivery<T>> {
        match self.rx.recv().await {
            Ok(event) => Some(Delivery::Event(event)),
            Err(RecvError::Lagged(n)) => Some(self.lagged(n)),
            Err(RecvError::Closed) => None,
        }
    }

    /// Like [`Subscription::recv`] without waiting.
    ///
    /// # Errors
    /// [`Empty::Empty`] if nothing is waiting, [`Empty::Closed`] once every
    /// publisher is gone and everything was received.
    pub fn try_recv(&mut self) -> Result<Delivery<T>, Empty> {
        match self.rx.try_recv() {
            Ok(event) => Ok(Delivery::Event(event)),
            Err(TryRecvError::Lagged(n)) => Ok(self.lagged(n)),
            Err(TryRecvError::Empty) => Err(Empty::Empty),
            Err(TryRecvError::Closed) => Err(Empty::Closed),
        }
    }

    /// The next event, skipping lag notices (still counted in
    /// [`Subscription::missed`]). For readers that tolerate gaps.
    pub async fn next_event(&mut self) -> Option<T> {
        loop {
            match self.recv().await? {
                Delivery::Event(event) => return Some(event),
                Delivery::Lagged { .. } => {}
            }
        }
    }

    /// Every event waiting right now, skipping lag notices.
    pub fn drain(&mut self) -> Vec<T> {
        let mut out = Vec::new();
        while let Ok(d) = self.try_recv() {
            if let Delivery::Event(event) = d {
                out.push(event);
            }
        }
        out
    }

    /// Events lost to lag since the subscription started.
    #[must_use]
    pub fn missed(&self) -> u64 {
        self.missed
    }

    /// Whether nothing is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rx.is_empty()
    }

    fn lagged(&mut self, n: u64) -> Delivery<T> {
        self.missed = self.missed.saturating_add(n);
        tracing::debug!(missed = n, "event subscriber lagged");
        Delivery::Lagged { missed: n }
    }
}

/// A parsed log entry of one instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogEvent {
    /// The instance.
    #[serde(flatten)]
    pub owner: PortOwner,
    /// The entry, shared with the instance's buffer.
    #[serde(flatten)]
    pub record: Arc<Record>,
}

/// Capacities of an [`EventBus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusOptions {
    /// Log events held per subscriber.
    pub log_capacity: usize,
    /// Instance events held per subscriber.
    pub instance_capacity: usize,
}

impl Default for BusOptions {
    fn default() -> Self {
        Self {
            log_capacity: DEFAULT_LOG_CAPACITY,
            instance_capacity: DEFAULT_INSTANCE_CAPACITY,
        }
    }
}

/// The topics core work publishes to. Cheap to clone; clones share topics.
#[derive(Debug, Clone)]
pub struct EventBus {
    logs: Topic<LogEvent>,
    instances: Topic<ManagerEvent>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(BusOptions::default())
    }
}

impl EventBus {
    /// A bus with the given capacities.
    #[must_use]
    pub fn new(opts: BusOptions) -> Self {
        Self {
            logs: Topic::new(opts.log_capacity),
            instances: Topic::new(opts.instance_capacity),
        }
    }

    /// Parsed log entries of all instances.
    #[must_use]
    pub fn logs(&self) -> &Topic<LogEvent> {
        &self.logs
    }

    /// Instance spawns, status changes and manager notices.
    #[must_use]
    pub fn instances(&self) -> &Topic<ManagerEvent> {
        &self.instances
    }
}
