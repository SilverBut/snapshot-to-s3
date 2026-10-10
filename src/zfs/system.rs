//! [`Zfs`] backed by the host `zfs` and `zpool` commands.

use super::{json, SendStream, SnapshotInfo, TargetInfo, Zfs};
use crate::model::{validate_dataset, validate_guid, Reader, SnapshotName};
use crate::process::{
    kill_and_reap, read_bounded_stderr, read_bounded_stdout, spawn_completion_task,
    DEFAULT_STDERR_LIMIT, DEFAULT_STDOUT_LIMIT,
};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt::Debug;
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::task::{Context as TaskContext, Poll};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Runs `zfs`/`zpool` from `PATH`, or from `SNAPSHOT_TO_S3_ZFS_BIN` and
/// `SNAPSHOT_TO_S3_ZPOOL_BIN` (used by tests to inject fakes).
pub struct SystemZfs {
    zfs_bin: OsString,
    zpool_bin: OsString,
    stderr_limit: usize,
    stdout_limit: usize,
}

/// Captured stdout of a snapshot listing: about 300 bytes per snapshot.
const LIST_STDOUT_LIMIT: usize = 64 * 1024 * 1024;

struct CommandOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

impl Default for SystemZfs {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemZfs {
    pub fn new() -> Self {
        let bin = |var, default: &str| std::env::var_os(var).unwrap_or_else(|| default.into());
        Self {
            zfs_bin: bin("SNAPSHOT_TO_S3_ZFS_BIN", "zfs"),
            zpool_bin: bin("SNAPSHOT_TO_S3_ZPOOL_BIN", "zpool"),
            stderr_limit: DEFAULT_STDERR_LIMIT,
            stdout_limit: DEFAULT_STDOUT_LIMIT,
        }
    }

    fn command(program: &OsString) -> Command {
        let mut command = Command::new(program);
        command.env("LC_ALL", "C").kill_on_drop(true);
        command
    }

    /// Runs a command to completion with bounded stdout and stderr capture.
    async fn run_capture<S>(&self, program: &OsString, args: &[S]) -> Result<CommandOutput>
    where
        S: AsRef<OsStr> + Debug,
    {
        self.run_capture_bounded(program, args, self.stdout_limit)
            .await
    }

    async fn run_capture_bounded<S>(
        &self,
        program: &OsString,
        args: &[S],
        stdout_limit: usize,
    ) -> Result<CommandOutput>
    where
        S: AsRef<OsStr> + Debug,
    {
        tracing::debug!("running {program:?} {args:?}");
        let mut child = Self::command(program)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to execute {program:?} {args:?}"))?;
        let stdout_task = tokio::spawn(read_bounded_stdout(child.stdout.take(), stdout_limit));
        let stderr_task = tokio::spawn(read_bounded_stderr(child.stderr.take(), self.stderr_limit));
        let status = child.wait().await.context("failed waiting for command")?;
        let stdout = stdout_task
            .await
            .context("stdout collector join failed")??;
        let stderr = stderr_task
            .await
            .context("stderr collector join failed")??;
        tracing::debug!("{program:?} exited with {}", status);
        if stdout.truncated {
            bail!("command stdout exceeded {stdout_limit} bytes: {program:?} {args:?}");
        }
        Ok(CommandOutput {
            status,
            stdout: stdout.text,
            stderr: stderr.text,
        })
    }

    async fn zfs<S: AsRef<OsStr> + Debug>(&self, args: &[S]) -> Result<CommandOutput> {
        self.run_capture(&self.zfs_bin, args).await
    }

    /// `zfs get -j -p <properties> <name>`.
    async fn zfs_get(&self, properties: &str, name: &str) -> Result<CommandOutput> {
        self.zfs(&["get", "-j", "-p", properties, name]).await
    }

    async fn ensure_pool_exists(&self, dataset: &str) -> Result<()> {
        let pool = dataset
            .split('/')
            .next()
            .filter(|pool| !pool.is_empty())
            .ok_or_else(|| anyhow!("dataset is missing pool name"))?;
        let output = self
            .run_capture(&self.zpool_bin, &["list", "-j", "-p", "-o", "name", pool])
            .await?;
        if output.status.success() {
            return json::verify_pool(&output.stdout, pool);
        }
        if looks_missing(&output.stderr) {
            bail!("target pool does not exist: {pool}");
        }
        bail!("failed checking pool {pool}: {}", output.stderr.trim())
    }

    /// Lowercase dataset type, or `None` if the dataset does not exist.
    async fn dataset_type(&self, dataset: &str) -> Result<Option<String>> {
        let output = self.zfs_get("type", dataset).await?;
        if !output.status.success() {
            if looks_missing(&output.stderr) {
                return Ok(None);
            }
            bail!(
                "failed reading dataset type for {dataset}: {}",
                output.stderr.trim()
            );
        }
        let entry = json::dataset(&output.stdout, "zfs get", dataset)?;
        let kind = entry.property("type")?;
        if !entry.kind.eq_ignore_ascii_case(kind) {
            bail!("zfs get JSON has inconsistent dataset types for {dataset}");
        }
        Ok(Some(kind.to_string()))
    }

    async fn ensure_filesystem(&self, dataset: &str) -> Result<()> {
        match self.dataset_type(dataset).await? {
            Some(kind) if kind == "filesystem" => Ok(()),
            Some(kind) => bail!("dataset is not a filesystem: {dataset} ({kind})"),
            None => bail!("dataset does not exist: {dataset}"),
        }
    }

    /// `guid` and `createtxg` of a snapshot, or `None` if it does not exist.
    async fn snapshot_props(
        &self,
        name: &SnapshotName,
    ) -> Result<Option<BTreeMap<String, String>>> {
        let full_name = name.full_name();
        let output = self.zfs_get("guid,createtxg", &full_name).await?;
        if !output.status.success() {
            if looks_missing(&output.stderr) {
                return Ok(None);
            }
            bail!(
                "failed reading snapshot properties for {full_name}: {}",
                output.stderr.trim()
            );
        }
        let entry = json::dataset(&output.stdout, "zfs get", &full_name)?;
        if entry.kind != "SNAPSHOT" {
            bail!("zfs get JSON is not a snapshot: {full_name}");
        }
        Ok(Some(entry.into_properties()))
    }

    async fn filesystem_guid(&self, dataset: &str) -> Result<String> {
        let output = self.zfs_get("guid", dataset).await?;
        if !output.status.success() {
            bail!(
                "failed reading filesystem guid for {dataset}: {}",
                output.stderr.trim()
            );
        }
        let entry = json::dataset(&output.stdout, "zfs get", dataset)?;
        if entry.kind != "FILESYSTEM" {
            bail!("zfs get JSON is not a filesystem: {dataset}");
        }
        let guid = entry.property("guid")?.to_string();
        validate_guid(&guid)?;
        Ok(guid)
    }

    async fn send_estimate(
        &self,
        current: &SnapshotName,
        base: Option<&SnapshotName>,
    ) -> Result<Option<u64>> {
        let output = self.zfs(&send_args(&["-nP", "-w"], current, base)).await?;
        if !output.status.success() {
            if base.is_some() && looks_candidate_invalid(&output.stderr) {
                return Ok(None);
            }
            bail!("zfs send estimate failed: {}", output.stderr.trim());
        }
        for line in output.stdout.lines().chain(output.stderr.lines()) {
            if let Some(raw) = line.strip_prefix("size\t") {
                return raw
                    .trim()
                    .parse::<u64>()
                    .with_context(|| format!("invalid estimate value: {raw}"))
                    .map(Some);
            }
        }
        bail!("zfs send -nP did not emit size")
    }
}

fn snapshot_from_props(
    name: SnapshotName,
    props: BTreeMap<String, String>,
    volume_guid: String,
) -> Result<SnapshotInfo> {
    let guid = props
        .get("guid")
        .cloned()
        .ok_or_else(|| anyhow!("missing guid for {name}"))?;
    validate_guid(&guid)?;
    validate_guid(&volume_guid)?;
    let createtxg = props
        .get("createtxg")
        .ok_or_else(|| anyhow!("missing createtxg for {name}"))?
        .parse::<u64>()
        .with_context(|| format!("invalid createtxg for {name}"))?;
    Ok(SnapshotInfo {
        name,
        guid,
        volume_guid,
        createtxg,
    })
}

/// `send <flags> [-i base] current` as owned argument strings.
fn send_args(flags: &[&str], current: &SnapshotName, base: Option<&SnapshotName>) -> Vec<String> {
    let mut args = vec!["send".to_string()];
    args.extend(flags.iter().map(|flag| flag.to_string()));
    if let Some(base) = base {
        args.push("-i".into());
        args.push(base.full_name());
    }
    args.push(current.full_name());
    args
}

fn looks_missing(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("does not exist") || lower.contains("no such pool")
}

fn looks_candidate_invalid(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("does not exist")
        || lower.contains("not an earlier snapshot")
        || lower.contains("incremental source")
}

/// Cancels the `zfs send` completion task when the reader is dropped.
struct CancelOnDropReader<R> {
    inner: R,
    cancel: CancellationToken,
}

impl<R> Drop for CancelOnDropReader<R> {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for CancelOnDropReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

#[async_trait]
impl Zfs for SystemZfs {
    async fn snapshot(&self, name: &SnapshotName) -> Result<SnapshotInfo> {
        validate_dataset(&name.dataset)?;
        self.ensure_filesystem(&name.dataset).await?;
        let volume_guid = self.filesystem_guid(&name.dataset).await?;
        let props = self
            .snapshot_props(name)
            .await?
            .ok_or_else(|| anyhow!("snapshot does not exist: {name}"))?;
        snapshot_from_props(name.clone(), props, volume_guid)
    }

    async fn snapshots(&self, dataset: &str) -> Result<Vec<SnapshotInfo>> {
        validate_dataset(dataset)?;
        self.ensure_filesystem(dataset).await?;
        let args = [
            "list", "-j", "-p", "-t", "snapshot", "-o", "name", "-d", "1", "-s", "creation",
            dataset,
        ];
        let output = self
            .run_capture_bounded(&self.zfs_bin, &args, LIST_STDOUT_LIMIT)
            .await?;
        if !output.status.success() {
            bail!(
                "failed listing snapshots for {dataset}: {}",
                output.stderr.trim()
            );
        }
        let mut items = Vec::new();
        for entry in json::datasets(&output.stdout, "zfs list")?.into_values() {
            if entry.kind != "SNAPSHOT" {
                bail!("zfs list JSON contains a non-snapshot: {}", entry.name);
            }
            let name = SnapshotName::parse(&entry.name)?;
            if name.dataset == dataset {
                items.push(self.snapshot(&name).await?);
            }
        }
        Ok(items)
    }

    async fn written(&self, base: &SnapshotName, current: &SnapshotName) -> Result<Option<u64>> {
        self.snapshot(current).await?;
        if self.snapshot_props(base).await?.is_none() {
            return Ok(None);
        }
        let property = format!("written@{}", base.snapshot);
        let full_name = current.full_name();
        let output = self.zfs_get(&property, &full_name).await?;
        if !output.status.success() {
            if looks_candidate_invalid(&output.stderr) {
                return Ok(None);
            }
            bail!("failed reading written size: {}", output.stderr.trim());
        }
        let entry = json::dataset(&output.stdout, "zfs get", &full_name)?;
        if entry.kind != "SNAPSHOT" {
            bail!("zfs get JSON is not a snapshot: {full_name}");
        }
        entry
            .property(&property)?
            .parse::<u64>()
            .with_context(|| format!("invalid {property} value for {full_name}"))
            .map(Some)
    }

    async fn estimate(
        &self,
        current: &SnapshotName,
        base: Option<&SnapshotName>,
    ) -> Result<Option<u64>> {
        self.snapshot(current).await?;
        if let Some(base) = base {
            if self.snapshot_props(base).await?.is_none() {
                return Ok(None);
            }
        }
        self.send_estimate(current, base).await
    }

    async fn send(
        &self,
        current: &SnapshotName,
        base: Option<&SnapshotName>,
    ) -> Result<SendStream> {
        self.snapshot(current).await?;
        if let Some(base) = base {
            self.snapshot(base).await?;
        }
        let args = send_args(&["-w"], current, base);
        let mut child = Self::command(&self.zfs_bin)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to execute {:?} {args:?}", self.zfs_bin))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("zfs send did not provide stdout"))?;
        let cancel = CancellationToken::new();
        let completion =
            spawn_completion_task(child, cancel.clone(), self.stderr_limit, "zfs send");
        Ok(SendStream {
            reader: Box::new(CancelOnDropReader {
                inner: stdout,
                cancel: cancel.clone(),
            }),
            completion,
            cancel,
        })
    }

    async fn target(&self, dataset: &str) -> Result<TargetInfo> {
        validate_dataset(dataset)?;
        self.ensure_pool_exists(dataset).await?;
        match self.dataset_type(dataset).await? {
            None => Ok(TargetInfo {
                exists: false,
                snapshots: Vec::new(),
            }),
            Some(kind) if kind != "filesystem" => {
                bail!("target dataset is not a filesystem: {dataset} ({kind})")
            }
            Some(_) => {
                let mut snapshots = self.snapshots(dataset).await?;
                snapshots.sort_by_key(|snapshot| snapshot.createtxg);
                Ok(TargetInfo {
                    exists: true,
                    snapshots,
                })
            }
        }
    }

    async fn check_clean(&self, latest: &SnapshotName) -> Result<()> {
        let output = self
            .zfs(&[
                "diff",
                "-H",
                latest.full_name().as_str(),
                latest.dataset.as_str(),
            ])
            .await?;
        if !output.status.success() {
            bail!("zfs diff failed: {}", output.stderr.trim());
        }
        if !output.stdout.trim().is_empty() {
            bail!("target filesystem changed since latest snapshot");
        }
        Ok(())
    }

    async fn receive(&self, dataset: &str, stream: &mut Reader) -> Result<()> {
        validate_dataset(dataset)?;
        self.ensure_pool_exists(dataset).await?;
        let mut child = Self::command(&self.zfs_bin)
            .args(["receive", "-u", dataset])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to execute {:?} receive", self.zfs_bin))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("zfs receive did not provide stdin"))?;
        let stderr_task = tokio::spawn(read_bounded_stderr(child.stderr.take(), self.stderr_limit));
        let copied = tokio::io::copy(stream.as_mut(), &mut stdin).await;
        drop(stdin);
        if let Err(error) = copied {
            kill_and_reap(&mut child)
                .await
                .context("stop zfs receive")?;
            let stderr = stderr_task
                .await
                .context("stderr collector join failed")??
                .text;
            bail!(
                "failed streaming input into zfs receive: {error}; stderr: {}",
                stderr.trim()
            );
        }
        let status = child
            .wait()
            .await
            .context("failed waiting for zfs receive")?;
        let stderr = stderr_task
            .await
            .context("stderr collector join failed")??
            .text;
        if !status.success() {
            bail!(
                "zfs receive failed with status {status}; stderr: {}",
                stderr.trim()
            );
        }
        Ok(())
    }
}
