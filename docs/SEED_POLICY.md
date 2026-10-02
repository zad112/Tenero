# Seed policy: how a brand-new node chooses its first peers, and how many seeds are needed

**Status: first version, by the author, not an audit. Unaudited, one node, not for real value.** This is threat C1 of `THREAT_MODEL.md`
(eclipse). The numbers below are from a simulation that drives the REAL engine against scripted peers (`crates/tenero-net/tests/eclipse_sim.rs`),
so they show what the engine does under a stated model, **not what an attacker on the real Internet can do**. Read "What this does not show".

## The rules (all in `engine.rs`, tests in `bootstrap_rules.rs`)

1. **Wait for every seed.** A node with no tried address (a first start) dials ONLY its configured seeds. It dials addresses those seeds
   told it about only after every seed *group* (an IPv4 /16, an IPv6 /32) has answered its address request, or after
   `bootstrap_wait_ms` (20 s) have passed. Two seeds in one group count as one. A dead seed therefore costs a first start one wait of 20 s.
   Setting `bootstrap_wait_ms = 0` turns the rule off.
2. **A fair share per seed.** While the first `outbound_target` (8) peers that the seeds did not themselves provide are being chosen, the
   addresses that descend from one seed may fill at most `max(2, outbound_target / number of origins)` of them. An address's *origin* is
   the seed it ultimately came from (an address told by a peer that a seed told us of has that seed's origin), **not** the peer that told
   us, because every peer an attacker owns is a new source for free, but only the seed list starts an origin. The seeds' own addresses are not
   limited. Peers beyond `outbound_target` are extra and are not limited. `max_outbound_per_source = 0` turns it off.
3. **Corroboration (`prefer_corroborated`): off.** Preferring addresses told by two or more source groups was built and measured; it helps
   the attacker (below), so it is off.

## What was measured

Model: a new node, 8 outbound slots, one honest population of 2,000 addresses in many groups, an attacker with 500 addresses in 40 groups.
Honest seeds answer after 3 s; hostile seeds answer at once (the attacker's best timing). Every dial succeeds and a hostile peer behaves like an
honest one (the worst case). Seeds hang up after answering. The measure is the share of the node's **first 8 dialled non-seed peers** that are hostile.
100 trials per cell (noise about ±3 points). Without the rules ("baseline") the engine behaved as it did before 2026-10-02.

| honest seeds | hostile seeds | baseline | with the wait | share of the seed list that is hostile |
|---|---|---|---|---|
| 3 | 1 | **100%** | 27% | 25% |
| 3 | 3 | **100%** | 35% (one shared list) / 51% (own lists) | 50% |
| 6 | 3 | **100%** | 22% / 34% | 33% |
| 2 | 3 | **100%** | 44% / 60% | 60% |
| 1 | 4 | **100%** | 60% / 79% (16% of nodes get all 8 hostile) | 80% |

* **At baseline a single fast hostile seed took every one of a new node's first dials**, whatever the number of honest seeds. This was a real hole.
* **With the wait, the attacker's share is about his share of the seed list** (the table's last column), whatever his speed or how many addresses
  he supplies. So the number that matters is the number of *independent seed operators in different network groups*, and what fraction of them an
  attacker can run.
* **Waiting for a quota of seeds does not work** (it was built first and failed): hostile seeds answer first and fill the quota. Only waiting for
  all of them, or a timeout, puts the honest answers into the choice.
* **The per-seed fair-share limit (rule 2) showed no measurable gain** over the wait alone, in the first wave or after every peer drops and the
  node refills (about 25–50% hostile with either; the differences were inside the noise or went both ways). It is kept because it is the only
  thing that bounds an attacker who feeds a node new addresses through the peers it has already connected to, and it costs little; **its benefit
  is argued, not measured.** The injected-fault sweep shows the code does what it says, not that it is needed.
* **Preferring corroborated addresses made things worse** against an attacker whose seeds hand out one shared list: honest seeds' random samples
  rarely overlap, a coordinated attacker's lists overlap completely, so "told by two sources" picks out the attacker (6 honest + 3 hostile:
  47% hostile with it, 22% without).
* **Nothing here helps when most of the seed list is hostile** (the 1-honest-4-hostile row). That is stated as a test so no one mistakes the policy
  for a defence.

## What this means for running seeds

* Several operators, in **different network groups**, none of them able to be controlled by the same party. Count operators, not machines.
* A list that is mostly honest gives the attacker only his share; a list where an attacker holds a majority gives him a majority.
* A seed answering slowly (more than 20 s) is treated as dead for the first start.
* Pinned peers (`trusted_peer`), anchors and stale-tip detection (see `THREAT_MODEL.md`) are what remain once a node has run: a node that has
  already connected needs seeds much less, and an operator who got a peer's address from a person they trust is not subject to any of this.

## What this does not show

* **An attacker who owns a large part of the honest population**, or who can block a node's connections to honest addresses, or who poisons the
  seed list in the release itself. The model has none of these.
* **Network groups are cheap for an attacker** (cloud hosts are in many /16s); the model gives him 40 and the result did not depend on it, but it was
  not varied.
* **Latency**: only "hostile first by 3 s" was measured. An honest seed slower than the 20 s wait is lost.
* **The model's honest peers never leave or lie**, no peer is slow, and every dial succeeds. A real network is messier.
* **A node that is already eclipsed** (a saved address book full of an attacker's addresses) is not helped by any of this.
* The measurement is of the first dials, and of one refill after every peer drops. Longer-term behaviour (eviction, address-book poisoning over
  weeks) is not simulated.

## Tests and faults

* `bootstrap_rules.rs`: 16 tests (the wait, its timeout to the millisecond, off switch, groups, restart, the fair share, descendants of
  descendants, seeds never limited, peers beyond the slots, the address-book origin and reporters).
* `eclipse_sim.rs`: 6 tests that state the measured findings with margins, and two ignored measurements that print the tables above
  (`cargo test --release -p tenero-net --test eclipse_sim -- --ignored --nocapture`; `TENERO_SIM_COORDINATED=0` for seeds with their own lists).
* A 29-fault injected sweep: **22 were caught the first time and 7 survived**. One survivor was dead code (an anchors clause that could never be true;
  removed), four were real gaps in the tests of the limit (now closed, all four caught), and two cannot change behaviour (a check that only matters
  for peers that cannot exist while only seeds are dialled, and a guard that a deduplication already makes redundant).
