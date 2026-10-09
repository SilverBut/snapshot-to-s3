# Engineering practices

Lessons from the code- and test-quality passes on this repository. The aim is safe change: a refactor
must not quietly alter behavior, and a test must not quietly lose its power. Commands are in
[CONTRIBUTING.md](../CONTRIBUTING.md#build-and-check); the test layout is in [tests/README.md](../tests/README.md).

## Change one side at a time

If tests and the code they test change together, both can break in a way that still agrees. A weakened
test lets a regression through, or a test gets "fixed" to match a behavior change nobody intended.
Keep one side frozen as the reference:

* Classify every file: **P** production (`src/**` outside tests), **T** tests (`tests/**`,
  `src/testing.rs`, `#[cfg(test)] mod tests`), **S** scripts and tooling (`scripts/**`, `.github/**`,
  `.devcontainer/**`), **D** docs.
* A commit changes one class. A path or import change forced on other classes goes in its own commit
  marked `[mechanical]` in the subject.
* Move files first, with no edits (`git mv` plus path updates), and prove it with
  `git diff -M100% --stat`. Edit them in later commits.
* When you refactor production code, keep inline test modules word for word (only `use` paths may
  change). When you refactor tests, leave production code alone.
* Push each stage and wait for `CI Gate` before starting the next one, so a regression points to one stage.

## Keep tests honest

* Never delete a test, loosen an assertion (exact value to `is_ok()` or `contains`) or change an expected
  value to make a change pass. If behavior changes on purpose, change the expectation in a P-side commit
  and say why.
* Before reorganizing tests, save `cargo test -- --list` (and `--ignored`). Afterwards, map every old test
  to its new name. The ignored set must stay the same.
* Merging tests into a table is fine only if the diff leaves every expected literal untouched.
* For refactors meant to change no behavior, snapshot what users can see before and after:
  every `--help`, error text and exit code, and the object keys, metadata and events that `MemoryStore`
  records.
* For E2E restructuring, compare the sequence of `snapshot-to-s3`/`zfs` commands (`E2E_TRACE=1`) of the
  old and new scripts. Only temporary paths may differ.

## Mutation testing

Line coverage shows that code ran; [cargo-mutants](https://mutants.rs) shows whether a test notices
when it is wrong. Use it in two ways:

* **Guard a test refactor.** Run it before and after. Every mutant caught before must still be caught.
  Compare by file and mutation, not line number, because lines move.
* **Find weak tests.** Each *missed* mutant is a behavior no test pins down. Typical fixes are an exact
  boundary on both sides (`limit` accepted, `limit + 1` rejected), the exact error message and not just
  `is_err()`, the full retry sequence or call count, and the exact bytes or events produced.

A full run over `s3/`, `store/`, `zfs/`, `crypto/stream.rs`, `backup/` and `restore/` takes 1–1.5 hours
on 8 parallel shards, which is too much for a development machine. Run it in CI from a temporary
workflow in a commit whose subject starts with `TEMP:`. Each shard runs
`cargo mutants --no-shuffle -j2 --timeout-multiplier 3 --shard N/8 -f …` and uploads `mutants.out/*.txt`.
Fetch the results with `gh run download`, then delete the workflow in another `TEMP:` commit before merging.

Some mutants cannot be caught, and that is acceptable. Record them in the PR with the reason. Examples:

* mutants of a constant expression (`MIB`, pipe sizes);
* `<` versus `<=` where both branches produce the same result;
* paths that need a stream of about 100 MiB or a real cloud metadata service.

Do not add a production hook just to catch one mutant. If code really needs an injection point (such as
a configurable IMDS base URL), add it in its own P commit before the tests.

## Writing tests

* **Fakes.** `src/testing.rs` provides `MemoryStore` and `FakeZfs` with fault-injection fields (for
  example `discard_parts`, `retryable_failures`, `send_failure`, `dirty`). Integration tests get them
  through the `test-support` feature; release builds never compile them. Add a narrowly named flag
  instead of a one-off fake. Struct fields can be private, so set flags after `default()`/`new()`.
* **Environment variables.** Wrap them in `testing::ScopedEnv`, which serializes on a process-wide lock
  and restores values on drop. Use at most one per test, or the test deadlocks. Integration tests use
  `tests/http_store/support/env.rs`.
* **HTTP.** Use the in-process servers and the SigV4 verifier in `tests/http_store/support/` rather than
  writing a new mock server. To test against a raw local socket, give the client `no_proxy` so a
  proxy set in the environment cannot intercept it.
* **ZFS.** `tests/zfs_backend` installs the fake `zfs`/`zpool` scripts from `tests/fixtures` through
  `SNAPSHOT_TO_S3_ZFS_BIN`/`SNAPSHOT_TO_S3_ZPOOL_BIN`. Extend those scripts; do not embed shell in Rust strings.
* **Data.** Generate streams in the test, with a fixed bound on their size. Commit a static fixture only
  if it cannot be generated, and record where it came from in `tests/fixtures/README.md`.
* **Writers.** Use `Vec<u8>` (tokio implements `AsyncWrite` for it), not a custom writer type.
* **Secret-bearing types** such as `Credentials` deliberately have no `Debug`. Use `.err().unwrap()`
  instead of `unwrap_err()`; do not add `Debug` to make the test compile.
* **Async.** A future that borrows temporaries and is held across `tokio::time::timeout` needs those
  temporaries bound to locals first.
* **Names** state the behavior: `retryable_part_failures_get_three_attempts_and_others_one`, not `test_retry2`.

## Scripts and tooling

* `scripts/` holds release and CI tools. `tests/` holds only tests, their harnesses and environment
  setup. Never keep generated files (`__pycache__`) in the repository.
* Python modules are packages run from the repository root (`python3 -m scripts.release.prepare`). They have
  type annotations and pass ruff. Validate inputs with explicit errors, never `assert`. The unit tests patch module-level
  names (`api`, `run`, …), so keep the code calling them through those module globals.
* Shell scripts use `set -euo pipefail` and pass `shellcheck -x`. Shared logic goes in a sourced `lib/`
  (`tests/provision/lib/seaweedfs.sh`, `tests/e2e/lib/*.sh`), not into copies, and parsing goes in one
  tested Python helper (`tests/e2e/lib/zfs_json.py`), not in repeated inline snippets. Pool-safety code
  in `tests/provision/lib/hosted.sh` is reviewed line by line; leave it unchanged unless that safety logic is the task.
* An E2E scenario is one file in `tests/e2e/cases/NN_name.sh` with a `# requires:` line. Keep each one
  runnable with `run.sh --case` instead of growing a single script.

## Production refactors

* Do not change observable behavior: CLI text, error text, exit codes, object layout, metadata or
  encryption format. A change that improves error context is a behavior change and needs its own PR.
* Name magic numbers without changing their values, such as `PART_RETRY_BACKOFF` and `READ_BUFFER_SIZE`.
* Split a module only when its parts change for different reasons. Size alone is not a reason, and a
  split that only moves code makes history harder to follow.
* Tighten `pub` to `pub(crate)` where the compiler allows it, without changing signatures.
* Check documented numbers before you "correct" them: 64 MiB × 10,000 parts is 625 GiB.

## Validation and resources

* Locally, run the smallest command that covers the change (`cargo test --lib MODULE`,
  `--test NAME`). Keep tests under 10 GiB of memory.
* Push and let CI run the heavy work: E2E on a fresh VM, the official Tink runtime, real S3,
  and mutation testing. Use `gh pr checks`, `gh run view RUN_ID --log-failed` and `gh run download`.
  A green local run does not replace `CI Gate`.
* After each stage, have someone (or a review agent) who did not write it review the diff.
  Ask them to check for weakened tests, changed expectations, behavior changes and pool-safety changes.

## Documentation

* Each fact has one home. Other documents link to it instead of copying it.
  [AGENTS.md](../AGENTS.md) is only an index pointing to the relevant documents.
* Update the document that describes a behavior or interface in the same PR that changes it. Code and
  test documentation are D-side commits.
