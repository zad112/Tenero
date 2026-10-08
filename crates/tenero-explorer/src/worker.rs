//! Asks the node on a thread of its own, every [`REFRESH`](crate::core::REFRESH) or when the window asks, so the window
//! never waits for it. A lost connection is dropped and made again at the next look (a restarted node has a new cookie,
//! which [`Source::connect`] reads afresh).

use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tenero_app::client::RemoteNode;

use crate::core::{fetch, Snapshot, Source};

/// What the worker tells the window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update {
    Snapshot(Box<Snapshot>),
    /// The node could not be reached or did not answer; the last snapshot (if any) is still the latest known.
    Problem(String),
}

/// What the window tells the worker.
enum Ask {
    RefreshNow,
    Quit,
}

pub struct Worker {
    asks: Sender<Ask>,
    updates: Receiver<Update>,
    join: Option<JoinHandle<()>>,
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Worker {
    /// Starts asking the node at `source` every `every`. `notify` is called whenever there is something new (the window
    /// passes a repaint).
    pub fn spawn(source: Source, every: Duration, notify: Arc<dyn Fn() + Send + Sync>) -> Worker {
        let (asks, ask_rx) = channel::<Ask>();
        let (up_tx, updates) = channel::<Update>();
        let join = std::thread::spawn(move || {
            let mut node: Option<RemoteNode> = None;
            loop {
                let update = match node.take().map_or_else(|| source.connect(), Ok) {
                    Ok(n) => match fetch(&n, unix_now()) {
                        Ok(s) => {
                            node = Some(n);
                            Update::Snapshot(Box::new(s))
                        }
                        // the connection is dropped: the next look makes a new one
                        Err(e) => Update::Problem(format!("the node did not answer: {e}")),
                    },
                    Err(e) => Update::Problem(e),
                };
                if up_tx.send(update).is_err() {
                    return;
                }
                notify();
                match ask_rx.recv_timeout(every) {
                    Ok(Ask::RefreshNow) | Err(RecvTimeoutError::Timeout) => {}
                    Ok(Ask::Quit) | Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        });
        Worker {
            asks,
            updates,
            join: Some(join),
        }
    }

    /// A worker with nothing behind it: the returned sender stands in for it. For drawing the window in a test.
    pub fn detached() -> (Worker, Sender<Update>) {
        let (asks, _) = channel::<Ask>();
        let (tx, updates) = channel::<Update>();
        (
            Worker {
                asks,
                updates,
                join: None,
            },
            tx,
        )
    }

    pub fn refresh_now(&self) {
        let _ = self.asks.send(Ask::RefreshNow);
    }

    pub fn try_recv(&self) -> Option<Update> {
        self.updates.try_recv().ok()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.asks.send(Ask::Quit);
        // a fetch in progress ends within the connection's own timeouts; the window does not wait for it
        drop(self.join.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_no_node_it_says_so_and_keeps_trying() {
        // a data folder with no cookie file: no node there
        let dir = std::env::temp_dir().join(format!("tenero-explorer-none-{}", std::process::id()));
        let source = Source {
            data_dir: dir,
            control: "127.0.0.1:1".parse().unwrap(),
            found_by: "a test".into(),
        };
        let w = Worker::spawn(source, Duration::from_millis(20), Arc::new(|| {}));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut problems = 0;
        while problems < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "no word from the worker"
            );
            match w.try_recv() {
                Some(Update::Problem(m)) => {
                    assert!(m.contains("is the node running"), "{m}");
                    problems += 1;
                }
                Some(other) => panic!("{other:?}"),
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }
}
