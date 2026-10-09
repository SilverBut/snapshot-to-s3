# Development environment

Offline checks are listed in [CONTRIBUTING.md](CONTRIBUTING.md#build-and-check). This page covers the
opt-in tests that need ZFS, a local S3 service or the official Tink runtime, and the CI that runs them.

## ZFS development pools

Containers share the host's ZFS kernel module through `/dev/zfs`, so pools are not isolated from the host.
Local tests use only an existing pool provided by an administrator:

* Discover pools; never hard-code names or backing paths:

  ```bash
  zfs version; zpool list; zpool get user:isdev; zpool status -P; sudo -n true
  ```

* Use only an `ONLINE` pool whose **pool** property (not a dataset property) `user:isdev` is exactly `yes`;
  recheck the chosen pool with `zpool get -H -o value user:isdev "$pool"`. If there is none, stop and ask
  for one.
* Never create, import, export, destroy or relabel pools, never change pool properties to make a pool
  eligible, and never prepare backing files or devices.

The label allows development testing, not unrestricted destruction. Within the pool, tests must:

* create a unique child namespace such as `$pool/smoke_<id>`, after checking that it does not exist,
  and change, write, roll back or destroy only inside it;
* mount test filesystems at explicit temporary mountpoints with `canmount=on`, and check
  `findmnt -n -o FSTYPE -T "$mountpoint"` reports `zfs` before writing;
* set `atime=off` on source and received filesystems. Otherwise verification reads change the receive
  target, and the next incremental receive fails with "destination ... has been modified". Never hide
  such changes with `zfs receive -F`;
* use `set -euo pipefail` and an exit trap for cleanup. Report cleanup failures, keep diagnostics, and
  never delete a mountpoint tree while its dataset is mounted.

Keep total test artifacts under 40 GiB and leave at least 20 GiB free (hosted VMs: 4 GiB each, set by
the wrappers). Prefer generated streams to large fixtures.

## Local end-to-end tests

[`tests/e2e/run.sh`](tests/e2e/run.sh) finds a labeled pool and creates a unique namespace.
It runs full, incremental and multi-object backups, restores into new and existing targets, a stdout
export, rejection of a dirty target or incomplete chain, and native-encrypted raw send, all with a
public-only GPG home for backup. It needs `python3`, GnuPG, a local S3 service and OpenZFS 2.3+.

```bash
tests/e2e/local_s3.sh "$RUNTIME_DIR"   # SeaweedFS in the foreground; run the rest elsewhere
source "$RUNTIME_DIR/local_s3.env"
export TEST_S3_ENDPOINT="$LOCAL_S3_ENDPOINT" TEST_S3_BUCKET="$LOCAL_S3_BUCKET"
tests/e2e/run.sh
cargo test --locked --test live_http -- --ignored
TINK_PYTHON=venv/bin/python cargo test --locked --test crypto_stream official_tink_runtime_bidirectional -- --ignored
```

Each scenario is a file in `tests/e2e/cases/`. `run.sh --list` names them, `--case NAME` runs one
case plus the cases it builds on, and `E2E_TRACE=1` logs every command.

`E2E_RUNTIME_DIR` (runtime files and mountpoints, default under `target/test-artifacts`) and
`E2E_GPG_DIR` (short GnuPG home path) must not exist beforehand. Failed runs keep them for diagnosis.
[`s3_probe.py`](tests/tooling/s3_probe.py) checks an endpoint's lock, multipart and range behavior.

**Never run [`ci_e2e.sh`](tests/provision/ci_e2e.sh) locally**: it creates and destroys its own pool and
only runs on fresh GitHub-hosted VMs.

## Cloud Copilot handoff

[`copilot-setup-steps.yml`](.github/workflows/copilot-setup-steps.yml) prepares the cloud agent's VM
(Ubuntu 26.04). It installs ZFS, Rust, GnuPG, Python and ShellCheck, then runs
[`copilot_setup.sh`](tests/provision/copilot_setup.sh). That script installs checksum-verified SeaweedFS
4.48 and Tink 1.16.1, creates a 4 GiB sparse-file pool labeled `user:isdev=yes`, records its GUID,
precompiles the tests, and writes `target/copilot-dev/env.sh` last. It refuses to run anywhere but a
fresh hosted VM.

Agents use only that pool:

```bash
source target/copilot-dev/env.sh
bash tests/provision/copilot_setup.sh --verify
```

`--verify` checks the pool's label and GUID, starts a temporary SeaweedFS, and runs the Tink, live HTTP,
probe and ZFS E2E tests. It then stops the service and checks that the pool is still there. Agents never
run the setup bootstrap itself (without `--verify`) or `ci_e2e.sh`. A missing handoff file or a failed
pool check is a setup failure: report it; never import, recreate or relabel a pool. CI runs the same setup and verification, so a broken agent environment fails the gate.

## CI

[CI](.github/workflows/ci.yml) runs on pushes, pull requests, merge groups and manual dispatch, on
standard `ubuntu-26.04` hosted VMs. The jobs are: tests; quality (fmt, clippy, ShellCheck, CI-script tests,
`scripts.release.prepare check`); release build; [RustSec audit](.github/workflows/security.yml) (also weekly); the
cloud setup check; and E2E. `CI Gate` passes only if all of them succeed.

The E2E job ([`ci_e2e.sh`](tests/provision/ci_e2e.sh)) refuses VMs that already have pools. It creates and
labels a temporary pool, starts SeaweedFS with throwaway credentials, and runs every opt-in test
(including a GET that lasts more than 120 s) plus the ZFS E2E script. Its exit trap destroys only that pool,
after checking the GUID. Diagnostics are uploaded without credentials, keys or large data.

PR jobs get read-only permissions and no secrets. Do not switch to `pull_request_target`. Check a pushed
branch with `gh run list --branch BRANCH`, `gh run view RUN_ID --log-failed` and `gh pr checks PR`.
Local results do not replace this remote check.

## Release automation

The steps are in [CONTRIBUTING.md](CONTRIBUTING.md#releases). Release workflows are `workflow_dispatch`
only. In publish mode the controller publishes only from a merged release PR whose exact merge commit passed `CI Gate` on `main`. The tag must be unchanged and
the artifact digests must verify. Privileged jobs check out controller code from `main`. The helpers are
tested by `tests/tooling/test_release.py`, including recovery of an existing release branch without
force-pushing.
