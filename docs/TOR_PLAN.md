# Tor (protocol version 3, with Dandelion++)

Status: **built, 2026-10-11, for the release after 0.3.0-gamma.1; unreviewed.** Owner's decisions (2026-10-11): an outside
Tor program (no Tor code in Tenero, no new dependency), outbound connections through it, a node's own transactions over
it, a "Tor only" mode, a `.onion` address for the seed, and the `.onion` address format in protocol version 3 now (the
release already forces an upgrade, so this costs none later). Not consensus.

## What it is for

A node connects to its peers directly, so every peer knows its IP address, and the first node to relay a transaction
knows where it came from (Dandelion++ makes that hard to read off the network, `docs/DANDELION_PLAN.md`, but the first
hop still sees it). Tor hides the IP address from the peers: they see a Tor exit, or (for a `.onion` peer) nothing.

## How (the pieces)

* **Tor itself is a separate program** the operator installs and runs: the "Tor Expert Bundle" (`tor.exe`, which listens
  on `127.0.0.1:9050`) or the Tor Browser (`127.0.0.1:9150` while it is open), or `tor` from a Linux distribution. Tenero
  talks to its SOCKS5 port. Tenero contains no Tor code and no new dependency: the SOCKS5 client (RFC 1928, with the
  username/password of RFC 1929 used only to keep connections apart) is written here, `crates/tenero-net/src/socks.rs`;
  it is not cryptography (the connection inside it is still Tenero's own Noise channel).
* **`proxy = 127.0.0.1:9050`** (`tenerod`): every `.onion` peer is dialled through Tor; other peers still directly. Each
  connection gets its own random SOCKS username, so Tor gives it its own circuit (Tor's `IsolateSOCKSAuth`, on by
  default): two peers cannot be linked as one node by sharing a circuit.
* **`tor_only = yes`**: EVERY outbound connection goes through Tor (an IP address too, through an exit), the node does not
  listen on a public address, and it advertises no IP address. Peers then never learn the node's IP. (It can still
  listen as a `.onion` service.)
* **The node's own transactions** go to a `.onion` peer first when it has one (`Engine::route_own`, Monero does the same):
  they leave the node through Tor, and Dandelion++ covers the relaying on the open network from there. With no `.onion`
  peer they take the Dandelion++ stem as before (through Tor anyway in `tor_only` mode). **Only a peer the node dialled**
  is used: relayed stems go only to outbound peers, so a stem sent to a peer that came in through the node's `.onion`
  service could only be the node's own, and that peer would know (a tester found this in 0.4.0-gamma.1; fixed after it).
* **A separate identity through Tor** (fixed after 0.4.0-gamma.1, where a tester found the two linked): the Noise static key
  is shown to the other side in every handshake, and `hello` carries a nonce, so one key and one nonce on both kinds of
  connection would let anyone who connects to the `.onion` address and to the IP address see that they are one node. So
  every connection through Tor (dialled through the proxy, or in through the `.onion` service) shows **another key and
  another nonce**, both made new at each start (`NetConfig::tor_key`, `EngineConfig::tor_nonce`). The checks that use the
  nonce (a connection to ourselves, two links to one peer) compare a link only with links of its own kind: otherwise each
  would be a question anyone could ask ("does it hang up if I send its direct nonce over Tor?"). The price: a `tor_only`
  dial that reaches the node's own IP address is not recognised as itself by its nonce.
* **A `.onion` service** (`onion_listen = 127.0.0.1:38363`, and `advertise = xxxxx.onion:38353`): Tor forwards the
  service's port to this local address, and peers that reach the node through its `.onion` address arrive there. They all
  come from `127.0.0.1`, so the node does not hold one against another: each is its own host for limits and bans (a ban
  only ends that connection; a peer behind Tor can always come back with another, as on any Tor service). The seed on the
  owner's server is set up this way (`docs/RUNNING.md`, "Tor").
* **Addresses.** `seed`, `trusted_peer` and `advertise` may be `xxxxx.onion:PORT` (version 3 onion addresses: 56
  characters). The address book keeps `.onion` addresses only when the node has a `proxy` (it could not dial them). Each
  `.onion` address is its own network group.
* **The wallet app** has two settings: the Tor proxy, and "Tor only". The pool miner has `--proxy` (a pool may be
  `.onion`). The wallet itself talks only to the node on the same computer, so it needs nothing.

## The wire (`docs/WIRE_PROTOCOL.md`)

In version 3 an address in `addrs` starts with its **type**: `1` an IP address (16 bytes, IPv4 as `::ffff:a.b.c.d`), `2` a
version 3 onion address (its 35 bytes: the service's public key 32, a checksum 2, the version 1, as in its 56 characters).
Then the port and `last_seen` as before. Tenero does not check an onion address's checksum (that needs SHA3, a new
dependency): Tor checks it when the address is dialled, and refuses a wrong one.

## What it does not do, said plainly

* **It is only as good as the Tor program and its configuration.** Tenero does not check that the proxy really is Tor.
* **Not in `tor_only` mode, the node still connects directly** to every peer that is not `.onion`: they see its IP. Only
  its own transactions are kept off those connections when it has a `.onion` peer.
* **A peer behind Tor cannot be banned for long** (see above), so a node with an `.onion` service relies on its connection
  limits against floods.
* **Timing.** Tor hides addresses, not timing: a peer that sees a transaction arrive over a Tor circuit a few hundred
  milliseconds after it was made still learns that it is new.
* **Few nodes, few `.onion` peers.** On `gamma`, at first only the seed will have a `.onion` address.
* **Payments are another layer**: amounts and addresses are Carrot's and FCMP++'s business (unaudited).
