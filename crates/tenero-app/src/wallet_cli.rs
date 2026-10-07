//! The wallet program's commands. The logic is here, behind a small [`Io`] trait, so that tests can drive it with a
//! scripted terminal; `src/bin/tenero_wallet.rs` supplies the real one (a hidden passphrase prompt).
//!
//! **Interim output scheme, unaudited, and nothing here has value.** The seed is printed once, by `create`, and by
//! `seed` after the passphrase; there is no word-list backup yet.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use rand_core::OsRng;
use tenero_core::hash::hex_lower;
use tenero_wallet::amount::{format_coins, parse_coins};
use tenero_wallet::{Address, Built, ChainView, FeeLevel, KdfParams, Wallet, WalletError, BANNER};
use zeroize::Zeroizing;

use crate::client::{read_cookie, RemoteNode, COOKIE_FILE};

pub const MIN_PASSPHRASE: usize = 8;
pub const DEFAULT_CONTROL: &str = "127.0.0.1:18332";

/// What the commands need from a terminal.
pub trait Io {
    /// A passphrase, not shown as it is typed. `prompt` says what it is for.
    fn passphrase(&mut self, prompt: &str) -> Result<Zeroizing<String>, String>;
    fn say(&mut self, line: &str);
}

struct Opts {
    wallet: Option<PathBuf>,
    data: Option<PathBuf>,
    control: SocketAddr,
    birth: Option<u64>,
    to: Option<String>,
    amount: Option<String>,
    file: Option<PathBuf>,
    coins: Option<usize>,
    unsent_file: Option<PathBuf>,
    /// `sweep` and `combine` only preview unless this is given.
    yes: bool,
    passphrase_file: Option<PathBuf>,
    /// Test only: a fast key derivation, so tests do not each spend a quarter of a second.
    weak_kdf_for_tests: bool,
}

fn parse_opts(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        wallet: None,
        data: None,
        control: DEFAULT_CONTROL.parse().expect("valid"),
        birth: None,
        to: None,
        amount: None,
        file: None,
        coins: None,
        unsent_file: None,
        yes: false,
        passphrase_file: None,
        weak_kdf_for_tests: false,
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let Some(key) = flag.strip_prefix("--") else {
            return Err(format!("unexpected argument `{flag}`"));
        };
        if key == "weak-kdf-for-tests" {
            o.weak_kdf_for_tests = true;
            continue;
        }
        if key == "yes" {
            o.yes = true;
            continue;
        }
        let value = it.next().ok_or_else(|| format!("--{key} needs a value"))?;
        if !seen.insert(key.to_string()) {
            return Err(format!("--{key} given twice"));
        }
        match key {
            "wallet" => o.wallet = Some(PathBuf::from(value)),
            "data" => o.data = Some(PathBuf::from(value)),
            "control" => {
                o.control = value
                    .parse()
                    .map_err(|_| format!("--control: `{value}` is not ip:port"))?
            }
            "birth" => {
                o.birth = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--birth: `{value}` is not a height"))?,
                )
            }
            "to" => o.to = Some(value.clone()),
            "amount" => o.amount = Some(value.clone()),
            "file" => o.file = Some(PathBuf::from(value)),
            "unsent-file" => o.unsent_file = Some(PathBuf::from(value)),
            "pieces" => {
                o.coins = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--pieces: `{value}` is not a number"))?,
                )
            }
            "passphrase-file" => o.passphrase_file = Some(PathBuf::from(value)),
            other => return Err(format!("unknown option `--{other}`")),
        }
    }
    Ok(o)
}

pub const USAGE: &str = "\
tenero-wallet: a wallet for the Tenero experimental coin (INTERIM output scheme, unaudited, no value)

  tenero-wallet create  --wallet FILE [--data DIR] [--birth HEIGHT]
  tenero-wallet restore --wallet FILE [--birth HEIGHT]        (asks for the seed, hidden)
  tenero-wallet address --wallet FILE
  tenero-wallet balance --wallet FILE --data DIR [--control IP:PORT]
  tenero-wallet pay     --wallet FILE --data DIR --to ADDRESS --amount COINS [--control IP:PORT]
  tenero-wallet pay-many --wallet FILE --data DIR --file PAYMENTS [--unsent-file FILE] [--control IP:PORT]
  tenero-wallet sweep   --wallet FILE --data DIR [--to ADDRESS] [--yes] [--control IP:PORT]
  tenero-wallet combine --wallet FILE --data DIR --pieces N [--yes] [--control IP:PORT]
  tenero-wallet seed    --wallet FILE
  tenero-wallet info    --data DIR [--control IP:PORT]

A payment that needs more pieces than one transaction can carry (your balance is made of separate pieces, one for each payment you
received), or more than 15 recipients, is split into several transactions that spend different pieces; `pay-many` reads one `ADDRESS AMOUNT` a line. `sweep` combines your pieces (to your own address, or to --to) and `combine`
makes N of the smallest into one: both only show a preview until --yes. Pieces that come back as change can be spent after 10 blocks.

--data is the node's data directory (the wallet reads the node's cookie file from it). The control address defaults
to 127.0.0.1:18332 (the test network; the dev network's default is 127.0.0.1:28332). The wallet asks for its
passphrase at a hidden prompt; --passphrase-file FILE reads it from a file instead, which is weaker.";

fn need<'a, T>(v: &'a Option<T>, what: &str) -> Result<&'a T, String> {
    v.as_ref().ok_or_else(|| format!("{what} is required"))
}

fn read_passphrase(
    o: &Opts,
    io: &mut dyn Io,
    prompt: &str,
    confirm: bool,
) -> Result<Zeroizing<String>, String> {
    if let Some(path) = &o.passphrase_file {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        return Ok(Zeroizing::new(
            text.trim_end_matches(['\r', '\n']).to_string(),
        ));
    }
    let p = io.passphrase(prompt)?;
    if confirm {
        let again = io.passphrase("Type it again: ")?;
        if *p != *again {
            return Err("the two passphrases differ".into());
        }
    }
    Ok(p)
}

fn kdf(o: &Opts) -> KdfParams {
    if o.weak_kdf_for_tests {
        KdfParams::TEST_ONLY_WEAK
    } else {
        KdfParams::DEFAULT
    }
}

fn connect(o: &Opts) -> Result<RemoteNode, String> {
    let data = need(&o.data, "--data (the node's data directory)")?;
    let cookie = read_cookie(&data.join(COOKIE_FILE))?;
    RemoteNode::connect(o.control, &cookie)
}

fn load(o: &Opts, io: &mut dyn Io) -> Result<(Wallet, Zeroizing<String>), String> {
    let path = need(&o.wallet, "--wallet")?;
    let pass = read_passphrase(o, io, "Wallet passphrase: ", false)?;
    let w = Wallet::load(path, pass.as_bytes()).map_err(|e| e.to_string())?;
    Ok((w, pass))
}

fn save(w: &Wallet, path: &Path, pass: &str, o: &Opts) -> Result<(), String> {
    w.save(path, pass.as_bytes(), kdf(o), &mut OsRng)
        .map_err(|e| e.to_string())
}

fn show_seed(io: &mut dyn Io, w: &Wallet) {
    io.say("");
    io.say("YOUR SEED (write it down on paper and keep it somewhere safe):");
    io.say(&format!("  {}", hex_lower(w.seed())));
    io.say("Anyone who sees it can spend your coins. If you lose both it and the wallet file, the coins are gone.");
    io.say("There is no word-list backup yet; this is the raw seed.");
}

/// The most recipients a `pay-many` file may list (a pool paying its miners: thousands, not millions).
pub const MAX_PAY_MANY: usize = 20_000;

/// Reads a `pay-many` file: one payment a line, `ADDRESS AMOUNT`; blank lines and lines starting with `#` are skipped. Strict: a bad line is an error
/// naming it, and nothing is paid.
pub fn read_payments(text: &str) -> Result<Vec<(Address, u64)>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.split_whitespace();
        let (Some(a), Some(v), None) = (words.next(), words.next(), words.next()) else {
            return Err(format!("line {}: expected `ADDRESS AMOUNT`", n + 1));
        };
        let to = Address::from_text(a).map_err(|e| format!("line {}: the address: {e}", n + 1))?;
        let units = parse_coins(v).ok_or_else(|| {
            format!(
                "line {}: `{v}` is not an amount (digits with up to 8 decimals)",
                n + 1
            )
        })?;
        if units == 0 {
            return Err(format!("line {}: an amount of nothing", n + 1));
        }
        out.push((to, units));
        if out.len() > MAX_PAY_MANY {
            return Err(format!("more than {MAX_PAY_MANY} payments in one file"));
        }
    }
    if out.is_empty() {
        return Err("the file lists no payments".into());
    }
    Ok(out)
}

fn fee_total(txs: &[Built]) -> u64 {
    txs.iter().map(|t| t.fee).sum()
}

/// Builds the transactions for `dests`, sends them and writes the wallet file (the reservations must reach it, or a second run would pick the same
/// coins), then says what happened.
fn pay_all(
    w: &mut Wallet,
    node: &mut RemoteNode,
    io: &mut dyn Io,
    o: &Opts,
    pass: &str,
    dests: &[(Address, u64)],
) -> Result<(), String> {
    let plan = w
        .build_batch(&*node, &mut OsRng, dests, FeeLevel::Low)
        .map_err(|e: WalletError| e.to_string())?;
    let sent = w.send_batch(node, &plan.txs);
    save(w, need(&o.wallet, "--wallet")?, pass, o)?;
    if dests.len() == 1 && plan.txs.len() == 1 && sent.failed.is_none() {
        let built = &plan.txs[0];
        io.say(&format!(
            "sent {} to {}",
            format_coins(built.amount),
            dests[0].0.to_text()
        ));
        io.say(&format!(
            "fee {}, change {}",
            format_coins(built.fee),
            format_coins(built.change)
        ));
        io.say(&format!("transaction {}", hex_lower(&built.id)));
        io.say("it is in the node's pool; it counts once a block takes it in");
        return Ok(());
    }
    let done = &plan.txs[..sent.sent];
    let paid: u64 = done.iter().map(|t| t.amount).sum();
    io.say(&format!(
        "sent {} in {} transaction{}, fees {} in all",
        format_coins(paid),
        done.len(),
        if done.len() == 1 { "" } else { "s" },
        format_coins(fee_total(done))
    ));
    for t in done {
        io.say(&format!(
            "transaction {}: {} to {} recipient{}, {} pieces spent, fee {}",
            hex_lower(&t.id),
            format_coins(t.amount),
            t.parts.len(),
            if t.parts.len() == 1 { "" } else { "s" },
            t.tx.prefix.inputs.len(),
            format_coins(t.fee)
        ));
    }
    io.say("they are in the node's pool; they count once a block takes them in");
    if !plan.unsent.is_empty() {
        let left: u64 = plan.unsent.iter().map(|(_, v)| *v).sum();
        io.say(&format!(
            "NOT SENT YET: {} payment{} worth {}. The pieces ran out: the change of a transaction can be spent after 10 blocks, so send these then.",
            plan.unsent.len(),
            if plan.unsent.len() == 1 { "" } else { "s" },
            format_coins(left)
        ));
        if let Some(path) = &o.unsent_file {
            let text: String = plan
                .unsent
                .iter()
                .map(|(a, v)| format!("{} {}\n", a.to_text(), format_coins(*v)))
                .collect();
            std::fs::write(path, text)
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            io.say(&format!(
                "they are listed in {}: pay-many it later",
                path.display()
            ));
        }
    }
    if let Some(e) = sent.failed {
        return Err(format!(
            "{} of {} transactions were sent; the node refused the next: {e}",
            sent.sent,
            plan.txs.len()
        ));
    }
    Ok(())
}

/// A preview, and (with `--yes`) the sending, of transactions that move this wallet's own coins.
fn move_own_coins(
    w: &mut Wallet,
    node: &mut RemoteNode,
    io: &mut dyn Io,
    o: &Opts,
    pass: &str,
    txs: Vec<Built>,
    what: &str,
) -> Result<(), String> {
    let coins: usize = txs.iter().map(|t| t.tx.prefix.inputs.len()).sum();
    io.say(&format!(
        "{what}: {coins} pieces in {} transaction{}, fees {} in all",
        txs.len(),
        if txs.len() == 1 { "" } else { "s" },
        format_coins(fee_total(&txs))
    ));
    if !o.yes {
        io.say("this is only a preview: nothing was sent. Run it again with --yes to send.");
        return Ok(());
    }
    let sent = w.send_batch(node, &txs);
    save(w, need(&o.wallet, "--wallet")?, pass, o)?;
    for t in &txs[..sent.sent] {
        io.say(&format!("transaction {}", hex_lower(&t.id)));
    }
    io.say("the new pieces can be spent after 10 blocks");
    match sent.failed {
        Some(e) => Err(format!(
            "{} of {} transactions were sent; the node refused the next: {e}",
            sent.sent,
            txs.len()
        )),
        None => Ok(()),
    }
}

/// Runs one command. `args` is everything after the program name.
pub fn run(args: &[String], io: &mut dyn Io) -> Result<(), String> {
    let Some((cmd, rest)) = args.split_first() else {
        return Err(USAGE.to_string());
    };
    if matches!(cmd.as_str(), "help" | "--help" | "-h") {
        io.say(USAGE);
        return Ok(());
    }
    let o = parse_opts(rest)?;
    io.say(BANNER);
    match cmd.as_str() {
        "create" => {
            let path = need(&o.wallet, "--wallet")?;
            if path.exists() {
                return Err(format!(
                    "{} already exists; refusing to overwrite a wallet",
                    path.display()
                ));
            }
            // a new wallet starts scanning at the node's tip, so it never reads blocks that cannot hold its coins
            let birth = match (o.birth, &o.data) {
                (Some(b), _) => b,
                (None, Some(_)) => connect(&o)?.tip().map_err(|e| e.to_string())?.0,
                (None, None) => return Err(
                    "give --birth HEIGHT, or --data DIR so the wallet can ask the node for the tip"
                        .into(),
                ),
            };
            let pass = read_passphrase(
                &o,
                io,
                "Choose a passphrase (at least 8 characters): ",
                true,
            )?;
            if pass.chars().count() < MIN_PASSPHRASE {
                return Err(format!(
                    "the passphrase must be at least {MIN_PASSPHRASE} characters"
                ));
            }
            let w = Wallet::create(&mut OsRng, birth);
            save(&w, path, &pass, &o)?;
            io.say(&format!("wallet written to {}", path.display()));
            io.say(&format!("address: {}", w.address().to_text()));
            io.say(&format!("scanning will start at height {birth}"));
            show_seed(io, &w);
            Ok(())
        }
        "restore" => {
            let path = need(&o.wallet, "--wallet")?;
            if path.exists() {
                return Err(format!(
                    "{} already exists; refusing to overwrite a wallet",
                    path.display()
                ));
            }
            let seed_hex = io.passphrase("Seed (64 hexadecimal digits, hidden): ")?;
            let seed = parse_seed(&seed_hex)?;
            let pass = read_passphrase(
                &o,
                io,
                "Choose a passphrase (at least 8 characters): ",
                true,
            )?;
            if pass.chars().count() < MIN_PASSPHRASE {
                return Err(format!(
                    "the passphrase must be at least {MIN_PASSPHRASE} characters"
                ));
            }
            let w = Wallet::from_seed(&seed, o.birth.unwrap_or(0));
            save(&w, path, &pass, &o)?;
            io.say(&format!("wallet restored to {}", path.display()));
            io.say(&format!("address: {}", w.address().to_text()));
            io.say(&format!(
                "scanning will start at height {}",
                o.birth.unwrap_or(0)
            ));
            Ok(())
        }
        "address" => {
            let (w, _) = load(&o, io)?;
            io.say(&w.address().to_text());
            Ok(())
        }
        "seed" => {
            let (w, _) = load(&o, io)?;
            show_seed(io, &w);
            Ok(())
        }
        "balance" => {
            let (mut w, pass) = load(&o, io)?;
            let node = connect(&o)?;
            let report = w.sync(&node).map_err(|e| e.to_string())?;
            save(&w, need(&o.wallet, "--wallet")?, &pass, &o)?;
            let b = w.balance(&node).map_err(|e| e.to_string())?;
            let (height, _) = node.tip().map_err(|e| e.to_string())?;
            io.say(&format!(
                "node height {height}; scanned {} blocks, found {} outputs",
                report.blocks_scanned, report.outputs_found
            ));
            io.say(&format!("total      {}", format_coins(b.total)));
            io.say(&format!("spendable  {}", format_coins(b.spendable)));
            io.say(&format!("immature   {}", format_coins(b.immature)));
            io.say(&format!("reserved   {}", format_coins(b.reserved)));
            Ok(())
        }
        "pay" => {
            let (mut w, pass) = load(&o, io)?;
            let to = need(&o.to, "--to")?;
            let to = Address::from_text(to).map_err(|e| format!("--to: {e}"))?;
            let amount = need(&o.amount, "--amount")?;
            let units = parse_coins(amount).ok_or_else(|| {
                format!("--amount: `{amount}` is not an amount (digits with up to 8 decimals)")
            })?;
            let mut node = connect(&o)?;
            w.sync(&node).map_err(|e| e.to_string())?;
            pay_all(&mut w, &mut node, io, &o, &pass, &[(to, units)])
        }
        "pay-many" => {
            let path = need(&o.file, "--file")?;
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let dests = read_payments(&text).map_err(|e| format!("--file: {e}"))?;
            let (mut w, pass) = load(&o, io)?;
            let mut node = connect(&o)?;
            w.sync(&node).map_err(|e| e.to_string())?;
            pay_all(&mut w, &mut node, io, &o, &pass, &dests)
        }
        "sweep" => {
            let to = match &o.to {
                Some(t) => Some(Address::from_text(t).map_err(|e| format!("--to: {e}"))?),
                None => None,
            };
            let (mut w, pass) = load(&o, io)?;
            let mut node = connect(&o)?;
            w.sync(&node).map_err(|e| e.to_string())?;
            let txs = w
                .build_sweep(&node, &mut OsRng, to.as_ref(), FeeLevel::Low)
                .map_err(|e: WalletError| e.to_string())?;
            move_own_coins(&mut w, &mut node, io, &o, &pass, txs, "sweep")
        }
        "combine" => {
            let count = *need(&o.coins, "--pieces")?;
            let (mut w, pass) = load(&o, io)?;
            let mut node = connect(&o)?;
            w.sync(&node).map_err(|e| e.to_string())?;
            let built = w
                .build_combine(&node, &mut OsRng, count, FeeLevel::Low)
                .map_err(|e: WalletError| e.to_string())?;
            move_own_coins(&mut w, &mut node, io, &o, &pass, vec![built], "combine")
        }
        "info" => {
            let node = connect(&o)?;
            let i = node.info()?;
            io.say(&format!(
                "network {} (node version {})",
                i.network, i.version
            ));
            io.say(&format!(
                "height {} ({})",
                i.height,
                crate::daemon::short_id(&i.tip_id)
            ));
            io.say(&format!("peers {} (inbound {})", i.peers, i.inbound));
            io.say(&format!("mempool {} transactions", i.mempool_txs));
            io.say(&format!(
                "{} node{}",
                match i.kind {
                    crate::control::NodeKind::Archive => "archive",
                    crate::control::NodeKind::Pruned => "pruned",
                },
                if i.pruned_below > 0 {
                    format!(" (proofs kept from height {})", i.pruned_below)
                } else {
                    String::new()
                }
            ));
            io.say(if i.syncing { "syncing" } else { "in sync" });
            Ok(())
        }
        other => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    }
}

fn parse_seed(text: &str) -> Result<[u8; 32], String> {
    let t = text.trim();
    if t.len() != 64 || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("the seed is 64 hexadecimal digits".into());
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&t[2 * i..2 * i + 2], 16).map_err(|e| e.to_string())?;
    }
    Ok(out)
}
