//! `tenero-poolcheck`: checks a mining pool against the pool protocol (`docs/POOL_PROTOCOL.md`). **Experimental and unaudited.**

use std::net::ToSocketAddrs;
use std::time::Duration;

use tenero_app::config::Network;
use tenero_app::pool_check::{failures, run, Options, Verdict};
use tenero_core::v2::ids::PowKind;

const USAGE: &str = "\
tenero-poolcheck: checks a mining pool against the pool protocol (EXPERIMENTAL, UNAUDITED)

  tenero-poolcheck --pool HOST:PORT --network NET (--pool-key HEX | --pool-unpinned) [--address ADDR] [--wait SECS]

  --pool HOST:PORT   the pool to check
  --network NET      the network it should serve: gamma, dev or test
  --pool-key HEX     the pool's public key (64 hexadecimal digits): the check fails if the pool proves another
  --pool-unpinned    check without a key
  --address ADDR     an address to give the pool as the one to pay (default: a fixed test address); nothing is ever sent to it
  --wait SECS        how long to wait for each answer (default 10)

It connects as a miner and checks what the protocol requires of a pool, one thing at a time. A pool that passes has shown only that it speaks the
protocol the way these checks look for it: nothing here can tell whether it pays. On a network with the real proof of work the two checks that need a
valid share are skipped (they need the 4 GiB dataset): try those with a real miner. The exit code is 1 if any check failed.";

fn main() {
    if tenero_app::daemon::wants_version(std::env::args().nth(1).as_deref()) {
        println!("{}", tenero_app::daemon::version_line("tenero-poolcheck"));
        return;
    }
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("help" | "--help" | "-h")
    ) {
        println!("{USAGE}");
        return;
    }
    let fail = |e: String| -> ! {
        eprintln!("error: {e}\n\n{USAGE}");
        std::process::exit(2);
    };
    let (mut pool, mut network, mut key, mut unpinned, mut address, mut wait) =
        (None, None, None, false, None, 10u64);
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let Some(k) = flag.strip_prefix("--") else {
            fail(format!("unexpected argument `{flag}`"))
        };
        if k == "pool-unpinned" {
            unpinned = true;
            continue;
        }
        let Some(v) = it.next() else {
            fail(format!("--{k} needs a value"))
        };
        match k {
            "pool" => pool = Some(v),
            "network" => network = Some(v),
            "pool-key" => {
                if v.len() != 64 || !v.bytes().all(|b| b.is_ascii_hexdigit()) {
                    fail("--pool-key must be 64 hexadecimal digits".into());
                }
                let mut b = [0u8; 32];
                for (i, x) in b.iter_mut().enumerate() {
                    *x = u8::from_str_radix(&v[2 * i..2 * i + 2], 16).expect("checked");
                }
                key = Some(b);
            }
            "address" => address = Some(v),
            "wait" => {
                wait = v
                    .parse()
                    .unwrap_or_else(|_| fail(format!("--wait: `{v}` is not a number")))
            }
            other => fail(format!("unknown option `--{other}`")),
        }
    }
    let (Some(pool), Some(network)) = (pool, network) else {
        fail("--pool and --network are required".into())
    };
    if key.is_none() && !unpinned {
        fail("give --pool-key, or --pool-unpinned to check without a key".into());
    }
    let network = Network::parse(&network)
        .unwrap_or_else(|| fail(format!("--network: `{network}` is not gamma, dev or test")));
    let addr = pool
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .unwrap_or_else(|| fail(format!("--pool: cannot look up `{pool}`")));
    let address = address.unwrap_or_else(|| {
        tenero_wallet::Wallet::from_seed(&[0x11; 32], network.wallet_network(), 0)
            .address()
            .to_text()
    });
    let o = Options {
        addr,
        pin: key,
        network: network.name().to_string(),
        pow: if network.real_pow() {
            PowKind::Matmul
        } else {
            PowKind::Sha256
        },
        address,
        wait: Duration::from_secs(wait.max(1)),
    };
    println!(
        "tenero-poolcheck: EXPERIMENTAL and UNAUDITED. Checking {addr} as a {} network pool.",
        network.name()
    );
    let checks = run(&o);
    for c in &checks {
        match &c.verdict {
            Verdict::Pass => println!("  PASS  {}", c.name),
            Verdict::Fail(why) => println!("  FAIL  {}\n          {why}", c.name),
            Verdict::Skipped(why) => println!("  skip  {}\n          {why}", c.name),
        }
    }
    let bad = failures(&checks);
    let skipped = checks
        .iter()
        .filter(|c| matches!(c.verdict, Verdict::Skipped(_)))
        .count();
    println!(
        "result: {} checks, {} passed, {} failed, {} skipped",
        checks.len(),
        checks.len() - bad - skipped,
        bad,
        skipped
    );
    if bad > 0 {
        std::process::exit(1);
    }
}
