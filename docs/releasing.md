# Releasing

A release is three installers for the Mira desktop app, attached to a GitHub release with
their checksums:

| Platform | File                         | Signature |
| -------- | ---------------------------- | --------- |
| macOS    | `Mira_X.Y.Z_universal.dmg`   | ad-hoc    |
| Windows  | `Mira_X.Y.Z_x64-setup.exe`   | none      |
| Linux    | `Mira_X.Y.Z_x86_64.AppImage` | none      |
| all      | `SHA256SUMS`                 | -         |

The DMG is universal, so one file serves Apple Silicon and Intel Macs. The Windows
installer is NSIS and installs per user, so it needs no administrator rights. `mira-cli`
is not packaged; it is built from source.

Everything is built by `.github/workflows/release.yml` on GitHub's own macOS, Windows and
Linux runners, with [cargo-packager](https://crates.io/crates/cargo-packager), so a release
needs no Mac or Windows machine. Its configuration is the `[package.metadata.packager]`
table in `mira-app/Cargo.toml`, and the icons are in `mira-app/icons`, whose README says
how to regenerate them.

## Why unsigned, and what it costs

Signing that macOS and Windows trust costs money every year, through an Apple Developer
membership and a Windows signing certificate or service, and buys mainly one thing: no
warning the first time someone opens the app.

What it costs instead:

- **macOS** blocks the app the first time it is opened, until someone clicks _Open
  Anyway_ in System Settings. Every new version is blocked again. The ad-hoc signature is
  free and changes nothing about that warning. What it prevents is worse: a Mac finding an
  unsigned app in a download calls it _damaged_, with no button to open it anyway.
- **Windows** SmartScreen shows _Windows protected your PC_ when the installer is run, and
  someone has to click _More info_, then _Run anyway_. The installed app does not warn
  again.
- **Nobody can tell your file from a tampered one.** A signature would prove a file came
  from you. The checksums are the substitute, and they only work if the list someone
  checks against did not come from the same place as the download. Post them somewhere
  separate as well; see [Sharing a release](#sharing-a-release).

This matters more for Mira than for most apps: its users trust its CA certificate, which
lets whoever holds the key read their HTTPS traffic. A tampered build could ship a CA of
its own.

[installing.md](installing.md) walks users through the warnings and the checksum, and is
the page to send them.

## How a release runs

1. You push a tag, `vX.Y.Z`, matching the version in the root `Cargo.toml`.
2. The workflow checks the tag against that version before building anything.
3. Three jobs build the installers on macOS, Windows and Linux. The macOS job checks that
   its signature covers the whole app and that the app runs on both kinds of Mac.
4. A last job computes `SHA256SUMS` over all three files and creates a **draft** release
   with the installers, the checksum file, and the checksums written into the notes.
5. You read the draft and publish it. Nothing is public until then.

A failed job can be re-run from the Actions tab. If the draft already exists, it is
replaced, so its checksums always match its files. A published release is never
replaced.

## Cutting a release

1. On a branch named `release/vX.Y.Z`, set `version` under `[workspace.package]` in the
   root `Cargo.toml`, and run `cargo update --workspace` so `Cargo.lock` follows.
   cargo-packager reads the app's version from there; there is no second copy to keep in
   step.
2. Commit, push, and open a pull request. CI builds it with `--locked`, which catches a
   `Cargo.lock` left behind. The branch prefix labels it `release`, which keeps it out of
   the notes.
3. Once it is merged, tag `main` and push the tag:

   ```bash
   git switch main
   git pull --ff-only
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

4. When the workflow finishes, open the draft under **Releases**, edit the notes as you
   like, keeping the checksum block, and **Publish**.

The draft lists the pull requests merged since the last tag, sorted by label as
`.github/release.yml` says: `enhancement` under Features, `bug` under Fixes,
`documentation` under Documentation, and the rest under Other changes. A new pull request
is labelled from its branch prefix by `.github/workflows/label.yml`: `feature/` gets
`enhancement`, `bugfix/` and `hotfix/` get `bug`, `docs/` gets `documentation`, and
`release/` gets `release`, which the notes leave out.
Change a label before tagging to move a pull request; relabelling later needs a re-run of
the workflow.

## Sharing a release

A public repository's releases are open to anyone. Also paste the contents of
`SHA256SUMS` wherever you announce the release. A checksum file sitting beside a download
proves only that the download was not damaged. Someone able to swap the installer could
swap the file next to it too, but not your announcement as well.

## Rehearsing without a tag

**Actions → Release → Run workflow** builds exactly as a release does, but keeps the three
installers and `SHA256SUMS` as one artifact of the run, `mira-installers`, rather than
creating a release. Download it from the run's summary page. It is also a way to hand
someone a build of a branch.

## Building locally

With `cargo install cargo-packager --locked` done once, from `mira-app`:

```bash
cargo packager --release
```

This builds the release binary, then the installers for the platform you are on, in
`target/packages/`: on a Mac, the ad-hoc signed `Mira.app` and a DMG for that Mac's
architecture alone; on Linux, a `.deb` and the AppImage, which need the packages the
workflow's **Install GPUI's libraries and the AppImage tooling** step installs.

A universal DMG takes both architectures' binaries glued together first. On an Apple
Silicon Mac, which is what the workflow runs on:

```bash
rustup target add x86_64-apple-darwin
cargo build --release -p mira-app                               # this Mac's own, arm64
cargo build --release -p mira-app --target x86_64-apple-darwin  # Intel
mkdir -p target/universal-apple-darwin/release
lipo -create -output target/universal-apple-darwin/release/mira \
  target/release/mira target/x86_64-apple-darwin/release/mira
cd mira-app && cargo packager --release --target universal-apple-darwin
```

`cargo packager` runs `cargo build --release` before it packages, whatever the target. The
first build above has already done it, so that costs nothing.

## Keeping it working

The workflow pins every third-party action to a commit, with the version beside it in a
comment, so that a moved tag cannot change what builds your installers, and installs
cargo-packager at a fixed version for the same reason. Bump them deliberately. The Linux
job builds on `ubuntu-22.04` on purpose: an AppImage runs only on systems at least as new
as the glibc it was built against, so move it to a newer Ubuntu only when GitHub retires
that runner.
