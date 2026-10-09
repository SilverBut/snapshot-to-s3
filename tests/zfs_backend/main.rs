//! Tests SystemZfs command handling, streams, and JSON using configurable fake zfs/zpool executables.
//! The fakes record arguments and accept file-based JSON overrides without requiring a real ZFS pool.

mod commands;
mod json;
mod streams;
mod support;
