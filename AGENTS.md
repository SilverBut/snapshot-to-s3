# Hints for Agents

## ZFS Development

### Use existing labeled pools only

This section ONLY applies to local running agents's unit/e2e test procedure.

Agents must not create or recreate zpools, prepare backing devices or
files, or import, export, or destroy pools. Use only existing development
pools marked with the pool property `user:isdev=yes`. This is a **zpool
property**, not a dataset property.

Discover eligible pools on the current machine; never hard-code pool
names or backing paths. The container uses the host's ZFS kernel module
through `/dev/zfs`, so visible pools are not isolated from the host.

Easy command to check conditions:

```bash
zfs version
zpool list
zpool get user:isdev
zpool status -P
sudo -n true
```

- Select only an `ONLINE` pool whose `user:isdev` value is exactly `yes`;
  verify the chosen pool with `zpool get -H -o value user:isdev "$pool"`.
  Stop and report an error if no suitable pool exists; ask the user to
  provide a labeled development pool rather than creating one. Never
  change pool properties or labels to make a pool eligible.
- The label permits development testing, not unrestricted destruction.
  Use a unique child namespace, such as `$pool/smoke_<unique-id>`, and
  verify that it does not already exist before creating it.
- Change properties, write data, receive streams, roll back, and destroy
  datasets only within the namespace created by the current test.
- Use explicit temporary mountpoints and `canmount=on` for test
  filesystems. Confirm that they are actually mounted as ZFS with
  `findmnt -n -o FSTYPE -T "$mountpoint"` before writing.
- Set `atime=off` on source and received test filesystems. Verification
  reads can otherwise update the destination after its snapshot and cause
  incremental receive to fail with "destination ... has been modified".
  Do not use `zfs receive -F` to hide unexpected destination changes.
- Use `set -euo pipefail` in test scripts and an exit trap for cleanup.
  Report cleanup failures and retain test files or streams needed to
  diagnose them; never remove a mountpoint tree while its dataset remains
  mounted.

