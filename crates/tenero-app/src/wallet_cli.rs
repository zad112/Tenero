//! The wallet program's commands. The logic is here, behind a small [`Io`] trait, so that tests can drive it with a
//! scripted terminal; `src/bin/tenero_wallet.rs` supplies the real one (a hidden passphrase prompt).
//!
//! **Carrot and FCMP++, unaudited, and nothing here has value.** A wallet belongs to one network (`gamma`, `dev` or
//! `test`), chosen when it is made: it takes only that network's addresses and talks only to that network's nodes. The
//! seed is printed once, by `create`, and by `seed` after the passphrase; there is no word-list backup in this program
//! (the wallet app has one).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use rand_core::OsRng;
use tenero_core::hash::hex_lower;
use tenero_wallet::amount::{format_coins, parse_coins};
use tenero_wallet::{
    Address, Built, ChainView, FeeLevel, KdfParams, Network, ViewTier, Wallet, WalletError, BANNER,
};
use zeroize::Zeroizing;

use crate::client::{read_cookie, RemoteNode, COOKIE_FILE};

pub const MIN_PASSPHRASE: usize = 8;

/// What the commands need from a terminal.
pub trait Io {
    /// A passphrase, not shown as it is typed. `prompt` says what it is for.
    fn passphrase(&mut self, prompt: &str) -> Result<Zeroizing<String>, String>;
    fn say(&mut self, line: &str);
}

struct Opts {
    wallet: Option<PathBuf>,
    data: Option<PathBuf>,
    /// The node's control address; the default is the wallet's network's port on this machine.
    control: Option<SocketAddr>,
    /// The network a new wallet is for (`create` and `restore`; `gamma` unless given).
    network: Option<Network>,
    birth: Option<u64>,
    to: Option<String>,
    amount: Option<String>,
    file: Option<PathBuf>,
    coins: Option<usize>,
    unsent_file: Option<PathBuf>,
    /// `sweep` and `combine` only preview unless this is given.
    yes: bool,
    passphrase_file: Option<PathBuf>,
    /// `view-key`: which tier (`all` or `received`).
    tier: Option<ViewTier>,
    /// `integrated-address`: the payment ID (16 hexadecimal digits); a random one if not given.
    payment_id: Option<[u8; 8]>,
    /// Test only: a fast key derivation, so tests do not each spend a quarter of a second.
    weak_kdf_for_tests: bool,
}

fn parse_opts(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        wallet: None,
        data: None,
        control: None,
        network: None,
        birth: None,
        to: None,
        amount: None,
        file: None,
        coins: None,
        unsent_file: None,
        yes: false,
        passphrase_file: None,
        tier: None,
        payment_id: None,
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
                o.control = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--control: `{value}` is not ip:port"))?,
                )
            }
            "network" => {
                o.network = Some(
                    crate::config::Network::parse(value)
                        .ok_or_else(|| format!("--network: `{value}` is not gamma, dev or test"))?
                        .wallet_network(),
                )
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
            "payment-id" => {
                let b = parse_payment_id(value)
                    .ok_or("--payment-id: 16 hexadecimal digits, not all zero")?;
                o.payment_id = Some(b);
            }
            "tier" => {
                o.tier = Some(match value.as_str() {
                    "all" => ViewTier::ViewAll,
                    "received" => ViewTier::ViewReceived,
                    _ => return Err(format!("--tier: `{value}` is not all or received")),
                })
            }
            other => return Err(format!("unknown option `--{other}`")),
        }
    }
    Ok(o)
}

pub const USAGE: &str = "\
tenero-wallet: a wallet for the Tenero experimental coin (Carrot and FCMP++, unaudited, no value)

  tenero-wallet create  --wallet FILE [--network gamma|dev|test] [--data DIR] [--birth HEIGHT]
  tenero-wallet restore --wallet FILE [--network gamma|dev|test] [--birth HEIGHT]   (asks for the seed, hidden)
  tenero-wallet address --wallet FILE
  tenero-wallet balance --wallet FILE --data DIR [--control IP:PORT]
  tenero-wallet pay     --wallet FILE --data DIR --to ADDRESS --amount COINS [--control IP:PORT]
  tenero-wallet pay-many --wallet FILE --data DIR --file PAYMENTS [--unsent-file FILE] [--control IP:PORT]
  tenero-wallet sweep   --wallet FILE --data DIR [--to ADDRESS] [--yes] [--control IP:PORT]
  tenero-wallet combine --wallet FILE --data DIR --pieces N [--yes] [--control IP:PORT]
  tenero-wallet seed    --wallet FILE
  tenero-wallet integrated-address --wallet FILE [--payment-id HEX16]
  tenero-wallet view-key --wallet FILE [--tier all|received]
  tenero-wallet restore-view --wallet FILE [--network gamma|dev|test]   (asks for the view key, hidden)
  tenero-wallet info    --data DIR [--network gamma|dev|test] [--control IP:PORT]

A payment that needs more pieces than one transaction can carry (your balance is made of separate pieces, one for each payment you
received), or more than 15 recipients, is split into several transactions that spend different pieces; `pay-many` reads one `ADDRESS AMOUNT` a line. `sweep` combines your pieces (to your own address, or to --to) and `combine`
makes N of the smallest into one: both only show a preview until --yes. Pieces that come back as change can be spent after 10 blocks.

A wallet belongs to the network it was made for (gamma unless --network says otherwise): it takes only that network's
addresses (TENg..., TENd..., TENt...) and refuses a node of another network.

A VIEW-ONLY wallet sees and cannot spend or sign. `view-key` prints a wallet's view key (a SECRET: whoever has it sees
what it shows): `--tier all` (the default) sees incoming and outgoing payments and the true balance; `--tier received`
sees incoming payments only, so it shows what was received, not a balance. `restore-view` makes a view-only wallet from
one.

--data is the node's data directory (the wallet reads the node's cookie file from it). The control address defaults
to the wallet's network's port on this machine: 127.0.0.1:38352 (gamma), 127.0.0.1:28332 (dev), 127.0.0.1:18332 (test).
The wallet asks for its passphrase at a hidden prompt; --passphrase-file FILE reads it from a file instead, which is
weaker.";

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

/// The program's name for a wallet network (the same names as the node's).
fn node_network(n: Network) -> crate::config::Network {
    match n {
        Network::Gamma => crate::config::Network::Gamma,
        Network::Dev => crate::config::Network::Dev,
        Network::Test => crate::config::Network::Test,
    }
}

/// Connects to the node of `network` (its default control port unless `--control` says otherwise) and refuses one of
/// another network: a wallet must never scan, or send to, another network's chain.
fn connect(o: &Opts, network: Network) -> Result<RemoteNode, String> {
    let data = need(&o.data, "--data (the node's data directory)")?;
    let cookie = read_cookie(&data.join(COOKIE_FILE))?;
    let net = node_network(network);
    let addr = o
        .control
        .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], net.default_control_port())));
    let node = RemoteNode::connect(addr, &cookie)?;
    let theirs = node.info()?.network;
    if theirs != net.name() {
        return Err(format!(
            "the node at {addr} runs the {theirs} network, and this wallet is for {}",
            net.name()
        ));
    }
    Ok(node)
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
    let Some(seed) = w.seed() else {
        io.say("this is a view-only wallet: it has no seed");
        return;
    };
    io.say("");
    io.say("YOUR SEED (write it down on paper and keep it somewhere safe):");
    io.say(&format!("  {}", hex_lower(seed)));
    io.say("Anyone who sees it can spend your coins. If you lose both it and the wallet file, the coins are gone.");
    io.say("There is no word-list backup yet; this is the raw seed.");
}

/// The most recipients a `pay-many` file may list (a pool paying its miners: thousands, not millions).
pub const MAX_PAY_MANY: usize = 20_000;

/// Reads a `pay-many` file: one payment a line, `ADDRESS AMOUNT`; blank lines and lines starting with `#` are skipped. Strict: a bad line is an error
/// naming it, and nothing is paid.
pub fn read_payments(text: &str, network: Network) -> Result<Vec<(Address, u64)>, String> {
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
        let to =
            Address::parse(a, network).map_err(|e| format!("line {}: the address: {e}", n + 1))?;
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
    // the network of a new wallet (and of `info`): gamma unless told
    let new_network = o.network.unwrap_or(Network::Gamma);
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
                (None, Some(_)) => {
                    connect(&o, new_network)?
                        .tip()
                        .map_err(|e| e.to_string())?
                        .0
                }
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
            let w = Wallet::create(&mut OsRng, new_network, birth);
            save(&w, path, &pass, &o)?;
            io.say(&format!(
                "wallet for the {} network written to {}",
                new_network.name(),
                path.display()
            ));
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
            let w = Wallet::from_seed(&seed, new_network, o.birth.unwrap_or(0));
            save(&w, path, &pass, &o)?;
            io.say(&format!(
                "wallet for the {} network restored to {}",
                new_network.name(),
                path.display()
            ));
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
        "integrated-address" => {
            let (w, _) = load(&o, io)?;
            let id = match o.payment_id {
                Some(id) => id,
                None => {
                    let mut id = [0u8; 8];
                    while id == [0; 8] {
                        rand_core::RngCore::fill_bytes(&mut OsRng, &mut id);
                    }
                    id
                }
            };
            let a = w
                .integrated_address(id)
                .ok_or("this wallet has no integrated address")?;
            io.say(&a.to_text());
            io.say(&format!("payment ID {}", hex_lower(&id)));
            Ok(())
        }
        "view-key" => {
            let (w, _) = load(&o, io)?;
            let tier = o.tier.unwrap_or(ViewTier::ViewAll);
            let key = w.view_key(tier).ok_or(
                "this wallet does not hold that tier (a view-received wallet gives only a view-received key)",
            )?;
            io.say("");
            io.say(match tier {
                ViewTier::ViewAll => "VIEW KEY (view-all): whoever has it sees every payment of this wallet, in and out, and its balance. It cannot spend.",
                _ => "VIEW KEY (view-received): whoever has it sees the payments this wallet receives. It cannot spend.",
            });
            io.say(&format!("  {}", key.as_str()));
            io.say("Keep it as secret as what it shows.");
            Ok(())
        }
        "restore-view" => {
            let path = need(&o.wallet, "--wallet")?;
            if path.exists() {
                return Err(format!(
                    "{} already exists; refusing to overwrite a wallet",
                    path.display()
                ));
            }
            let key = io.passphrase("View key (TENview1..., hidden): ")?;
            let w = Wallet::from_view_key(&key, new_network)?;
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
            save(&w, path, &pass, &o)?;
            io.say(&format!(
                "view-only wallet ({}) for the {} network written to {}",
                match w.tier() {
                    ViewTier::ViewAll => "view-all",
                    _ => "view-received",
                },
                new_network.name(),
                path.display()
            ));
            io.say(&format!("address: {}", w.address().to_text()));
            io.say(&format!(
                "scanning will start at height {}",
                w.birth_height()
            ));
            Ok(())
        }
        "seed" => {
            let (w, _) = load(&o, io)?;
            show_seed(io, &w);
            Ok(())
        }
        "balance" => {
            let (mut w, pass) = load(&o, io)?;
            let node = connect(&o, w.network())?;
            let report = w.sync(&node).map_err(|e| e.to_string())?;
            save(&w, need(&o.wallet, "--wallet")?, &pass, &o)?;
            let b = w.balance(&node).map_err(|e| e.to_string())?;
            let (height, _) = node.tip().map_err(|e| e.to_string())?;
            io.say(&format!(
                "node height {height}; scanned {} blocks, found {} outputs",
                report.blocks_scanned, report.outputs_found
            ));
            match w.tier() {
                ViewTier::ViewReceived => io.say(
                    "VIEW-RECEIVED wallet: it cannot see what is spent, so `total` is what was RECEIVED, not a balance",
                ),
                ViewTier::ViewAll => io.say("view-only wallet: it sees the balance, and cannot spend"),
                ViewTier::Full => {}
            }
            io.say(&format!("total      {}", format_coins(b.total)));
            io.say(&format!("spendable  {}", format_coins(b.spendable)));
            io.say(&format!("immature   {}", format_coins(b.immature)));
            io.say(&format!("reserved   {}", format_coins(b.reserved)));
            Ok(())
        }
        "pay" => {
            let (mut w, pass) = load(&o, io)?;
            let to = need(&o.to, "--to")?;
            let to = Address::parse(to, w.network()).map_err(|e| format!("--to: {e}"))?;
            let amount = need(&o.amount, "--amount")?;
            let units = parse_coins(amount).ok_or_else(|| {
                format!("--amount: `{amount}` is not an amount (digits with up to 8 decimals)")
            })?;
            let mut node = connect(&o, w.network())?;
            w.sync(&node).map_err(|e| e.to_string())?;
            pay_all(&mut w, &mut node, io, &o, &pass, &[(to, units)])
        }
        "pay-many" => {
            let path = need(&o.file, "--file")?;
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let (mut w, pass) = load(&o, io)?;
            let dests = read_payments(&text, w.network()).map_err(|e| format!("--file: {e}"))?;
            let mut node = connect(&o, w.network())?;
            w.sync(&node).map_err(|e| e.to_string())?;
            pay_all(&mut w, &mut node, io, &o, &pass, &dests)
        }
        "sweep" => {
            let (mut w, pass) = load(&o, io)?;
            let to = match &o.to {
                Some(t) => Some(Address::parse(t, w.network()).map_err(|e| format!("--to: {e}"))?),
                None => None,
            };
            let mut node = connect(&o, w.network())?;
            w.sync(&node).map_err(|e| e.to_string())?;
            let txs = w
                .build_sweep(&node, &mut OsRng, to.as_ref(), FeeLevel::Low)
                .map_err(|e: WalletError| e.to_string())?;
            move_own_coins(&mut w, &mut node, io, &o, &pass, txs, "sweep")
        }
        "combine" => {
            let count = *need(&o.coins, "--pieces")?;
            let (mut w, pass) = load(&o, io)?;
            let mut node = connect(&o, w.network())?;
            w.sync(&node).map_err(|e| e.to_string())?;
            let built = w
                .build_combine(&node, &mut OsRng, count, FeeLevel::Low)
                .map_err(|e: WalletError| e.to_string())?;
            move_own_coins(&mut w, &mut node, io, &o, &pass, vec![built], "combine")
        }
        "info" => {
            let node = connect(&o, new_network)?;
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

fn parse_payment_id(text: &str) -> Option<[u8; 8]> {
    let t = text.trim();
    if t.len() != 16 || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 8];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&t[2 * i..2 * i + 2], 16).ok()?;
    }
    (out != [0; 8]).then_some(out)
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
