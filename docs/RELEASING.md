# Making a release (the owner's checklist)

A release is made by the `release` workflow (`.github/workflows/release.yml`) from a tag. The workflow cannot do two things, which are yours:
**the GPU tests** (there is no GPU in CI: CLAUDE.md rule 5) and **pressing "Publish"**.

## 1. Before the tag (on your machine)

1. `main` is green in CI (all of `rust`, `tests`, `supply-chain`).
2. The version in `Cargo.toml` (`[workspace.package]`) is the one you want, and the numbers in `crates/tenero-gui/tenero.rc` follow it (FILEVERSION `0,1,0,1` for
   `0.1.0-alpha.1`). `crates/tenero-app/tests/version.rs` pins the version; change it with the version.
3. The notes exist: `docs/releases/v<version>.md`. Read them: every sentence must still be true of this build.
4. **Run the GPU checks** (CUDA 13.4 `bin\x64` on PATH), from a clean checkout of the commit you will tag, and write down the card and the result:

       cargo test --release -p tenero-gpu -- --ignored --test-threads=1
       cargo test --release -p tenero-miner --test gpu_mining -- --ignored --nocapture --test-threads=1

5. Try the `release` workflow once as a **dry run**: Actions, release, "Run workflow" (no tag). It builds both packages and keeps them for a week as artifacts. Download
   the Windows zip, unzip it somewhere that is not the repository, run `tenerod --version`, start `tenero-wallet-gui.exe`, check the icon and the About tab.

## 2. The tag

    git tag -a v0.1.0-alpha.1 -m "Tenero v0.1.0-alpha.1"
    git push origin v0.1.0-alpha.1

The workflow refuses a tag that is not `v` + the version in `Cargo.toml`, or that has no notes file. It builds, packs, runs the packed programs, and creates a **draft,
pre-release** release.

## 3. The draft

1. Open the draft under Releases. Download the files from the draft itself (not from your build folder) and check them against `SHA256SUMS`.
2. In the notes, replace "GPU test status ... NOT YET DONE" with the card, the driver, the date and the commit you tested; copy the glibc floor from the workflow log
   ("the oldest glibc the Linux programs need") into the Linux paragraph.
3. Press **Publish release** only when you are satisfied. A published release is public and the file names are in people's hands.
4. If something is wrong after publishing: say so in the pinned issue (https://github.com/zad112/Tenero/issues/1), then fix and release the next alpha. Do not replace
   files in a published release under the same name.

## What a release does not promise

It is not code-signed, not reproducible bit for bit, and not audited. The notes say so; do not remove those lines.
