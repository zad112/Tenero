# Security policy

**Tenero is an experiment: unaudited, run by one person, and not for real value.** Do not put anything you cannot afford to lose on it. The network can be reset
at any time (`docs/EMERGENCY_PLAN.md`), and nothing cryptographic here has had an independent review (`docs/THREAT_MODEL.md` says what has and has not been checked).

## Reporting a problem

Please report a security problem **privately**: on this repository's **Security** tab, choose **Report a vulnerability** (GitHub's private vulnerability
reporting). Please do not open a public issue for something that could be used against a running node, miner or wallet before it is fixed.

What is useful: what you did, what happened, which commit or build (`tenerod --version` prints it), and, if you can, a small input that shows it.

**What to expect:** one person maintains this, in spare time. There is no bug bounty and no promised response time; reports are read and answered as soon as
that is possible. A fix for a rule of the chain (consensus) may mean a network reset, which is announced as `docs/EMERGENCY_PLAN.md` describes.

## What counts

* **In scope:** the Rust code on `main`: block and transaction validation, the peer-to-peer protocol and encrypted channel, storage, the node, miner and wallet
  programs and their local control interface, the wallet file and seed handling, the build and CI files.
* **Known and written down already** (still welcome if you have more detail): the items in `docs/KNOWN_ISSUES.md` and `docs/THREAT_MODEL.md`.
* **Out of scope:** the frozen Python in `reference/` and on the `legacy-python` branch (a test reference and an old prototype, with known flaws kept on
  purpose); the interim wallet scheme's stated limits (`crates/tenero-wallet/src/interim.rs`); anything that needs access to a machine you already control.

## Supported versions

Only the latest commit on `main`. There are no releases yet.
