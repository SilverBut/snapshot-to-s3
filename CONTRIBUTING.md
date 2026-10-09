# Contributing to snapshot-to-s3

Thanks for contributing.

This repository is in a from-scratch rebuild phase. Documentation in `README.md`, `docs/design.md`, `docs/storage.md`,
and `docs/workflow.md` defines requirements; it is not a blanket claim that every requirement is already implemented.

## Development setup

### Prerequisites

- Rust current stable toolchain
- Cargo (bundled with Rust)
- Linux with ZFS userspace tools available for ZFS-backed tests

> Hosted CI provisions disposable ZFS/S3 infrastructure and explicitly runs opt-in tests. Local infrastructure
> tests still require an existing labeled development pool. See [DEVELOPMENT.md](DEVELOPMENT.md).

### Build and run

```bash
git clone https://github.com/SilverBut/snapshot-to-s3.git
cd snapshot-to-s3

cargo build --locked
cargo run -- --help
```

## Code quality gates

Before opening a pull request, run:

```bash
cargo fmt --all -- --check
cargo test --locked
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release --locked
```

If your change touches optional infrastructure tests, include clear notes about environment and execution results.

## Testing notes

- Keep unit tests close to implementation when practical.
- Add integration tests under `tests/` for cross-module behavior.
- Avoid adding tests that require production credentials.
- For local S3 capability checks, see `tests/support/local_s3.sh` and `tests/support/s3_probe.py`.
- `tests/support/zfs_s3_e2e.sh` validates same-namespace safety, public-only GPG-home usage, and native encrypted ZFS raw recovery.

After local S3 startup and env-file load, run integration checks with the current script/test interfaces:

```bash
source "$RUNTIME_DIR/local_s3.env"
export TEST_S3_ENDPOINT="$LOCAL_S3_ENDPOINT" TEST_S3_BUCKET="$LOCAL_S3_BUCKET"
tests/support/zfs_s3_e2e.sh
cargo test --test live_http -- --ignored
```

## Submitting changes

1. Branch from `main`.
2. Make focused commits with clear messages.
3. Update directly related docs when behavior or interfaces change.
4. Open a PR with:
   - Problem statement
   - Design/behavior summary
   - Validation evidence (commands + outcomes)
   - Any unresolved risks or environment-dependent gaps

## Versioning and releases

We use SemVer tags:

- Stable: `vMAJOR.MINOR.PATCH`
- Pre-release: `vMAJOR.MINOR.PATCH-alpha.N`, `-beta.N`, `-rc.N`, or `-pre.N`

Releases use an explicit UI decision, not Conventional Commits or every change to `Cargo.toml`.

1. Open **Actions -> Prepare Release -> Run workflow** on `main`.
2. Choose `initial`, `patch`, `minor` or `major`, and a channel (`stable`, `alpha`, `beta`, `pre`, `rc`).
   `initial` uses the declared package version before the first stable release. Normal bumps use the latest
   published stable version; prerelease runs increment their channel number, and stable promotes that core version.
3. The workflow creates a short-lived draft PR under `automation/release-vVERSION`. It updates the local package
   version in both Cargo files, adds `.github/release-plan.json`, and prepares a `CHANGELOG.md` version section.
4. Edit that section using GitHub's file editor **on the release PR branch**, write actual release notes, and
   remove `<!-- RELEASE_NOTES_NEED_REVIEW -->`. Commit messages need no special convention. Candidate commits
   in the PR body are reference material, not approved release notes.
5. Complete CI validates notes, version consistency, and all real infrastructure tests. The Release controller
   marks a successful draft ready; merge it using a **merge commit**, not squash or rebase.
6. After the exact main merge commit's CI succeeds, the controller builds that frozen SHA as a static Linux
   x86_64 musl binary. It verifies the CLI version/static linkage, packages the archive and `SHA256SUMS`,
   checks the tag, uploads assets to a draft Release, validates uploaded sizes/digests, then publishes.

Source, version, notes and artifacts belong to one frozen commit; advancing `main` cannot change that release.
No crates.io publication is performed. Ordinary CI keeps read-only permissions; only trusted release jobs can
create a PR, mark it ready or publish. The repository must allow GitHub Actions to create PRs. Preparation
explicitly dispatches CI, and publication consumes CI completion rather than relying on bot-generated tag pushes.
GitHub may still require approval of bot-created PR runs; the explicit dispatch and subsequent maintainer notes
commit provide actual checks, not a skipped green gate.

Repeated preparation of the same version reuses the managed branch/PR and preserves handwritten notes. A different
open release proposal blocks another preparation. Existing legacy `release/v...` branches are not force-overwritten.
When a release has been accepted but not published, do not bump again: use **Actions -> Release -> retry** on `main`.
The retry finds the accepted PR's merge SHA and requires that exact commit's CI Gate. A cancelled/failed CI must
first be rerun for that commit. Existing tags pointing elsewhere are errors; published assets are never overwritten.

**Release -> build-only** is the safe verification mode: build the selected ref and upload workflow artifacts,
without creating tags, drafts or official Releases. This is also how release automation changes are tested before
authorizing a real version.

### Verifying downloads

```bash
sha256sum -c SHA256SUMS
```

## Need help?

Open a GitHub issue or discussion with reproduction details and expected behavior.
