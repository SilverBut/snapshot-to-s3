# Contributing to snapshot-to-s3

Thanks for contributing.

This repository is in a from-scratch rebuild phase. Documentation in `README.md`, `docs/design.md`, `docs/storage.md`,
and `docs/workflow.md` defines requirements; it is not a blanket claim that every requirement is already implemented.

## Development setup

### Prerequisites

- Rust current stable toolchain
- Cargo (bundled with Rust)
- Linux with ZFS userspace tools available for ZFS-backed tests

> CI runs credential-free checks only; tests that require real ZFS pools or S3 endpoints are opt-in.

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

The release workflow is tag-driven. Pushing a matching tag triggers:

1. Static Linux x86_64 (musl) release build
2. Artifact packaging (`snapshot-to-s3-linux-x86_64.tar.gz`)
3. `SHA256SUMS` generation
4. GitHub Release publication (stable or pre-release by tag pattern)

### Verifying downloads

```bash
sha256sum -c SHA256SUMS
```

## Need help?

Open a GitHub issue or discussion with reproduction details and expected behavior.
