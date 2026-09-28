# How to make a release

This page gives every step of a release: the preparation, the test of the
pipeline, the tag, the publish to crates.io, and the GitHub release. It also
gives the recovery for each failure that can occur.

`docs/versioning.md` gives which number to choose. `docs/repository.md` gives
the layout of the repository.

## What a release is

A release lives in three places. Each place holds a different thing:

| Place | What it holds | Who reads it | Can you undo it |
|---|---|---|---|
| A git tag `vX.Y.Z` | the exact commit of the release | a person who reads the source | yes. A person who fetched it keeps the old one |
| crates.io | the source of six crates at version `X.Y.Z` | a Rust program, through `cargo add` | **no.** A yank hides a version and deletes nothing |
| A GitHub release | the notes, and the C library for three platforms | a C, C++, or Python program | yes. Edit or delete it at any time |

The six crates on crates.io are `labstream`, `labstream-core`,
`labstream-wire`, `labstream-proto`, `labstream-time`, and `labstream-net`.
Every one of them gets the same version in each release.

`labstream-capi` does not go to crates.io. The GitHub release carries it as
three built libraries. The section
[Why the C library is not on crates.io](#why-the-c-library-is-not-on-cratesio)
gives the reason.

### One tag does all three

`.github/workflows/release.yml` makes the release. The push of a tag that starts
with `v` starts it. You prepare one commit and push one tag. The workflow then
does the rest, in this order:

```
push of tag vX.Y.Z
      │
      ▼
   verify ──────────► tag = manifest?  changelog has X.Y.Z?  fmt, clippy, tests on Linux
      │
      ├──────────────► publish ───► crates.io: each crate that crates.io does not hold
      │                   │
      └──────────────► binaries ──► liblsl for Linux, macOS, Windows
                          │
                          ▼
                       release ───► GitHub release: notes + three libraries
```

`publish` and `binaries` run at the same time. `release` waits for both. If
`verify` fails, nothing else runs, and nothing reaches crates.io.

## Setup that you do once

These settings exist already for this repository. Read this section when a
setting is lost, when a crate is new, or when the repository moves.

### Trusted publishers on crates.io

The workflow publishes with no stored token. crates.io gives the workflow a
token that lasts 30 minutes, and only if the crate names this repository as a
trusted publisher.

For each of the six crates, open the crate on crates.io. Then open Settings,
then Trusted Publishing, and make sure that this entry exists:

| Field | Value |
|---|---|
| Repository owner | `rednayan` |
| Repository name | `labstream` |
| Workflow filename | `release.yml` |
| Environment | empty, unless you use the approval below |

If one crate has no entry, the publish stops at that crate. v0.1.2 stopped that
way. The section [When something fails](#when-something-fails) gives the
recovery.

crates.io accepts a trusted publisher only for a crate that exists. A new
crate therefore goes to crates.io by hand the first time. The section
[Publish a new crate](#publish-a-new-crate) gives the steps.

### The workflow must be known to GitHub

GitHub runs a workflow only after it registers the file. It registers a file
when a push to `main` changes that file. To make sure that GitHub knows the
release workflow, run:

```sh
gh workflow list -R rednayan/labstream
```

The list must show `Release`. If it does not, push a commit that changes
`.github/workflows/release.yml`. The first push of this repository did not
register the file, and a comment change in the file corrected it.

### Approval before each publish (optional)

The `publish` job can wait until a person approves it:

1. In the repository settings on GitHub, open Environments.
2. Make an environment named `crates-io`, and add yourself as a reviewer.
3. Add `environment: crates-io` to the `publish` job in `release.yml`.
4. On crates.io, write `crates-io` in the Environment field of each trusted
   publisher.

A release then stops before the publish until you approve it in the Actions
tab.

## Before you release

1. **Choose the number.** `docs/versioning.md` gives the rule. Before 1.0, a
   breaking change raises the minor number, and every other change raises the
   patch number.
2. **Make sure that `main` is green.** A release starts from the head of
   `main`, and CI must pass on that commit.
3. **Run the workbench if a protocol rule changed.** A change to `-wire`,
   `-proto`, `-time`, or the protocol parts of `-net` needs a conformance run.
   Record the result in `docs/conformance.md`. `CONTRIBUTING.md` gives the rule.
4. **Test across two machines if the socket layer changed.** A change to
   `socket2`, `libc`, or the discovery code can hide one stream from another
   machine. CI uses loopback alone and cannot see that. Run
   `conformance/oracle/labrecorder.sh` on one machine and a recorder on a
   second machine. `conformance/README.md` gives the commands.

## Make the release, step by step

In the commands below, `X.Y.Z` is the new version, for example `0.1.3`.

### Step 1: prepare the release commit

1. Pull `main`:

   ```sh
   git switch main
   git pull
   ```

2. In `CHANGELOG.md`, add a heading below `## [Unreleased]`:

   ```markdown
   ## [Unreleased]

   ## [X.Y.Z] - YYYY-MM-DD
   ```

   The entries that were below `[Unreleased]` are now below the new heading.
   Write the date of the release. Add one or two sentences below the heading
   that tell a reader what the release is. The GitHub release shows them first.

3. At the end of `CHANGELOG.md`, change the link of `[Unreleased]`, and add a
   link for the new version:

   ```markdown
   [Unreleased]: https://github.com/rednayan/labstream/compare/vX.Y.Z...HEAD
   [X.Y.Z]: https://github.com/rednayan/labstream/releases/tag/vX.Y.Z
   ```

4. In the root `Cargo.toml`, set the new version in five places: `version` in
   `[workspace.package]`, and `version` in each of the four entries of
   `[workspace.dependencies]`.

   ```toml
   [workspace.package]
   version = "X.Y.Z"

   [workspace.dependencies]
   labstream-wire = { path = "crates/labstream-wire", version = "X.Y.Z" }
   labstream-proto = { path = "crates/labstream-proto", version = "X.Y.Z" }
   labstream-time = { path = "crates/labstream-time", version = "X.Y.Z" }
   labstream-net = { path = "crates/labstream-net", version = "X.Y.Z" }
   ```

   If an entry keeps the old number, the published crate asks crates.io for the
   old release of that dependency.

5. In `README.md`, change the version in the first sentence of the Status
   section.

6. Update `Cargo.lock`:

   ```sh
   cargo build --workspace
   ```

7. Make sure that no old number is left. Each line must show the new number:

   ```sh
   grep -nE '(^|, )version = "' Cargo.toml
   grep -n "This is version" README.md
   ```

### Step 2: run the checks of the workflow on your machine

The `verify` job runs these checks. Run them first, because a failure costs
less here.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps
cargo publish --workspace --dry-run --allow-dirty
```

The last command packages each crate and builds it from the package, as
crates.io will. It uploads nothing. Its output must name the six crates, and
`labstream-capi` and `lsl-peer` must not be in it.

### Step 3: commit and push

```sh
git add -A
git commit -m "Release vX.Y.Z"
git push origin main
```

Wait until CI passes on this commit:

```sh
gh run list -R rednayan/labstream --branch main --limit 3
```

### Step 4: test the pipeline with no publish

This run builds the three libraries and stops. It publishes nothing and writes
no GitHub release.

```sh
gh workflow run release.yml -R rednayan/labstream --ref main
```

Then watch it:

```sh
gh run watch -R rednayan/labstream $(gh run list -R rednayan/labstream --workflow release.yml --limit 1 --json databaseId -q '.[0].databaseId')
```

`Verify the tag` and the three `Build liblsl` jobs must pass. `Publish to
crates.io` and `Write the GitHub release` show as skipped. That is correct for
this run.

If this run fails, correct the cause and push again. No tag exists yet, so
nothing needs to be undone.

### Step 5: tag and push the tag

> **CAUTION:** This step publishes to crates.io. A published version cannot be
> deleted. Read the version number twice.

Copy this command, and replace only `X.Y.Z`:

```sh
git tag -a vX.Y.Z -m "vX.Y.Z" && git push origin vX.Y.Z
```

The tag must be `v`, then the number, with no other character. `v.0.1.2`,
`0.1.2`, and `V0.1.2` are wrong. A wrong tag fails in `verify`, and nothing is
published, but the wrong tag stays on GitHub until you delete it.

`git tag -a` needs `-m`. Without it, git opens an editor for the message, and
an empty message stops the command with `fatal: no tag message?`.

Tag the commit that passed step 4. After `git pull`, that commit is the head of
`main`.

### Step 6: watch the release

```sh
gh run list -R rednayan/labstream --workflow release.yml --limit 1
gh run watch -R rednayan/labstream <run-id>
```

All six jobs must pass.

### Step 7: make sure that the release is complete

1. **crates.io.** Each crate must show the new version:

   ```sh
   for c in labstream labstream-core labstream-wire labstream-proto labstream-time labstream-net; do
     printf "%-16s " $c
     curl -s -A "labstream-release-check" https://crates.io/api/v1/crates/$c/X.Y.Z -o /dev/null -w "%{http_code}\n"
   done
   ```

   Each line must end in `200`. A `404` means that crates.io does not hold that
   crate at that version.

2. **The GitHub release.** It must show the notes and three files:

   ```sh
   gh release view vX.Y.Z -R rednayan/labstream
   ```

   The files are `liblsl-x86_64-linux.so`, `liblsl-aarch64-macos.dylib`, and
   `lsl-x86_64-windows.dll`.

3. **docs.rs.** docs.rs builds the documentation of each crate some minutes
   after the publish. Open `https://docs.rs/labstream/X.Y.Z` to see it.

## What the workflow does, in detail

### `verify`

It runs on Linux. On the push of a tag it makes sure of two things:

- The tag without its `v` equals `version` in `[workspace.package]`. If the tag
  does not match, the release publishes one version under the name of another.
- `CHANGELOG.md` holds a heading that starts with `## [X.Y.Z]`.

Then it runs `cargo fmt --check`, `cargo clippy` with warnings as errors, and
`cargo test --workspace`.

A run from the Actions tab has no tag, so it skips the two tag checks. It runs
the rest.

### `publish`

It runs only on the push of a tag. It does these steps:

1. It gets a token from crates.io through `rust-lang/crates-io-auth-action`.
   crates.io gives it only to a workflow that a trusted publisher names.
2. It asks crates.io, for each of the six crates, whether crates.io holds the
   crate at the version.
3. If crates.io holds all six, it publishes nothing and passes. This lets a
   second run of one release finish.
4. Otherwise it runs `cargo publish --workspace` and gives `--exclude` for each
   crate that crates.io holds. Cargo finds the order itself. A published crate
   that needs an excluded crate takes it from crates.io.

The list of six crates is in the step. A new crate that goes to crates.io must
go in that list.

### `binaries`

It builds `labstream-capi` in release mode on `ubuntu-latest`,
`macos-latest`, and `windows-latest`. It reads the file name from the report
of Cargo, and it gives each file a name that states the platform:

| Platform | File |
|---|---|
| Linux, x86-64 | `liblsl-x86_64-linux.so` |
| macOS, Apple silicon | `liblsl-aarch64-macos.dylib` |
| Windows, x86-64 | `lsl-x86_64-windows.dll` |

It stores each file as an artifact of the run. It runs on the push of a tag and
on a run from the Actions tab.

### `release`

It runs only on the push of a tag, after `publish` and `binaries` pass. It
takes the section of `CHANGELOG.md` for the version, and it adds install
instructions. Then it runs `gh release create` with those notes and the three
files. `--verify-tag` makes sure that the tag exists on GitHub.

## When something fails

Find the job that failed in the run. Then read its row.

| What failed | What reached crates.io | What to do |
|---|---|---|
| `verify`: the tag does not match the manifest | nothing | Delete the tag. Correct the tag or the manifest. Tag again |
| `verify`: no changelog section | nothing | Delete the tag. Add the section, commit, push. Tag again |
| `verify`: fmt, clippy, or a test | nothing | Delete the tag. Correct the code, commit, push. Tag again |
| `publish`: `403 Forbidden`, "token is not valid for crate" | the crates before that crate | Add the trusted publisher of that crate. Rerun the failed jobs |
| `publish`: any other failure partway | the crates before the failure | Correct the cause. Rerun the failed jobs |
| `binaries`: one platform | everything, if `publish` passed | Rerun the failed jobs. If the code is at fault, read [A release with no libraries](#a-release-with-no-libraries) |
| `release` | everything | Rerun the failed jobs, or write the release by hand |
| `gh workflow run` answers `404` | nothing | GitHub does not know the workflow. Read [The workflow must be known to GitHub](#the-workflow-must-be-known-to-github) |

### Delete a tag

Delete a tag only when crates.io holds nothing of the version. If crates.io
holds any crate at that version, keep the tag, because the tag and crates.io
must name the same code.

```sh
git push origin --delete vX.Y.Z
git tag -d vX.Y.Z
```

Then correct the cause, commit, push, and tag again.

### Rerun the failed jobs

In the Actions tab, open the run, and select "Re-run failed jobs". Or run:

```sh
gh run rerun <run-id> -R rednayan/labstream --failed
```

The rerun uses the workflow file of the tagged commit, and not the file on
`main`. The jobs that passed keep their results and their artifacts.

If `gh` answers "This workflow run cannot be retried", look at the run first.
Someone can have run it again already:

```sh
gh api repos/rednayan/labstream/actions/runs/<run-id> -q '"attempt=\(.run_attempt) \(.conclusion)"'
```

### A publish that stopped partway

A version on crates.io is permanent. A stopped publish therefore cannot go
back, and it must go forward. `publish` excludes each crate that crates.io
holds, so a rerun of the failed jobs publishes the rest.

The tags before v0.1.3 hold an older workflow. That workflow stops when
crates.io holds some of the crates and not all of them. For such a tag, publish
the rest by hand, as the next section gives. Then rerun the failed jobs, so
that `release` writes the GitHub release.

### A release with no libraries

If a build of `binaries` fails because of the code, the crates are on
crates.io, and no GitHub release exists. crates.io holds the version, so do not
delete the tag. Do one of these:

- Correct the code, and make the next patch release.
- Build the libraries on each platform by hand, and write the GitHub release by
  hand. The section [Write the GitHub release by hand](#write-the-github-release-by-hand)
  gives the commands.

## Publish to crates.io by hand

The workflow publishes. Use these steps when it cannot: for a new crate, or
for a partial publish from an older tag.

A published version is permanent. Read the version number twice.

1. Make a token on crates.io: Account Settings, then API Tokens. Give it the
   scope `publish-update`, or `publish-new` for a new crate.
2. Give the token to Cargo:

   ```sh
   cargo login
   ```

3. Switch to the exact code of the tag:

   ```sh
   git switch --detach vX.Y.Z
   ```

4. Publish. If crates.io holds some crates at the version already, give one
   `--exclude` for each:

   ```sh
   cargo publish --workspace
   cargo publish --workspace --exclude labstream-time
   ```

5. Go back to `main`:

   ```sh
   git switch main
   ```

`labstream-capi` and `lsl-peer` set `publish = false`, so the command leaves
them out.

If one crate must go alone, use this order. Each crate needs the crates above
it:

1. `cargo publish -p labstream-wire`
2. `cargo publish -p labstream-time`
3. `cargo publish -p labstream-proto`
4. `cargo publish -p labstream-net`
5. `cargo publish -p labstream-core`
6. `cargo publish -p labstream`

crates.io needs some seconds to add a new version to its index. If a step
reports that it cannot find the crate before it, wait and run it again.

Remove the token after the publish, on the same page of crates.io. The workflow
does not need it.

### Publish a new crate

crates.io accepts a trusted publisher only for a crate that exists. For a new
crate:

1. Add the crate to the list in the publish step of `release.yml`.
2. Make the release as usual. The `publish` job stops at the new crate, with
   `403 Forbidden`.
3. Publish the new crate by hand, with a token that has the scope
   `publish-new`, as the section above gives.
4. On crates.io, add the trusted publisher for the new crate.
5. Rerun the failed jobs. `publish` publishes the rest, and `release` writes
   the GitHub release.

## Write the GitHub release by hand

Use these steps when the `release` job cannot run.

1. Get the three libraries from the run, or build each one on its platform:

   ```sh
   gh run download <run-id> -R rednayan/labstream -D assets
   ```

   To build one by hand:

   ```sh
   cargo build -p labstream-capi --release
   ```

2. Write the notes. Copy the section of `CHANGELOG.md` for the version into
   `notes.md`.
3. Make the release:

   ```sh
   gh release create vX.Y.Z -R rednayan/labstream --title vX.Y.Z --notes-file notes.md --verify-tag assets/*/*
   ```

To add one file to a release that exists:

```sh
gh release upload vX.Y.Z -R rednayan/labstream liblsl-x86_64-linux.so
```

A GitHub release can be edited or deleted at any time, in the Releases page or
with `gh release edit` and `gh release delete`. A deletion keeps the tag.

## Take back a version from crates.io

crates.io never deletes a version. It can yank one. A yanked version stays
available to a project that has it in its `Cargo.lock`. A new project does not
get it.

Yank a version only when it does harm, for example when it writes wrong data.
For a defect that does no harm, make the next patch release.

```sh
cargo yank --version X.Y.Z labstream-net
```

Yank the same version of each of the six crates, because they move together.
`--undo` reverses a yank:

```sh
cargo yank --version X.Y.Z --undo labstream-net
```

Write the reason in `CHANGELOG.md`, below the version.

## Why the C library is not on crates.io

`labstream-capi` builds one shared library, and its only target is a `cdylib`.
A `cdylib` holds no `rlib`, so a Rust program cannot link it. Cargo takes the
dependency and builds the crate, and then the first use of it stops with
"unresolved module or unlinked crate".

crates.io carries source and not a binary. A C program needs a built
`liblsl.so` or `lsl.dll`, and crates.io gives neither. A publication of this
crate therefore reaches no reader who can use it. It can also send a Rust
reader to a crate that does not link.

`labstream-capi` sets `publish = false`, so no command can publish it by
mistake. The GitHub release carries the built library for each platform, and a
C program that takes one needs no toolchain.

## Checklist

Copy this list into the pull request or the issue of the release.

```
- [ ] Number chosen from docs/versioning.md: X.Y.Z
- [ ] Conformance run, if a protocol rule changed
- [ ] Test across two machines, if the socket layer changed
- [ ] CHANGELOG.md: [X.Y.Z] - YYYY-MM-DD, with a summary, and both links
- [ ] Cargo.toml: version in five places
- [ ] README.md: version in the Status section
- [ ] cargo build --workspace, to update Cargo.lock
- [ ] fmt, clippy, test, doc, publish --dry-run: all pass
- [ ] Commit "Release vX.Y.Z", push, CI passes
- [ ] gh workflow run release.yml: verify and binaries pass
- [ ] git tag -a vX.Y.Z -m "vX.Y.Z" && git push origin vX.Y.Z
- [ ] Release run: six jobs pass
- [ ] crates.io: six crates at X.Y.Z
- [ ] GitHub release: notes and three files
```

## The releases so far

| Version | Date | How it went |
|---|---|---|
| 0.1.0 | 2026-08-03 | By hand, from `labstream-core` |
| 0.1.1 | 2026-08-04 | By hand, from `labstream-core` |
| `labstream` 0.1.0 | 2026-08-04 | From `labstream-rs`. Its tag here is `labstream-v0.1.0` |
| 0.1.2 | 2026-09-29 | The first from this repository. `labstream-wire` had no trusted publisher, so the workflow published `labstream-time` and stopped. The other five went by hand, and a rerun wrote the GitHub release |
