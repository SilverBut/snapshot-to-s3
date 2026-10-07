# Development Guidelines

## Devcontainer Setup

Ensure the host has OpenZFS installed and an administrator has provided a development pool. The container uses the
host's ZFS kernel module through `/dev/zfs`; visible pools are not isolated from the host.

Development tests and agents must only use existing pools. Do not create or recreate pools, prepare backing devices
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
zpool get -H -o value user:isdev "$pool"
```

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

Current support scripts:

* `tests/support/local_s3.sh` — local S3 service bootstrap/helper (actively evolving)
* `tests/support/s3_probe.py` — endpoint capability probe for lock/multipart/range assumptions
* `tests/support/zfs_s3_e2e.sh` — ZFS + S3 end-to-end validation script

Consult script interfaces before use because arguments and behavior may change while the rebuild is in progress.

Current `zfs_s3_e2e.sh` coverage includes:

* isolated safe namespace usage on a discovered `ONLINE` `user:isdev=yes` pool
* backup/restore with a dedicated **public-only** GPG home for encryption selection
* native encrypted ZFS dataset raw backup/recovery checks in that same namespace

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

## Commit workflow during rebuild

Use small, frequent, locally verified commits coordinated by the parent integration owner. Do not rely on automatic
push/release actions for in-progress rebuild branches.