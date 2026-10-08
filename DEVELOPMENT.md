# Development Guidelines

## Devcontainer Setup

Ensure the host has OpenZFS installed and an administrator has provided a development pool. The container uses the
host's ZFS kernel module through `/dev/zfs`; visible pools are not isolated from the host.

Local agent tests must only use existing pools. Do not create or recreate pools, prepare backing devices
or files, import, export or destroy pools, or change pool labels to make them eligible. If no suitable pool exists,
stop and ask the administrator to provide one.

Discover pools on the current machine rather than hard-coding names or backing paths:

```bash
zfs version
zpool list
zpool get user:isdev
zpool status -P
sudo -n true
```

Select only an ONLINE pool whose **pool property** `user:isdev` is exactly `yes`, and verify the selected pool:

```bash
zpool get -j -p user:isdev "$pool"
```

Inspect the selected pool's JSON `properties["user:isdev"].value`: it must be the string `yes`.
Automated discovery uses `zpool list -j -p -o name,health`, requires `ONLINE` state/health, then reads and
rechecks the **pool** property with `zpool get -j -p`. No pool may be created or relabeled to pass this check.
The commands above for general inspection are interactive diagnostics, not formats to parse in scripts.

The label permits isolated development testing, not unrestricted destruction. Follow [AGENTS.md](AGENTS.md):

* Use a unique child namespace such as `$pool/smoke_<unique-id>`, and verify it does not already exist before creating it.
* Change properties, write, receive, roll back and destroy datasets only within the namespace created by the test.
* Use explicit temporary mountpoints and `canmount=on`; confirm `findmnt -n -o FSTYPE -T "$mountpoint"` reports `zfs`
  before writing.
* Set `atime=off` on source and received filesystems so verification reads do not modify the receive target.
* Use `set -euo pipefail` and an exit trap for cleanup. Report cleanup failures and retain diagnostic files when needed.
  Never remove a mountpoint tree while its dataset remains mounted.
* Do not use `zfs receive -F` to hide unexpected target changes.

The acceptance scenarios in [docs/design.md](docs/design.md#acceptance-scenarios) define later validation of the
backup and restore requirements; preparing this environment does not establish that those behaviors are implemented.

## Local S3 and capability probes (opt-in)

Infrastructure-backed tests are opt-in and should not be silently treated as passing when skipped.

Local end-to-end runs consume an existing `ONLINE` development pool whose pool property is `user:isdev=yes`.
Remote CI runs on a fresh standard GitHub-hosted Ubuntu VM and provisions its own temporary file-backed pool.
The hosted bootstrap refuses local and self-hosted execution and refuses VMs with existing pools.

The optional official Tink runtime interoperability test checks encryption and decryption in both directions
against Python Tink 1.16.1 with a 3 MiB payload. With Tink installed in a project-local virtual environment, run:

```bash
TINK_PYTHON=venv/bin/python cargo test --locked --test crypto_stream official_tink_runtime_bidirectional -- --ignored
```

Current support scripts:

* `tests/support/local_s3.sh` — local S3 service bootstrap/helper (actively evolving)
* `tests/support/s3_probe.py` — endpoint capability probe for lock/multipart/range assumptions
* `tests/support/zfs_s3_e2e.sh` — ZFS + S3 end-to-end validation script

Consult script interfaces before use because arguments and behavior may change while the rebuild is in progress.

Current `zfs_s3_e2e.sh` coverage includes:

* isolated safe namespace usage on a discovered `ONLINE` `user:isdev=yes` pool
* backup/restore with a dedicated **public-only** GPG home for encryption selection
* native encrypted ZFS dataset raw backup/recovery checks in that same namespace

The script requires `python3` (standard-library JSON parsing, also used by the capability probes) and OpenZFS
`zfs`/`zpool` `get`/`list` support for `-j`, usually available in 2.3+. It uses ordinary standard-library JSON
decoding, checks map keys against each object's `name`, and validates the pool state, dataset types and
required property values. Unrelated fields and envelope versions are ignored; there is no custom duplicate-key
validator. Use `-j -p`, without `--json-int`, to retain exact decimal GUID strings. Invalid required data or
failed commands, including permission failures while checking namespace absence, are fatal rather than evidence
of a missing dataset or an unlabeled pool. There is no table-output fallback.

The unique test namespace is checked against a successful recursive JSON dataset listing before creation.
Runtime files and explicit mountpoints default to a unique directory under `target/test-artifacts`.
Set `E2E_RUNTIME_DIR` to select a different dedicated diagnostics directory; it must not already exist.
Failed runs retain that directory for diagnostics. Successful cleanup removes it only after destroying the
test namespace and checking that no test mounts remain.
Private and public-only GnuPG homes use a separate short `.gpg_<unique-id>` directory in the project root.
For deep CI checkouts, set `E2E_GPG_DIR` to a short dedicated directory that does not already exist.
The script checks the socket path length before launching GnuPG. Successful cleanup removes those homes
after stopping their agents; failed runs report and retain both runtime directories for diagnostics.
Modification commands do not need JSON output, and binary `zfs receive` input is unchanged. `zfs diff -H`
and `zfs send -nP` are separate machine formats (neither accepts `-j` in OpenZFS 2.4.4).

Once `local_s3.sh` is running and `local_s3.env` is loaded, use the current commands:

```bash
source "$RUNTIME_DIR/local_s3.env"
export TEST_S3_ENDPOINT="$LOCAL_S3_ENDPOINT" TEST_S3_BUCKET="$LOCAL_S3_BUCKET"
tests/support/zfs_s3_e2e.sh
cargo test --test live_http -- --ignored
```

## Capacity and artifact budget

For local infrastructure and test artifacts, keep total incremental usage within **40 GiB** and preserve at least
**20 GiB** free disk space. If both constraints cannot be met, reduce scope or stop and report the blocker.

Prefer generated streams and bounded fixtures over large persistent objects when validating multipart/error paths.

The disposable hosted E2E fixture uses a **2 GiB sparse backing file**, a **4 GiB free-space reserve** and a
**4 GiB incremental service/artifact budget**. Actual fixture data is small; a sparse file is not evidence of
available storage. CI checks measured free space and monitors growth. The hosted wrapper sets
`E2E_MIN_FREE_BYTES` and the existing local S3 budget overrides explicitly; local defaults remain unchanged.

## Local checks and remote acceptance

Before committing, run the offline tests and checks:

```bash
cargo test --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
python3 -m unittest discover -s tests/support -p 'test_ci_*.py' -v
```

Do not run [ci_e2e.sh](tests/support/ci_e2e.sh) on the shared development host, even by forging runner environment
variables. Its infrastructure bootstrap is exclusively for fresh GitHub-hosted VMs. Use
[zfs_s3_e2e.sh](tests/support/zfs_s3_e2e.sh) for local tests with the existing labeled pools instead.

### Hosted workflow

[CI](.github/workflows/ci.yml) runs on pushes, pull requests, manual dispatch and merge groups. It uses fixed
`ubuntu-26.04` standard hosted VMs, not a self-hosted runner, a larger runner or a privileged job container.
Separate jobs run:

* all default Rust unit and integration tests;
* formatting, strict clippy, ShellCheck, and CI gate/hosted-guard tests;
* the release build;
* the reusable [RustSec audit](.github/workflows/security.yml), also run weekly;
* every opt-in test and the real ZFS/S3 E2E harness.

The E2E job installs the distribution's JSON-capable tools to verify the VM has no existing pools, then
[ci_e2e.sh](tests/support/ci_e2e.sh) builds checksum-verified OpenZFS 2.4.4 against the running kernel headers.
It installs matching tools and loads the built modules, checking both unprivileged and sudo command resolution.
The distribution's 2.4.1 lacks the user-defined pool property needed by the shared test harness.
The script creates a uniquely named temporary pool with `user:isdev=yes` and starts checksum-verified SeaweedFS 4.48
with throwaway credentials. It explicitly runs official Tink 1.16.1 bidirectional interoperability, live HTTP
capability tests, the real GET regression lasting more than 120 seconds, and full/multi-step raw ZFS recovery.
GnuPG homes and mountpoints use short dedicated paths under the runner's temporary directory.

An exit trap stops owned services, verifies the created pool's identity/GUID, destroys only that CI pool and
removes its backing file after successful pool shutdown. It refuses to delete mounted runtime trees. Cleanup
failures fail the job; diagnostics are uploaded even on failure. Artifacts exclude S3 credentials, GPG homes,
native ZFS keys and large data/backing files.

PR jobs have read-only repository permissions, no repository secrets and no persisted checkout credentials.
Fork PRs use the same disposable infrastructure, subject to GitHub's approval requirements for first-time
contributors. Do not replace `pull_request` with `pull_request_target` to execute untrusted PR code.

GitHub's licensed Dependency Review feature is not required. RustSec audits the actual lockfile in CI, without
advisory ignore lists; vulnerable dependencies fail the gate.

### Main-branch gate and remote verification

`CI Gate` requires every test, quality, release, audit and E2E job to succeed. Missing, skipped, cancelled or
failed prerequisite jobs cannot produce a successful gate. Main-branch protection requires a PR, this check
from the GitHub Actions app, and an up-to-date branch. No independent human approval is required; administrators
retain the explicitly allowed emergency bypass. Normal force pushes and branch deletion are prohibited.

Commit on a topic branch, push, and verify the exact revision remotely:

```bash
gh run list --branch YOUR_BRANCH
gh run watch RUN_ID --exit-status --compact
gh run view RUN_ID --log-failed
gh pr checks PR_NUMBER
```

Passing local tests or merely linting the workflow is not remote acceptance. All opt-in and real ZFS tests
must actually run. Verify both the run results and the server-side protection settings; workflow YAML alone
does not configure a repository's required checks.