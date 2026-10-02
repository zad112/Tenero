//! The seed health check: `tenero-seedcheck help`. **Experimental, unaudited; nothing on any network it checks has value.**

use std::io::Write;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tenero_app::config::Network;
use tenero_app::daemon::chain_id_of;
use tenero_app::seedcheck::{
    evaluate, history_line, parse_args, probe, uptime, EvalConfig, ProbeConfig, USAGE,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "help" || args[0] == "--help" {
        println!("{USAGE}");
        return;
    }
    let read = |p: &str| std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"));
    let o = match parse_args(&args, &read) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("tenero-seedcheck: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let network = Network::parse(&o.network).expect("parse_args checked the network");
    let chain_id = match chain_id_of(network) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "tenero-seedcheck: cannot work out the chain id of {}: {e}",
                o.network
            );
            std::process::exit(2);
        }
    };
    let pc = ProbeConfig {
        timeout: Duration::from_secs(o.timeout_s),
        addr_wait: Duration::from_secs(o.addr_wait_s),
    };
    // every seed at once: a dead one costs the time it takes to give up, not that time each
    let probes: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = o
            .seeds
            .iter()
            .map(|seed| {
                let pc = pc.clone();
                s.spawn(move || probe(seed, chain_id, &pc))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("a probe thread"))
            .collect()
    });
    let mut ec = EvalConfig::new(chain_id);
    ec.max_lag_blocks = o.max_lag;
    ec.min_addrs = o.min_addrs;
    ec.min_seeds = o.min_seeds;
    ec.private_network = o.private;
    let report = evaluate(&probes, &ec);
    println!(
        "network {} (chain {})",
        o.network,
        chain_id[..4]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    print!("{}", report.to_text());
    if let Some(path) = &o.history {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let lines: Vec<String> = report.seeds.iter().map(|s| history_line(now, s)).collect();
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(mut f) => {
                for l in &lines {
                    let _ = writeln!(f, "{l}");
                }
            }
            Err(e) => eprintln!("tenero-seedcheck: cannot write {path}: {e}"),
        }
        let text = std::fs::read_to_string(path).unwrap_or_default();
        println!("uptime over the last 50 checks:");
        for s in &report.seeds {
            let (up, n) = uptime(&text, &s.seed, 50);
            println!("  {}  up {up} of {n}", s.seed);
        }
    }
    std::process::exit(report.severity.exit_code());
}
