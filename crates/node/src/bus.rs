//! In-process fan-out for protocol 2 streams. Producers (the sampler, the
//! alert engine, the container watcher) send without blocking; each
//! subscribed connection holds a receiver. A receiver that falls behind
//! loses the oldest frames and is told how many (`lagged`), so a slow
//! client can never make the node buffer without bound.

use nightgrid_node_proto::{ContainersResult, Event, Sample, UsageSample};
use tokio::sync::broadcast;

use crate::events::NewEvent;
use crate::store::{Result, Store};

pub struct Bus {
    pub events: broadcast::Sender<Event>,
    pub metrics: broadcast::Sender<Sample>,
    pub containers: broadcast::Sender<ContainersResult>,
    pub usage: broadcast::Sender<UsageSample>,
}

impl Default for Bus {
    fn default() -> Self {
        Self {
            events: broadcast::channel(256).0,
            metrics: broadcast::channel(16).0,
            containers: broadcast::channel(16).0,
            usage: broadcast::channel(16).0,
        }
    }
}

impl Bus {
    /// Someone is watching live metrics: sample faster.
    pub fn metrics_watched(&self) -> bool {
        self.metrics.receiver_count() > 0
    }

    pub fn containers_watched(&self) -> bool {
        self.containers.receiver_count() > 0
    }

    /// Records `e` and pushes it to `events` subscribers. Returns its `seq`.
    pub fn record(&self, store: &Store, e: &NewEvent) -> Result<i64> {
        let seq = store.event_add(e)?;
        if self.events.receiver_count() > 0 {
            if let Some(ev) = store.events_after(seq - 1, 1)?.into_iter().next() {
                let _ = self.events.send(ev);
            }
        }
        Ok(seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightgrid_node_proto::{EventKind, Level};

    #[test]
    fn records_and_pushes_the_stored_event() {
        let bus = Bus::default();
        let s = Store::open_in_memory().unwrap();
        // Nobody listening: still recorded.
        bus.record(&s, &NewEvent::new(1, EventKind::Boot, Level::Info, "Booted", "")).unwrap();
        let mut rx = bus.events.subscribe();
        let seq = bus.record(&s, &NewEvent::new(2, EventKind::Other, Level::Warn, "x", "token=hunter2secret")).unwrap();
        let ev = rx.try_recv().unwrap();
        assert_eq!((ev.seq, seq, ev.title.as_str()), (2, 2, "x"));
        // What subscribers see went through the same filter as the store.
        assert_eq!(ev.detail, "(hidden: looked like it contained a secret)");
    }
}
