# Emergency plan: what happens when a rule is wrong (a DRAFT, E8 of `THREAT_MODEL.md`)

**Status: a draft written by the author (2026-10-03), with the decisions that are the owner's marked DECIDE. Nothing here has been rehearsed. Finishing it (the decisions, the channel, the two code gaps decided, and one drill) is M11.0 in `M10_M11_PLAN.md`.**
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

## 2. Who decides (DECIDE)

Today the owner is the only maintainer, so the owner decides everything below. **DECIDE:** whether anyone else may (a named second person who can
say "stop mining" without the owner), and how that person is reached. A plan with one person who may be asleep, on holiday or unreachable is
the plan's weakest point.

## 3. The first hour: stop the damage

In this order, the first two before anything is understood:

1. **Stop your own node and miner** (`tenerod stop`; for a miner process, its own stop). A node that keeps running keeps spreading a bad block.
2. **Say so, in the channel testers watch** (DECIDE the channel: a pinned issue on the repository, a mailing list, a chat room; none exists
   today), in plain words: what is known, what is not, and "stop mining and stop your nodes until told". No theories, no blame.
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
| the bad block is recent and nobody has spent from it | a **fix and a coordinated rewind**: a new build that refuses the bad block; testers rewind to the last good block (a `rewind` command is NOT built: **a gap**) |
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

1. **No second person** (section 2) and **no channel** (section 3). Both are the owner's to set up.
2. **No rewind tool**: a coordinated rewind is described but there is no command that does it; a reset is the only thing that works today.
3. **No upgrade mechanism**: a rule change reaches nodes by everyone installing a new build; there is no activation height or version signalling
   (`CONSENSUS_V2.md` section 10 has a rules version, not an activation schedule). A planned rule change (such as the timestamp rule of E3) needs
   one, or a reset.
4. **Nothing has been rehearsed.** The first time any of this is done should not be a real emergency: a drill (a deliberately broken build on a
   scratch network, run through sections 3 to 6) is the cheapest test of the plan.
