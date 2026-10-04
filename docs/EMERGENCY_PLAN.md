# Emergency plan: what happens when a rule is wrong (a DRAFT, E8 of `THREAT_MODEL.md`)

**Status: the owner's decisions are in (2026-10-04), the rewind command is built, and the drill has been run once (2026-10-04, section 9: one scenario, a wrong reward rule, run by the author on one machine with the owner watching the results). It found three faults, now fixed. Still not rehearsed: the other kinds of emergency, and a run with the people the plan names doing the steps themselves (today that is the owner alone, who has read the drill's commands but not typed them).** A plan written by the author (2026-10-03). Finishing it was M11.0 in `M10_M11_PLAN.md`.
It exists because of `M8_PLAN.md` section 7: a consensus bug on a live chain cannot be fixed quietly, and the time to decide who does what is
before it happens, not during. The project is still an unaudited test network with no value; this plan is for that network, and is written so
that it can grow into the plan for a network that matters.

## 1. What counts as an emergency

Any of these, found by anyone (a tester, the owner, a fuzzer run, a reviewer):

1. **A consensus bug**: two builds, or two nodes, disagree about whether a block is valid (a chain split); or a block that breaks a rule in
   `CONSENSUS_V2.md` is accepted (inflation, a double spend, a spend without a valid proof).
2. **A key-handling bug**: a seed, a spend key, a passphrase or the control cookie can leak (into a log, a file, the network) or be guessed.
3. **A remotely exploitable crash or hang** of `tenerod` by an unauthenticated peer.
4. **A difficulty failure**: the chain has stopped (no block for a long time) or is racing (blocks far faster than aimed for). See
   `THREAT_MODEL.md` E3 and E4: a timestamp-manipulating minority, or a large miner leaving, can cause either.

A bug that only affects one user (a wallet that will not open) is not an emergency; it is an issue.

## 2. Who decides (DECIDED by the owner, 2026-10-04)

**The owner is the only maintainer and the only person who can say "stop mining"; nobody else is named.** That is the plan's weakest point, stated plainly: one
person who may be asleep, on holiday or unreachable. While the network is a handful of testers with nothing of value on it, that is accepted. **It must be
reopened before anyone else's money is on the network, or before the owner will be away for days with testers running** (then: name someone, or tell testers the
network is paused).

## 3. The first hour: stop the damage

In this order, the first two before anything is understood:

1. **Stop your own node and miner** (`tenerod stop`; for a miner process, its own stop). A node that keeps running keeps spreading a bad block.
2. **Say so, in the channel testers watch** (DECIDED 2026-10-04: **a pinned issue on the project's repository**; it does not exist yet, and the owner creates it
   before the first tester joins; its link goes in the README and the release notes), in plain words: what is known, what is not, and "stop mining and stop your nodes until told". No theories, no blame.
3. **Keep the evidence**: the logs (they carry no secrets: `THREAT_MODEL.md` G6), the data directory (copy it, do not repair it), the exact
   build (`--version`: the commit), the block id and height where it went wrong.
4. **Do not patch in public before the fix is understood**, for a bug that lets someone steal or inflate; do announce that there is a bug.

## 4. Understand it

- Reproduce it with a test first (a new vector or a regression test), so that the fix is checked against the failure and not against a belief.
  A consensus rule's fix starts in the reference (`tools/make_vectors_v2.py`), then `CONSENSUS_V2.md`, then the Rust code, then the vectors
  (CLAUDE.md rule 1), and says in the commit what changed and why.
- Find **how far back** it goes: the first block that should have been refused. That decides the choice in section 5.

## 5. The choice: roll forward or reset (DECIDE per case, with these defaults)

| the situation | default |
|---|---|
| the bad block is recent and nobody has spent from it | a **fix and a coordinated rewind**: a new build that refuses the bad block; testers rewind to the last good block with **`tenerod rewind`** (built 2026-10-04: the node stopped, a copy of the data directory first, `--to HEIGHT`; `docs/RUNNING.md`) |
| the chain split but both sides are valid under different readings of a rule | decide which reading is the rule (the document, then the reference), **fix the other build**, and let the shorter side re-sync |
| inflation or a forged spend already happened and was spent on | **reset the network**: a new genesis (a new chain id), announced, with the reason. This is acceptable ONLY because the network has no value; **on a network with value this choice is the hardest one and this plan must be rewritten first** |
| a key-handling bug | **a new build first**; testers move their coins (they have none of value) to a new wallet made with it; the old wallets are called compromised |
| the chain has stopped (hash rate gone) | wait, or restart with a lower starting difficulty by a new genesis (see E4: a hundredth of the hash rate takes about 9 hours to recover) |

Resets are expected on this network (`M10_M11_PLAN.md` M11.2 says so), which is what makes these defaults tolerable. They are not tolerable later.

## 6. Tell people what to do

One message, in the same channel, with: the build to install (a version and a SHA-256 of the archive), the exact steps (stop, replace, start,
and for a reset: delete the data directory, the new chain id and where the seed node is), what happened to their coins (for a test network:
"nothing of value was lost"), and when the next update will be, even if it is "in two hours, no news".

## 7. After it

- A written account within a week: what was wrong, since when, how it was found, what the fix is, what test now catches it, what was not
  understood. Items found by someone else are credited to them (`THREAT_MODEL.md` section 6). Flattering or not, it goes in the threat model.
- The release that carried the bug is **withdrawn, not patched silently** (`M10_M11_PLAN.md` M11.4's rollback rule).

## 8. What this plan does not have (gaps, in order of importance)

1. **No second person** (decided 2026-10-04: the owner alone, section 2) and **the channel does not exist yet** (decided: a pinned issue; the owner creates it before the first tester).
2. ~~No rewind tool~~ **Built 2026-10-04: `tenerod rewind`** (an offline command with a dry run by default; tested on real node data). Its limit: it removes blocks on THIS node only; a peer that still has them offers them again, so a rewind is always paired with a build that refuses the bad block, and every tester runs it.
3. **No upgrade mechanism, DECIDED 2026-10-04: none will be built; a rule change is a reset** (a new genesis and chain id, announced as in sections 5 and 6). A planned rule change, such as the timestamp rule of E3, goes in with the fresh chain of M11.2. This is acceptable only while the network has nothing of value; the plan must be rewritten before it has.
4. ~~Nothing has been rehearsed.~~ **One drill done 2026-10-04 (section 9).** It covered ONE scenario (inflation by a wrong reward rule, found early, nobody had spent from it) and not the others in section 1 (a key leak, a remote crash, a stopped chain). Drill the others before they matter, and repeat this one with the owner typing the steps.

## 9. The drill, 2026-10-04 (what was done, what it found)

**What was done.** Run by the author (an AI assistant) at the owner's request, on this machine, on a scratch SHA-256 test network with two nodes (and a third that
joined late), in about ten minutes of wall clock. The "broken build" was this commit's source with one deliberate bug: from height 11 on, the reward rule paid
1 atomic unit too much. The good build refused such blocks. **The scenario:** both nodes mined with the good build to height 19, then ran the broken build, which
mined to height 30 (blocks 20 to 30 are bad). Then sections 3 to 6 were followed in order, with timestamps (kept in `drill.log` outside the repository):

| step | what happened | time since the "discovery" |
|---|---|---|
| 3.1 stop your own node and miner | both stopped cleanly with `tenerod stop`; no process left | 4 s |
| 3.3 keep the evidence | data folders and logs copied (0.7 MB) | 4 s |
| 3.2 announce | a message drafted (`announcement-1.txt`: what is known, what is not, stop, keep your folder, next update). **Not posted**: no pinned issue exists yet | 15 s |
| 4 find how far back | a fresh node on the good build synced from a broken node and refused block 20. **This took about 2.5 minutes, nearly all of it because the log did not say which block it was (fault 2)** | 2 min 50 s |
| 4 reproduce with a test | the existing test suite, run on the broken source, **fails 5 tests** (3 emission vector tests, 2 validator tests): for a bug of this kind the plan's "test first" is already met by the suite | 3 min 42 s |
| 5 choose | the bad block is recent and nothing was spent from it: the fixed build plus a coordinated rewind | 3 min 42 s |
| 6 tell people | a release archive with its SHA-256 and the message (`announcement-2.txt`: the five steps) written | 3 min 46 s |
| 6 the testers' steps | following the message literally on both nodes: hash checked, folder copied, `tenerod rewind --to 19` as a dry run and then with `--yes`, restart on the fixed build | 3 min 53 s |
| result | both nodes at height 28 with the SAME tip; the fixed build logged no invalid block; the chain went on from 19 | 4 min 14 s |
| a node that did NOT update | a node still on the broken build, with blocks 20 to 30, connected to a fixed node: the fixed node banned it on block 20 and stayed on the good chain | |

**It is not a real timing.** Nothing waited for a person: no reading, no deciding, no typing, no asleep owner, no tester who is away. A real emergency is hours; this
says only that the commands themselves are quick and that the order in sections 3 to 6 works.

**What the drill found, and the fix (all in the commit that adds this section):**
1. **Section 3 said to record the build with `--version`, and `tenerod` had no `--version`.** Now `tenerod --version` prints the version and the source commit
   (`-dirty` if the tree had changes, `unknown` for a build from an archive unless `TENERO_COMMIT` is set), and the node's log starts with the same line.
2. **The log said only "sent an invalid block", with no height, no id and no reason, which is the first thing section 4 needs.** The line that bans a peer now reads
   `sent an invalid block (height 20, id 000238ae...): CoinbaseAmount { expected: 2000000000, got: 2000000001 }`; a test (`an_invalid_block_bans_the_peer_at_once...`) holds it.
   (The reason is in the line only when the peer is banned or dropped; a penalty that does not drop the peer is still silent.)
3. **`tenerod rewind` said "it would lose N blocks" even after it had done it.** Now "it lost".

**Smaller things noticed, not changed:** the chain kept growing between the discovery and the stop (the miner mines until stopped, so stopping first matters more than
any other step); the dry run is the default, which was right (nobody removed a block by accident).

**What the drill did NOT test:** the other emergencies in section 1; announcing in a real channel; a tester who does not read the message; a network of more than
three nodes; a bug in the rewind command's own code path on a pruned node (`pop_block` works on pruned blocks, which is tested in the store, not in a drill); a bug found
after someone has spent from the bad blocks (the plan says: reset).
