//! Runs [`Core`](crate::core::Core) on a thread so that the window never waits for a scan, a proof or a node.

use std::path::Path;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use tenero_wallet::KdfParams;

use crate::core::Core;
use crate::settings::Settings;
use crate::view::{Cmd, Event};

pub struct Backend {
    tx: Sender<Cmd>,
    rx: Receiver<Event>,
    join: Option<JoinHandle<()>>,
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
        let (ev_tx, rx) = channel::<Event>();
        let app_dir = app_dir.to_path_buf();
        let join = std::thread::spawn(move || {
            let mut core = Core::new(&app_dir, settings, kdf);
            let _ = ev_tx.send(Event::Snapshot(Box::new(core.snapshot())));
            notify();
            loop {
                let events = match cmd_rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(cmd) => core.handle(cmd),
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
        }
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }

    pub fn try_recv(&self) -> Option<Event> {
        self.rx.try_recv().ok()
    }

    /// Waits for the worker to finish (after `Cmd::Quit`).
    pub fn join(&mut self) {
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}
