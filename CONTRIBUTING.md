# Contributing

## Build and check

Requires stable Rust. Run these before opening a pull request; CI runs them too:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
python3 -m unittest discover -s tests/tooling -p 'test_*.py'
python3 -m scripts.release.prepare check
shellcheck tests/e2e/*.sh tests/provision/*.sh tests/provision/lib/*.sh
```

Tests that need ZFS, a local S3 service or the official Tink runtime are opt-in. See
[DEVELOPMENT.md](DEVELOPMENT.md); never point them at production credentials or pools.

## Code layout

See the [architecture](docs/design.md#architecture). In short:

* Unit tests sit next to the code. Workflow tests against in-memory fakes are in
  `src/workflow_tests.rs`, and the fakes are in `src/testing.rs`.
* Integration tests are in `tests/`, with static fixtures in `tests/fixtures/` and scripts in
  `tests/e2e/`, `tests/provision/` and `tests/tooling/`; release and CI tools are in `scripts/`.
* Keep `backup`/`restore` independent of HTTP and ZFS details; they use the `ObjectStore` and `Zfs`
  traits.
* Every buffer must have a fixed bound. Never read a stream, or anything whose size the user controls,
  into memory or a temporary file.
* `unsafe` code is forbidden.

## Pull requests

Branch from `main` and keep commits focused. Update the docs that describe changed behavior or
interfaces. In the PR, describe the problem, the change, the validation you ran (commands and results),
and any remaining risks. `main` requires the `CI Gate` check and an up-to-date branch.

## Releases

Versions are SemVer tags: `vX.Y.Z`, or `vX.Y.Z-alpha.N` / `-beta.N` / `-rc.N` / `-pre.N`.

1. Run **Actions → Prepare Release** on `main` with a bump (`initial`, `patch`, `minor`, `major`) and a
   channel (`stable`, `alpha`, `beta`, `pre`, `rc`). It opens a PR on
   `automation/release-vVERSION` that updates the versions, adds `.github/release-plan.json` and starts
   a `CHANGELOG.md` section.
2. On that branch, write the release notes and remove `<!-- RELEASE_NOTES_NEED_REVIEW -->`. CI fails
   while the notes are unreviewed or the versions disagree.
3. When CI passes, merge it with a **merge commit** and wait for CI on that merge commit in `main`.
4. Run **Actions → Release** on `main` with mode **publish**. It builds a static x86_64 musl binary,
   verifies it, and publishes it with `SHA256SUMS` (check with `sha256sum -c SHA256SUMS`).

Both workflows run only when started manually; no push, PR or CI completion starts a release.

Preparing the same version again reuses its PR and keeps the notes. If an accepted release was not
published, run **Actions → Release → publish** again instead of bumping. **Release → build-only** and
**Prepare Release → dry_run** test the automation without creating tags or releases. Nothing is published
to crates.io.
