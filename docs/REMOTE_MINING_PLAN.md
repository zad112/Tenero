# Remote mining: a miner that uses someone else's node (a PROPOSED design, nothing is built)

**Status (2026-10-07): the first step is BUILT, tested, and was run with a real GPU on one computer and across the internet to a rented server (a private `dev` chain, one miner, one address; `docs/README_FACTS.md` has the numbers and what was NOT measured). Earlier status (2026-10-06): built and tested on one computer: the node's miner service (`crates/tenero-app/src/miner_service.rs`, settings `miner_listen`, `miner_key`, `miner_max`, `miner_rate`), the miner's `--node` and `--key`, and the miner's checks of every template (`remote_miner::check_template`). Everything after it (open to anyone, a pool and its shares, asking two nodes) is NOT built.** Tenero is unaudited and
experimental; this document is a plan and a list of risks, not a promise. Wallet access to a remote node is a later step
(see "After the miner") and is not designed here.

## Why

Today a miner needs a node it can reach on **this computer**: `tenero-miner` talks to `tenerod` over the control port, which
accepts loopback addresses only (`docs/CONTROL_PROTOCOL.md`), and the node holds a 4 GiB dataset, a copy of the chain and
a port to the internet. Some people want to mine without running that. The aim: **a GPU machine that mines on a node run by
someone else, with the node's operator choosing to offer that service.** The reward still goes to the miner's own address.
There is no pool and no share accounting in this step: this is solo mining, as today. The end goal is pools (see below).

## What the miner uses today (read from `crates/tenero-app/src/remote_miner.rs`)

Three requests of the control protocol: `info` (height, tip id, is the node syncing), `block_template` (the node builds a whole
block, its coinbase paying the payout the miner gave for that height, plus the target) and `submit_block` (the node
validates it completely and tells its peers). The miner hashes the header and returns the block. It never needs the
chain, the wallet, the mempool or the network.

## What must NOT be done: opening the control port

The control port can read the whole chain, send transactions and stop the node, behind one cookie. It is not encrypted. It
stays loopback only. A remote mining service is a **separate listener** with its own small allowlist, so a mistake in it
cannot reach `stop`, `submit_tx` or the cookie.

## The design

1. **A separate listener on the node**, off by default, switched on by the operator (`serve_miners`, an address and port; the
   port to be chosen). Settings for the most miners at once, a rate limit per address and an optional key. The operator
   can leave it closed; nothing else changes for a node that does.
2. **Allowlist: only `info`, `block_template` and `submit_block`.** Any other request kind closes the connection without
   an answer. The answer to `info` is trimmed to what a miner needs (height, tip id, syncing, network, version); the
   node's peer counts and pruning state are not given to strangers.
3. **Encryption by the Noise channel the peer-to-peer code already has** (`snow`, already approved; no new
   dependency). Integrity matters more than secrecy here (see the trust section): it stops a person on the path from
   changing a template or dropping a found block, and hides the miner's traffic from that person. Anyone may connect
   without a key; if the operator sets a **pre-shared key**, only those who hold it can (the case of one's own server).
4. **The miner checks every template instead of trusting it** (new code in `remote_miner.rs`; today it checks only the height of the template, and I found no check of the coinbase or `tx_root`, which is safe only because the node is local):
   * the **coinbase pays the miner's payout** for that height, for the whole reward. Without this check a hostile node
     could put its own address in the coinbase: the miner would hash a block whose reward goes to the node. This is the
     one **theft** risk of the whole feature, and it must have a test that fails without the check;
   * `tx_root` in the header matches the coinbase and transactions in the body, so the body cannot be swapped after the
     work is done;
   * height, previous id and version are what `info` said, and the timestamp is near this computer's clock;
   * the target is not trusted blindly: a miner can check that a found block meets the target it was given, but it
     **cannot** check the target is the right difficulty, because that needs the chain (see "What a hostile node can still do").
5. **The miner's own connection logic stays**: pause while the node is syncing, replace the job when the tip moves, reconnect with
   back-off. A new address form (`--node HOST:PORT`, plus `--key` when needed) replaces `--control`/`--data` for the remote case;
   the local way keeps working unchanged.
6. **Rule 1 and 6 apply**: a new message set, or the reuse of the control frames behind the allowlist, gets golden vectors from
   the Python reference and a test per rule (see "Tests").

### Reuse the control frames, or a new message set?

Recommended: **reuse** the control protocol's frames and the existing `info`, `block_template` and `submit_block` encodings (and
their vectors), with the listener enforcing the allowlist. Less new code, less new surface. The cost: the protocol doc
must say that the same bytes now also cross a network, and the `info` answer gets a trimmed variant. Decision 1 below.

## What a hostile node can and cannot do to a miner

| What | Can it? | Why / defence |
|---|---|---|
| Steal the reward by paying itself | **No**, if the miner checks the coinbase (design point 4) | The check is the defence; **without it, yes**. |
| Swap the block body after the work | No | `tx_root` check; the block id binds everything. |
| Make the miner waste work: a stale tip, a slow node, a block it never relays, a wrong target | **Yes** | The miner cannot see the real chain. Mitigation later: ask two nodes and compare tips; not designed here. |
| Keep the miner on a fork (an eclipse) | **Yes**, if it is the only node | Same: two or more nodes, from different operators. |
| Choose which transactions are in the block, leave them out | Yes (censoring) | The node builds the body; a miner using a remote node gives that up. |
| Learn the miner's IP address and its payout output | Yes | The coinbase output is public on the chain anyway; the IP is what the operator sees. Say so in the README. |
| Make the miner mine on the wrong network | Partly | `info` carries the network and chain id; the miner must refuse a mismatch. |

## What a miner can do to the node (the operator's side)

Each `block_template` costs the node a block build; each `submit_block` costs a complete validation, proof of work included.
A public service will be abused, so, before it is switched on in any release:

* limits per connection and per address (requests per minute, templates per tip, submissions per minute), a hard cap on the
  number of miners, and a request size cap (the node builds templates of at most 2 MB; the limit for a submitted block must be checked against the consensus size rule);
* the template's transaction selection cached per tip, so a miner asking again only costs a new coinbase;
* a miner that sends malformed or unrequested messages is dropped and its address banned for a time, as the peer code does;
* the service is **off while the node is syncing** (it already answers an error).

None of this is measured. **How many miners one small server can serve is unknown**; the first release of this should say
"tried with N miners" with a real N.

## Tests (each rule gets one; mutation-checked like the discovery tests)

* a request outside the allowlist closes the connection, for every other request kind;
* a template whose coinbase pays someone else is refused by the miner (and the test fails if the check is removed);
* a template whose `tx_root`, height, previous id or network is wrong is refused;
* the pre-shared key: wrong key refused, none refused when one is set;
* rate limits and the cap on miners (a flood does not starve the node's network code);
* a full run in the simulator and on one machine: a miner in another process mines a block on a node that only has the
  service port, and it is accepted by a third node;
* then, on the owner's machine only (rule 5): a real GPU miner against a node on the VPS, with the speed and the number of
  miners measured, not claimed.

## Decisions (the owner, 2026-10-06)

1. **Reuse the control frames** behind the allowlist: yes.
2. **Open to anyone** when the operator turns the service on, the key optional: yes.
3. **Names and port (the author's choice, changeable):** the node settings `miner_listen` (an address and port, empty means off, in
   the style of `listen`), `miner_max` (the most miners at once), `miner_key` (the optional pre-shared key, 64 hex digits)
   and `miner_rate` (requests per minute per address); the default port **38334** (38333 is the peer port, 38332 the control
   port). The miner takes `--node HOST:PORT` and `--key`.
4. **First step: mining on your own node over the internet, a key required, before strangers can use it:** yes. How to test it
   is open: see "How the first step is tested".
5. **The seed server never offers it:** decided. It stays a seed.

## The goal is pools, and a pool is a different trust model

The owner's end goal (2026-10-06) is that miners connect to **mining pools** once those exist, so that most people need no node
of their own. This document designs only the part that comes first, **solo mining on a remote node**, and a pool differs
in ways that change the rules above:

* **Who is paid.** In a pool the coinbase pays the POOL, not the miner. The check "the coinbase pays the miner's own
  payout" (design point 4) cannot be used there; the pool's honesty is what the miner trusts, and the pool pays the miner
  out of band or by a later transaction. A miner must never be able to confuse the two modes: **solo mode keeps the
  strict check; a pool mode is a separate, explicit choice** that says "this server keeps the reward".
* **Shares.** A pool needs the miner to send **shares** (solutions to an easier target than the block's) so it can count work. That is a
  new request and a new rule (what a valid share is, the pool's difficulty per miner, duplicate and stale shares). `block_template`
  and `submit_block` alone do not do it.
* **A job, not a whole block.** Pool protocols usually give the miner only a header to work on and let the pool build the
  body. The miner then cannot check `tx_root` against a body (it has none); it trusts the pool for the body.
* **Payout accounting is the pool's problem**, with its own risks (a pool that does not pay, steals or is attacked). Nothing in
  Tenero can protect a miner from that, and the README would have to say so.
* **The pool needs a node.** So the node-side service here (a node that builds templates and accepts blocks) is also
  what a pool operator's software would sit on. That is why it is built first; the pool and the share protocol would be a
  separate design document, and **a pool does not exist for Tenero today, and no one has said they will run one**.

What stays the same for both: the separate listener, the allowlist idea, Noise, the rate limits and the opt-in operator.

**The pool interface is written down first, as a draft standard: `docs/POOL_PROTOCOL.md`** (owner's rule, 2026-10-06: set the standard before any pool exists, so every pool and every miner that follows it works together). It defines the messages (`hello`, `job`, `submit_share`, `share_result`, `set_share_target`), a nonce range for each miner, the share rules, how the miner tells the solo service from a pool (a different handshake prologue and a different flag, `--pool`), what a miner can and cannot check, and a conformance kit (`tenero-poolcheck`) so that "compatible" can be tested. Nothing in it is built.

## How the first step is tested

The owner asked how, since the seed is not to serve miners. The author's suggestion, in order:

1. **One computer, two processes:** a node with `miner_listen 127.0.0.1:38334` on its own data directory, and `tenero-miner --node
   127.0.0.1:38334 --key ...` against it, on the `test` network (SHA-256) first, with no GPU. This is automatic (a test) and needs no
   second machine.
2. **The WSL Ubuntu on the owner's PC** as the "other" computer (CLAUDE.md: it exists; it has no Windows drives mounted). A node there and the
   miner on Windows across the virtual network. It is a different operating system and a real socket between two stacks, but it is
   **not the internet**.
3. **A real internet run needs a second machine that is not the seed.** Options for the owner to choose between then: a
   small second server, a friend's machine, or the PC with its router's port forwarded and the miner on a phone hotspot's laptop.
   Running a second node on the seed's own server is possible but puts a 4 GiB dataset next to the seed, and the seed's free
   memory has not been measured for it.
4. Rule 5 applies: the speed and the number of miners are numbers from a run, not claims.

## The stages (the owner, 2026-10-07)

**Stage 1: the miner service (done and measured once).** The solo service, the miner's template checks, a GPU run on one computer and across the internet to a rented test server (`docs/README_FACTS.md`). Still open there: the WSL run (low value now), more than one miner, floods, the epoch boundary.

**Stage 2: the pool standard, made testable** (`docs/POOL_PROTOCOL.md`, decisions made 2026-10-07: nonce prefix, optional tip witness, the pool key pinned by default, port 38335, job declaration included as an optional capability). In order: (1) golden vectors for every message, made by an independent Python reference (CLAUDE.md rules 1 and 6); (2) a small reference pool, for tests only, on the SHA-256 test chain first; (3) `tenero-miner --pool`, checked against it; (4) `tenero-poolcheck`, for a pool developer to run against their pool; (5) hostile tests (a pool that lies about shares or sets a bad target, a miner that floods or sends junk shares or junk declarations); (6) a real GPU run against the reference pool with the share rate and the pool's cost per share measured; (7) a threat-model entry before anything is released.

**Stage 3: our own pool, on the test server, so that people with less hardware can take part in the tests and receive test coins.** Nothing is built. What it needs, and what must be decided first:

* **A pool program** (`tenero-pool`, working name) that runs beside a node on the test server: it builds the jobs from the node (the pool's payout in the coinbase), checks every share (it needs a node's 4 GiB dataset for that), keeps the share accounting in the existing `redb` store (no new dependency), finds blocks and hands them to its node, and pays its miners.
* **Which network:** the testers are on **alpha**, so the pool must serve alpha, which means a second node on the test server that syncs alpha from the seed (the server's 16 GB holds two nodes, 8 GiB, and the pool). The private `dev` node there now is for the stage 1 and 2 tests. **Decision for the owner.**
* **How miners are paid:** the author suggests the simplest scheme for a test, **proportional by shares for each block found** (every share in the round earns a part of the reward), **0% fee**, a **minimum payout** so that payments are not dust. A fairer scheme (PPLNS) can come later. **Decision for the owner.** The pool's reward matures after 60 blocks like any other, so payouts lag.
* **The payout wallet:** a wallet of its own, **not the owner's**, kept on the server so that it can pay. That puts spending keys on a public server, so it holds only the pool's mining rewards, which are test coins. Today's wallet builds one payment to one address at a time (`Wallet::build_payment`), so a payout round is one payment per miner; batching several miners into one payment is a wallet change that is NOT made.
* **A public listener,** port 38335 open to everyone: it needs the limits of the miner service (miners at once, per address, requests, frame size, timeouts) and more (shares per second, bad shares in a row, declarations), a ban for repeat offenders, and its own threat-model entries before it is opened. How many miners it carries is unknown and must be measured, not claimed.
* **What the README and the screens must say, plainly:** in pool mode the reward goes to the **pool**, run by the author; the pool pays you on its own rules, and you trust it to; this is a test network and the coins have **no value**; the pool is **unaudited** and one person's server, and a pool makes the network less decentralised.
* **What the pool owner owes:** a page that publishes the fee, the scheme and the smallest payout, and a record of payments that miners can check against the chain.

## The pool option in the miner and the app (CLI and GUI)

The owner's requirement: the miner can mine on a pool, **ours is the default, and any other pool can be entered.** Nothing is built. The design:

* **CLI:** `tenero-miner --pool HOST:PORT --pool-key HEX --address tni1...` for a pool of one's choosing, and `--pool default` for the program's own (a built-in address and key, as the built-in seed has, in a list like `ALPHA_SEEDS`; more than one may be given, tried in order). The pool's key is **pinned**: a miner refuses a pool whose key is not the one it was given. `--pool` and `--node` cannot be combined, and the pool path says on the screen, every time it starts, that **the reward goes to the pool and not to the address given**. `--address` is where the pool is told to pay. `--witness HOST:PORT` is the optional tip check.
* **GUI:** a Mining-tab choice between **Mine on a pool** and **Mine alone with my own node** (today's way), with the pool's address and key fields (the program's own pool filled in), a worker name, a list of other pools the user has added, and the same plain statement about where the reward goes. In pool mode **no node is started**: the app runs only the miner, so a user with little memory (a node holds 4 GiB) can take part. The payout address is the wallet's own address, filled in. The status shows the pool's name, shares accepted and refused, and the pool's last answer; a balance shown by the pool is NOT in version 1 (a miner reads it on the pool's page).
* **Which is the default mode:** pool mode becomes the default **only when our pool exists, has been tested and a release carries its address and key**. Until then the app and the miner stay solo, as today. A release that changes the default must say so in its notes and in the pinned issue.
* **Built-in default pool and its key:** needs a release, like the seed's address did. The key of the default pool is public.
* **Tests:** the miner's pool path against the reference pool and then against ours (stage 2 and 3), the app's pool settings and the "no node started" rule in `core.rs` tests (the app's logic is tested without a window), and a check that a pool miner and a solo miner cannot be confused (the handshake name).

## After the miner

The wallet is harder: it must ask a node about outputs and key images, and the node learns who is asking. The questions
are: scan whole blocks (more private, heavy, today's `blocks` request) or ask for particular outputs (light, tells the
node which are yours); the node can lie about the chain or leave out a payment; and the interim wallet scheme is not
Carrot. That deserves its own document and its own threat-model entry, as Monero's remote nodes carry the same warning
(less private). It is not in scope here.

## Threat model

This feature needs its own entry in `docs/THREAT_MODEL.md` before any release: the reward-theft check, the abuse of a public
service, the eclipse of a single-node miner and the operator's exposure. It has none yet.
