//! On Linux a service manager's "stop" sends SIGTERM (and a closed terminal sends SIGHUP): the node must treat both as a clean shutdown, like Ctrl-C.
//! Measured before the `termination` option of `ctrlc` was switched on (2026-10-04): exit status 143 and no "shutting down" line.

#![cfg(unix)]

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn scratch(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("tenero-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("a free port")
        .local_addr()
        .expect("an address")
        .port()
}

/// Starts a node on the test network, waits until it is up, sends it `signal`, and returns (its exit status was success, its log).
fn stop_with(signal: &str) -> (bool, String) {
    let dir = scratch("signal");
    let data = dir.join("data");
    let log = dir.join("node.log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_tenerod"))
        .args(["--data", data.to_str().unwrap(), "--network", "test"])
        .args(["--control", &format!("127.0.0.1:{}", free_port())])
        .args(["--log_file", log.to_str().unwrap()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("tenerod starts");
    // up when the log says where the control interface listens
    let start = Instant::now();
    while !std::fs::read_to_string(&log).is_ok_and(|t| t.contains("control interface on")) {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "the node did not start"
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "the node stopped by itself"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let sent = Command::new("kill")
        .args([&format!("-{signal}"), &child.id().to_string()])
        .status()
        .expect("kill runs");
    assert!(sent.success());
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if start.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            panic!("the node did not stop after {signal}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    (status.success(), text)
}

#[test]
fn sigterm_is_a_clean_shutdown() {
    let (ok, log) = stop_with("TERM");
    assert!(ok, "exit status was not success; log:\n{log}");
    assert!(log.contains("shutting down"), "{log}");
    assert!(log.contains("stopped at height"), "{log}");
}

#[test]
fn sighup_is_a_clean_shutdown() {
    let (ok, log) = stop_with("HUP");
    assert!(ok, "exit status was not success; log:\n{log}");
    assert!(log.contains("stopped at height"), "{log}");
}
