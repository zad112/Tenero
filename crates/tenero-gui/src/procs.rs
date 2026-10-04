//! Starting, watching and stopping the node and the miner as processes of their own.
//!
//! **Safety rules (the owner's, and they are why this module is small):**
//! * Only a process this program started is ever stopped, and only through the handle `spawn` returned for it. Nothing
//!   here looks a process up by name or number.
//! * The node is stopped by asking it to (the control interface's `Stop`, which lets it finish writing its database),
//!   and only if it has not stopped after a minute is the program's own handle to it killed.
//! * A child gets no window: on Windows it is created with `CREATE_NO_WINDOW`, and its output goes to a file in the app
//!   folder, never to a terminal.

use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tenero_app::client::{read_cookie, RemoteNode, COOKIE_FILE};
use tenero_app::config::Network;

use crate::settings::{MinerBackend, NodeKind, Settings};

/// A child process this program started. Cloning gives another handle to the same child (the window keeps one so that it
/// can still end what it started if the worker thread is stuck).
#[derive(Clone)]
pub struct Proc {
    child: Arc<Mutex<Child>>,
    /// Where its output goes.
    pub log: PathBuf,
}

/// The `CREATE_NO_WINDOW` process creation flag.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

impl Proc {
    /// Starts `exe` with `args`, output to `log` (replaced), no window, no input.
    pub fn spawn(exe: &Path, args: &[OsString], log: &Path) -> Result<Proc, String> {
        if let Some(dir) = log.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        let out = File::create(log).map_err(|e| format!("cannot write {}: {e}", log.display()))?;
        let err = out.try_clone().map_err(|e| e.to_string())?;
        let mut cmd = Command::new(exe);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let child = cmd
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", exe.display()))?;
        Ok(Proc {
            child: Arc::new(Mutex::new(child)),
            log: log.to_path_buf(),
        })
    }

    /// `None` while it runs; its exit as words once it has stopped.
    pub fn exited(&mut self) -> Option<String> {
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        match child.try_wait() {
            Ok(None) => None,
            Ok(Some(s)) => Some(match s.code() {
                Some(0) => "stopped".to_string(),
                Some(c) => format!("stopped with code {c}"),
                None => "was ended".to_string(),
            }),
            Err(e) => Some(format!("cannot tell: {e}")),
        }
    }

    /// Like [`Proc::exited`] on a handle that only wants to know (`None` = still running).
    pub fn exited_quietly(&self) -> Option<()> {
        let mut me = self.clone();
        me.exited().map(|_| ())
    }

    /// Waits up to `timeout` for it to stop by itself.
    pub fn wait(&mut self, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        loop {
            if self.exited().is_some() {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Ends this child at once (the program's own handle; nothing else is touched).
    pub fn kill(&mut self) {
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        let _ = child.kill();
        let _ = child.wait();
    }

    /// The last lines of its output, for showing why it stopped.
    pub fn tail(&self, lines: usize) -> String {
        tail_of(&self.log, lines)
    }
}

/// The last `lines` lines of a text file (empty if it cannot be read). Reads at most the last 64 KiB.
pub fn tail_of(path: &Path, lines: usize) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map_or(0, |m| m.len());
    let start = len.saturating_sub(64 * 1024);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    let _ = f.take(64 * 1024).read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Where a program is: in the configured folder, else next to this one.
pub fn program_path(settings: &Settings, name: &str) -> PathBuf {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    if let Some(d) = &settings.program_dir {
        return d.join(file);
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(&file)))
        .unwrap_or_else(|| PathBuf::from(file))
}

/// The arguments that start the node with these settings. Mining is never switched on here: the miner is its own
/// program, started on purpose.
pub fn node_args(s: &Settings) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    let mut push = |k: &str, v: OsString| {
        a.push(format!("--{k}").into());
        a.push(v);
    };
    push("data", s.data_dir.clone().into_os_string());
    push("network", s.network.name().into());
    push("control", s.control.to_string().into());
    push(
        "prune_keep",
        match s.node_kind {
            NodeKind::Archive => "0".to_string(),
            NodeKind::Pruned { keep } => keep.to_string(),
        }
        .into(),
    );
    for seed in &s.seeds {
        push("seed", seed.into());
    }
    if let Some(l) = &s.listen {
        push("listen", l.into());
    }
    push("color", "never".into());
    // the screen goes to a file nobody watches live: a status line now and then is enough
    push("status_every", "30".into());
    a
}

/// The arguments that start the miner for `address`, reporting to `status_file`.
pub fn miner_args(s: &Settings, address: &str, status_file: &Path) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    let mut push = |k: &str, v: OsString| {
        a.push(format!("--{k}").into());
        a.push(v);
    };
    push("data", s.data_dir.clone().into_os_string());
    push("control", s.control.to_string().into());
    push("address", address.into());
    push("backend", s.miner_backend.name().into());
    push("pace", s.miner_pace_secs.to_string().into());
    match s.miner_backend {
        MinerBackend::Cpu => push("cores", s.miner_cores.to_string().into()),
        MinerBackend::Gpu => {
            push("gpu-device", s.miner_gpu_device.to_string().into());
            if s.miner_gpu_auto_batch {
                push("gpu-batch", "auto".into());
            }
        }
        MinerBackend::Sha256 => {}
    }
    push("status-file", status_file.as_os_str().to_os_string());
    push("color", "never".into());
    a
}

/// Tries to reach a node at the control address with the cookie in its data folder: `Some` if one answers.
/// (A node the program did not start itself, or one an earlier run left going.)
pub fn reach_node(s: &Settings) -> Option<RemoteNode> {
    let cookie = read_cookie(&s.data_dir.join(COOKIE_FILE)).ok()?;
    RemoteNode::connect(s.control, &cookie).ok()
}

/// Asks the node to shut down cleanly and, for a node this program started, waits for it to stop, killing its own
/// handle only if a minute is not enough. `Ok` says how it ended.
pub fn stop_node(node: &RemoteNode, own: Option<&mut Proc>) -> Result<&'static str, String> {
    node.stop()?;
    match own {
        Some(p) => {
            if p.wait(Duration::from_secs(60)) {
                Ok("stopped cleanly")
            } else {
                p.kill();
                Ok("did not stop in a minute and was ended")
            }
        }
        None => Ok("asked to stop"),
    }
}

/// Which network a settings value describes, in words for the screen.
pub fn network_words(n: Network) -> &'static str {
    match n {
        Network::Test => "test network (SHA-256, a CPU can mine it)",
        Network::Dev => "development network (the real GPU proof of work)",
        Network::Alpha => {
            "alpha network (the first test release: the real proof of work, no premine, no value)"
        }
    }
}
