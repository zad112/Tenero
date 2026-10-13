//! Runs [`Core`](crate::core::Core) on a thread so that the window never waits for a scan, a proof or a node.

use std::path::Path;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tenero_wallet::KdfParams;

use crate::core::{Core, Work, WorkState};
use crate::settings::Settings;
use crate::view::{Cmd, Event, Snapshot};

/// The window has gone (nothing will read what is sent any more).
#[derive(Debug)]
pub struct WindowGone;

/// How the worker's events reach the window. A [`Snapshot`] is everything the window draws, made again after every change
/// (about twice a second while a node or a miner runs), so only the LATEST one is worth anything: it is kept in one slot that
/// each new one overwrites. Everything else (notices, errors, results) is queued in order. Up to 0.4.1 the snapshots were
/// queued too, and a window that drew nothing for hours (minimized: Windows sends it no frames, so it reads nothing) held
/// every one: about 20 GB after a night of mining, freed all at once, with a freeze, when the window was shown again.
#[derive(Clone)]
pub struct Outbox {
    tx: Sender<Event>,
    latest: Arc<Mutex<Option<Box<Snapshot>>>>,
}

impl Outbox {
    pub fn send(&self, e: Event) -> Result<(), WindowGone> {
        match e {
            Event::Snapshot(s) => {
                if let Ok(mut slot) = self.latest.lock() {
                    *slot = Some(s);
                }
                Ok(())
            }
            e => self.tx.send(e).map_err(|_| WindowGone),
        }
    }
}

/// The window's end of an [`Outbox`].
pub struct Inbox {
    rx: Receiver<Event>,
    latest: Arc<Mutex<Option<Box<Snapshot>>>>,
}

impl Inbox {
    /// The next event: the queued ones first, in order, then the latest snapshot (if one came since the last look). A
    /// snapshot is made after the events of the same change, so it comes after them here too.
    pub fn try_recv(&self) -> Option<Event> {
        if let Ok(e) = self.rx.try_recv() {
            return Some(e);
        }
        self.latest
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
            .map(Event::Snapshot)
    }
}

/// A new outbox and its inbox.
pub fn mailbox() -> (Outbox, Inbox) {
    let (tx, rx) = channel::<Event>();
    let latest = Arc::new(Mutex::new(None));
    (
        Outbox {
            tx,
            latest: Arc::clone(&latest),
        },
        Inbox { rx, latest },
    )
}

pub struct Backend {
    tx: Sender<Cmd>,
    rx: Inbox,
    join: Option<JoinHandle<()>>,
    registry: crate::core::Registry,
    work: WorkState,
}

impl Backend {
    /// Starts the worker. `notify` is called whenever there is something new to draw (the window passes a repaint).
    pub fn spawn(
        app_dir: &Path,
        settings: Settings,
        kdf: KdfParams,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> Backend {
        let (tx, cmd_rx) = channel::<Cmd>();
        let (ev_tx, rx) = mailbox();
        let app_dir = app_dir.to_path_buf();
        let registry = crate::core::Registry::default();
        let worker_registry = registry.clone();
        let work = WorkState::default();
        let worker_work = work.clone();
        let join = std::thread::spawn(move || {
            let mut core = Core::with_registry(&app_dir, settings, kdf, worker_registry);
            core.set_work_state(worker_work.clone());
            let _ = ev_tx.send(Event::Snapshot(Box::new(core.snapshot())));
            notify();
            let set = |w: Option<Work>| {
                if let Ok(mut g) = worker_work.lock() {
                    *g = w;
                }
            };
            loop {
                let events = match cmd_rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(cmd) => {
                        set(Some(Work {
                            what: cmd.doing(),
                            since: Instant::now(),
                            done: 0,
                            total: 0,
                        }));
                        let events = core.handle(cmd);
                        set(None);
                        events
                    }
                    Err(RecvTimeoutError::Timeout) => core.tick(),
                    // the window is gone without saying Quit: end cleanly all the same
                    Err(RecvTimeoutError::Disconnected) => core.handle(Cmd::Quit),
                };
                let mut quit = false;
                for e in events {
                    quit |= matches!(e, Event::Quit);
                    if ev_tx.send(e).is_err() {
                        quit = true;
                    }
                }
                notify();
                if quit {
                    break;
                }
            }
        });
        Backend {
            tx,
            rx,
            join: Some(join),
            registry,
            work,
        }
    }

    /// What the worker is busy with, if it is handling a command (it can be for minutes: a combine's proofs).
    pub fn work(&self) -> Option<Work> {
        self.work.lock().ok().and_then(|w| w.clone())
    }

    /// A backend with no worker behind it: commands go nowhere and nothing comes back. For drawing the window in a test.
    pub fn detached() -> (Backend, Outbox, Receiver<Cmd>) {
        let (tx, cmd_rx) = channel::<Cmd>();
        let (ev_tx, rx) = mailbox();
        (
            Backend {
                tx,
                rx,
                join: None,
                registry: Default::default(),
                work: Default::default(),
            },
            ev_tx,
            cmd_rx,
        )
    }

    /// For when the worker does not answer `Quit`: ends what this window started without it. The miner is ended; the node is
    /// asked to stop through a connection of its own and given a short time before its handle is ended too. Only programs
    /// this window started are touched (they are the handles it kept).
    pub fn emergency_stop(&self, settings: &Settings) {
        let procs: Vec<(bool, crate::procs::Proc)> =
            self.registry.lock().map(|r| r.clone()).unwrap_or_default();
        for (is_node, mut p) in procs {
            if p.exited().is_some() {
                continue;
            }
            if is_node {
                if let Some(n) = crate::procs::reach_node(settings) {
                    let _ = n.stop();
                }
                if !p.wait(Duration::from_secs(20)) {
                    p.kill();
                }
            } else {
                p.kill();
            }
        }
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }

    pub fn try_recv(&self) -> Option<Event> {
        self.rx.try_recv()
    }

    /// Waits for the worker to finish (after `Cmd::Quit`).
    pub fn join(&mut self) {
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}
