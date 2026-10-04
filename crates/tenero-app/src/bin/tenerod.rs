//! The node program. `tenerod --config tenero.conf`, or settings as `--key value` options; see
//! `docs/RUNNING.md` for every setting. `tenerod stop --data DIR` and `tenerod status --data DIR` talk to a running
//! node. **Experimental, unaudited, nothing on any network it runs has value.**

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tenero_app::client::{read_cookie, RemoteNode, COOKIE_FILE};
use tenero_app::config::Raw;
use tenero_app::daemon;
use tenero_app::log::{Level, Logger};
use tenero_app::ui::{Event as UiEvent, Screen, Verbosity};

const USAGE: &str = "\
tenerod: the Tenero node (EXPERIMENTAL, UNAUDITED; no launched network exists)

  tenerod --config FILE [--key value ...]       run a node
  tenerod --data DIR --network test|dev|alpha [...] run a node with settings on the command line
  tenerod --version                             which build this is (version and source commit)
  tenerod status --data DIR [--control IP:PORT] ask a running node about itself
  tenerod stop   --data DIR [--control IP:PORT] ask a running node to shut down cleanly
  tenerod rewind --data DIR --network test|dev|alpha --to HEIGHT [--yes]
                                                emergency: with the node STOPPED, take the newest blocks off its chain down to HEIGHT.
                                                Without --yes it only says what it would remove. See docs/EMERGENCY_PLAN.md

Settings (the same keys in the file as `key = value` and on the command line as `--key value`):
  data, network (test|dev|alpha), listen, seed (repeatable), peers, max_inbound, allow_private_peers, control,
  prune_keep (0 = archive node), assume_valid (height:blockid), mine (off|sha256|cpu|gpu), mine_to (address),
  mine_cores, mine_pace, gpu_device, gpu_batch, log_level, log_file, status_every, quiet, verbose, color.

What the screen shows: a banner, a status block that redraws in place on a terminal (plain lines when the output is a file or a pipe),
and events in plain words. The log file keeps the full detail. `--quiet` shows only warnings and errors; `--verbose` also shows every
line of the log; `--color auto|always|never` (colour is off with NO_COLOR, and never used when the output is not a terminal).";

fn control_client(args: &[String]) -> Result<RemoteNode, String> {
    let (mut data, mut control) = (None, "127.0.0.1:18332".to_string());
    let mut it = args.iter();
    while let Some(f) = it.next() {
        let v = it.next().ok_or_else(|| format!("{f} needs a value"))?;
        match f.as_str() {
            "--data" => data = Some(PathBuf::from(v)),
            "--control" => control = v.clone(),
            other => return Err(format!("unknown option {other}")),
        }
    }
    let data = data.ok_or("--data is required")?;
    let cookie = read_cookie(&data.join(COOKIE_FILE))?;
    RemoteNode::connect(
        control
            .parse()
            .map_err(|_| format!("--control: `{control}` is not ip:port"))?,
        &cookie,
    )
}

/// `tenerod rewind`: see `daemon::rewind`. Returns the exit code.
fn rewind_command(args: &[String]) -> i32 {
    let (mut data, mut network, mut to, mut yes) = (None, None, None, false);
    let mut it = args.iter();
    while let Some(f) = it.next() {
        if f == "--yes" {
            yes = true;
            continue;
        }
        let Some(v) = it.next() else {
            eprintln!("error: {f} needs a value");
            return 2;
        };
        match f.as_str() {
            "--data" => data = Some(PathBuf::from(v)),
            "--network" => network = Some(v.clone()),
            "--to" => match v.parse::<u64>() {
                Ok(h) => to = Some(h),
                Err(_) => {
                    eprintln!("error: --to: `{v}` is not a block height");
                    return 2;
                }
            },
            other => {
                eprintln!("error: unknown option {other}");
                return 2;
            }
        }
    }
    let (Some(data), Some(network), Some(to)) = (data, network, to) else {
        eprintln!("error: rewind needs --data, --network and --to\n\n{USAGE}");
        return 2;
    };
    let Some(network) = tenero_app::config::Network::parse(&network) else {
        eprintln!("error: --network: `{network}` is not test, dev or alpha");
        return 2;
    };
    match daemon::rewind(&data, network, to, yes) {
        Ok(r) => {
            println!(
                "the chain was at height {} (tip {}); to height {} (tip {}) {} {} block(s), newest {} down to {}",
                r.tip_height,
                daemon::short_id(&r.tip_id),
                r.new_height,
                daemon::short_id(&r.new_tip_id),
                if r.applied { "it lost" } else { "it would lose" },
                r.removed.len(),
                r.tip_height,
                r.new_height + 1
            );
            if r.applied {
                println!(
                    "DONE: the chain is now at height {} (tip {}). The removed blocks are listed in {}. The side-branch pool was set aside.",
                    r.new_height,
                    daemon::short_id(&r.new_tip_id),
                    r.record.map(|p| p.display().to_string()).unwrap_or_default()
                );
                println!("Start the node again ONLY with a build that refuses the bad block; a peer that still has it will send it again.");
            } else {
                println!("nothing was changed (a dry run). Make a copy of the data directory, then run the same command again with --yes.");
            }
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(
        args.first().map(String::as_str),
        Some("help" | "--help" | "-h")
    ) {
        println!("{USAGE}");
        return;
    }
    if matches!(
        args.first().map(String::as_str),
        Some("version" | "--version" | "-V")
    ) {
        println!(
            "tenerod v{} (commit {}) EXPERIMENTAL, UNAUDITED; nothing on any network it runs has value",
            daemon::VERSION,
            daemon::COMMIT
        );
        return;
    }
    match args.first().map(String::as_str) {
        Some("stop") => {
            let r = control_client(&args[1..]).and_then(|c| c.stop());
            match r {
                Ok(()) => println!("the node is shutting down"),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("rewind") => {
            let code = rewind_command(&args[1..]);
            std::process::exit(code);
        }
        Some("status") => {
            match control_client(&args[1..]).and_then(|c| c.info()) {
                Ok(i) => println!(
                    "network {} | height {} ({}) | peers {} (in {}) | mempool {} | {}{}",
                    i.network,
                    i.height,
                    daemon::short_id(&i.tip_id),
                    i.peers,
                    i.inbound,
                    i.mempool_txs,
                    if i.syncing { "syncing" } else { "in sync" },
                    if i.pruned_below > 0 {
                        format!(" | pruned below {}", i.pruned_below)
                    } else {
                        String::new()
                    },
                ),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        _ => {}
    }
    // `--config FILE` is read first and the other options laid over it
    let mut config_path = None;
    if let Some(i) = args.iter().position(|a| a == "--config") {
        if i + 1 >= args.len() {
            eprintln!("error: --config needs a file");
            std::process::exit(2);
        }
        config_path = Some(args[i + 1].clone());
        args.drain(i..=i + 1);
    }
    let file_text = match &config_path {
        Some(p) => match std::fs::read_to_string(p) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("error: cannot read {p}: {e}");
                std::process::exit(2);
            }
        },
        None => String::new(),
    };
    let cfg = Raw::from_file_text(&file_text)
        .and_then(|r| r.with_args(&args))
        .and_then(|r| r.into_config());
    let cfg = match cfg {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let verbosity = if cfg.quiet {
        Verbosity::Quiet
    } else if cfg.verbose {
        Verbosity::Verbose
    } else {
        Verbosity::Normal
    };
    let screen = Arc::new(Screen::for_stderr(cfg.color, verbosity, cfg.status_every));
    let log = match Logger::new(cfg.log_level, cfg.log_file.as_deref(), false) {
        Ok(l) => Arc::new(l.with_screen(Arc::clone(&screen))),
        Err(e) => {
            eprintln!("error: cannot open the log file: {e}");
            std::process::exit(2);
        }
    };
    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let (s, l) = (Arc::clone(&shutdown), Arc::clone(&log));
        let handler = ctrlc::set_handler(move || {
            // a second Ctrl-C while shutting down ends the process at once
            if s.swap(true, Ordering::SeqCst) {
                std::process::exit(130);
            }
            l.log_event(
                Level::Info,
                "shutdown requested (Ctrl-C again to stop at once)",
                UiEvent::ShuttingDown,
            );
        });
        if let Err(e) = handler {
            log.warn(&format!("Ctrl-C will not shut the node down cleanly: {e}"));
        }
    }
    if let Err(e) = daemon::run(&cfg, Arc::clone(&log), shutdown, None) {
        log.error(&e);
        std::process::exit(1);
    }
}
