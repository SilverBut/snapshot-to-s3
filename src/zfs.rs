use crate::model::{validate_dataset, validate_guid, Reader, SnapshotName};
use crate::process::{
    read_bounded_stderr, read_bounded_stdout, spawn_completion_task, DEFAULT_STDERR_LIMIT,
    DEFAULT_STDOUT_LIMIT,
};
use crate::zfs_api::{SendStream, SnapshotInfo, TargetInfo, Zfs};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

pub struct SystemZfs {
    zfs_bin: OsString,
    zpool_bin: OsString,
    stderr_limit: usize,
    stdout_limit: usize,
}

impl SystemZfs {
    pub fn new() -> Self {
        Self {
            zfs_bin: std::env::var_os("SNAPSHOT_TO_S3_ZFS_BIN").unwrap_or_else(|| "zfs".into()),
            zpool_bin: std::env::var_os("SNAPSHOT_TO_S3_ZPOOL_BIN")
                .unwrap_or_else(|| "zpool".into()),
            stderr_limit: DEFAULT_STDERR_LIMIT,
            stdout_limit: DEFAULT_STDOUT_LIMIT,
        }
    }

    async fn run_capture(&self, program: &OsString, args: &[String]) -> Result<CommandOutput> {
        let mut child = Command::new(program)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to execute {:?} {:?}", program, args))?;

        let stdout_task = tokio::spawn(read_bounded_stdout(child.stdout.take(), self.stdout_limit));
        let stderr_task = tokio::spawn(read_bounded_stderr(child.stderr.take(), self.stderr_limit));
        let status = child.wait().await.context("failed waiting for command")?;

        let stdout = stdout_task
            .await
            .context("stdout collector join failed")??;
        let stderr = stderr_task
            .await
            .context("stderr collector join failed")??;

        if stdout.truncated {
            bail!(
                "command stdout exceeded {} bytes: {:?} {:?}",
                self.stdout_limit,
                program,
                args
            );
        }

        Ok(CommandOutput {
            status,
            stdout: stdout.text,
            stderr: stderr.text,
        })
    }

    async fn ensure_pool_exists(&self, dataset: &str) -> Result<()> {
        let pool = dataset
            .split('/')
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("dataset is missing pool name"))?;

        let args = vec![
            "list".into(),
            "-H".into(),
            "-o".into(),
            "name".into(),
            pool.into(),
        ];
        let output = self.run_capture(&self.zpool_bin, &args).await?;
        if output.status.success() {
            return Ok(());
        }

        if looks_missing(&output.stderr) {
            bail!("target pool does not exist: {pool}");
        }
        bail!("failed checking pool {pool}: {}", output.stderr.trim())
    }

    async fn dataset_type(&self, dataset: &str) -> Result<Option<String>> {
        let args = vec![
            "get".into(),
            "-H".into(),
            "-p".into(),
            "-o".into(),
            "value".into(),
            "type".into(),
            dataset.into(),
        ];
        let output = self.run_capture(&self.zfs_bin, &args).await?;
        if output.status.success() {
            return Ok(Some(output.stdout.trim().to_string()));
        }

        if looks_missing(&output.stderr) {
            return Ok(None);
        }
        bail!(
            "failed reading dataset type for {dataset}: {}",
            output.stderr.trim()
        )
    }

    async fn ensure_filesystem(&self, dataset: &str) -> Result<()> {
        match self.dataset_type(dataset).await? {
            Some(kind) if kind == "filesystem" => Ok(()),
            Some(kind) => bail!("dataset is not a filesystem: {dataset} ({kind})"),
            None => bail!("dataset does not exist: {dataset}"),
        }
    }

    async fn snapshot_props(
        &self,
        name: &SnapshotName,
    ) -> Result<Option<BTreeMap<String, String>>> {
        let args = vec![
            "get".into(),
            "-H".into(),
            "-p".into(),
            "-o".into(),
            "property,value".into(),
            "guid,createtxg".into(),
            name.full_name(),
        ];
        let output = self.run_capture(&self.zfs_bin, &args).await?;
        if !output.status.success() {
            if looks_missing(&output.stderr) {
                return Ok(None);
            }
            bail!(
                "failed reading snapshot properties for {}: {}",
                name.full_name(),
                output.stderr.trim()
            );
        }

        let mut values = BTreeMap::new();
        for line in output.stdout.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let (property, value) = line
                .split_once('\t')
                .ok_or_else(|| anyhow!("invalid zfs get line: {line}"))?;
            values.insert(property.to_string(), value.to_string());
        }
        Ok(Some(values))
    }

    async fn filesystem_guid(&self, dataset: &str) -> Result<String> {
        let args = vec![
            "get".into(),
            "-H".into(),
            "-p".into(),
            "-o".into(),
            "value".into(),
            "guid".into(),
            dataset.into(),
        ];
        let output = self.run_capture(&self.zfs_bin, &args).await?;
        if !output.status.success() {
            bail!(
                "failed reading filesystem guid for {dataset}: {}",
                output.stderr.trim()
            );
        }
        let guid = output.stdout.trim().to_string();
        validate_guid(&guid)?;
        Ok(guid)
    }

    fn snapshot_from_props(
        name: SnapshotName,
        props: BTreeMap<String, String>,
        volume_guid: String,
    ) -> Result<SnapshotInfo> {
        let guid = props
            .get("guid")
            .cloned()
            .ok_or_else(|| anyhow!("missing guid for {}", name.full_name()))?;
        validate_guid(&guid)?;
        validate_guid(&volume_guid)?;

        let createtxg = props
            .get("createtxg")
            .ok_or_else(|| anyhow!("missing createtxg for {}", name.full_name()))?
            .parse::<u64>()
            .with_context(|| format!("invalid createtxg for {}", name.full_name()))?;

        Ok(SnapshotInfo {
            name,
            guid,
            volume_guid,
            createtxg,
        })
    }

    async fn send_estimate_internal(
        &self,
        current: &SnapshotName,
        base: Option<&SnapshotName>,
    ) -> Result<Option<u64>> {
        let mut args = vec!["send".into(), "-nP".into(), "-w".into()];
        if let Some(base) = base {
            args.push("-i".into());
            args.push(base.full_name());
        }
        args.push(current.full_name());

        let output = self.run_capture(&self.zfs_bin, &args).await?;
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

impl Default for SystemZfs {
    fn default() -> Self {
        Self::new()
    }
}

struct CommandOutput {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

fn looks_missing(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("does not exist")
        || lower.contains("no such pool")
        || lower.contains("dataset does not exist")
        || lower.contains("snapshot does not exist")
}

fn looks_candidate_invalid(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("does not exist")
        || lower.contains("not an earlier snapshot")
        || lower.contains("incremental source")
}

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
            .ok_or_else(|| anyhow!("snapshot does not exist: {}", name.full_name()))?;
        Self::snapshot_from_props(name.clone(), props, volume_guid)
    }

    async fn snapshots(&self, dataset: &str) -> Result<Vec<SnapshotInfo>> {
        validate_dataset(dataset)?;
        self.ensure_filesystem(dataset).await?;

        let args = vec![
            "list".into(),
            "-H".into(),
            "-t".into(),
            "snapshot".into(),
            "-o".into(),
            "name".into(),
            "-d".into(),
            "1".into(),
            "-s".into(),
            "creation".into(),
            dataset.into(),
        ];
        let output = self.run_capture(&self.zfs_bin, &args).await?;
        if !output.status.success() {
            bail!(
                "failed listing snapshots for {dataset}: {}",
                output.stderr.trim()
            );
        }

        let mut items = Vec::new();
        for line in output.stdout.lines() {
            let full = line.trim();
            if full.is_empty() {
                continue;
            }
            let parsed = SnapshotName::parse(&format!("zfs:{full}"))?;
            if parsed.dataset != dataset {
                continue;
            }
            let info = self.snapshot(&parsed).await?;
            items.push(info);
        }
        Ok(items)
    }

    async fn written(&self, base: &SnapshotName, current: &SnapshotName) -> Result<Option<u64>> {
        let _ = self.snapshot(current).await?;
        if self.snapshot_props(base).await?.is_none() {
            return Ok(None);
        }

        let args = vec![
            "get".into(),
            "-H".into(),
            "-p".into(),
            "-o".into(),
            "value".into(),
            format!("written@{}", base.snapshot),
            current.full_name(),
        ];
        let output = self.run_capture(&self.zfs_bin, &args).await?;
        if !output.status.success() {
            if looks_candidate_invalid(&output.stderr) {
                return Ok(None);
            }
            bail!("failed reading written size: {}", output.stderr.trim());
        }

        output
            .stdout
            .trim()
            .parse::<u64>()
            .with_context(|| format!("invalid written value: {}", output.stdout.trim()))
            .map(Some)
    }

    async fn estimate(
        &self,
        current: &SnapshotName,
        base: Option<&SnapshotName>,
    ) -> Result<Option<u64>> {
        let _ = self.snapshot(current).await?;
        if let Some(base) = base {
            if self.snapshot_props(base).await?.is_none() {
                return Ok(None);
            }
        }
        self.send_estimate_internal(current, base).await
    }

    async fn send(
        &self,
        current: &SnapshotName,
        base: Option<&SnapshotName>,
    ) -> Result<SendStream> {
        let _ = self.snapshot(current).await?;
        if let Some(base) = base {
            let _ = self.snapshot(base).await?;
        }

        let mut args = vec!["send".into(), "-w".into()];
        if let Some(base) = base {
            args.push("-i".into());
            args.push(base.full_name());
        }
        args.push(current.full_name());

        let mut child = Command::new(&self.zfs_bin)
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to execute {:?} {:?}", self.zfs_bin, args))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("zfs send did not provide stdout"))?;

        let cancel = CancellationToken::new();
        let completion =
            spawn_completion_task(child, cancel.clone(), self.stderr_limit, "zfs send");

        let reader: Reader = Box::new(CancelOnDropReader {
            inner: stdout,
            cancel: cancel.clone(),
        });

        Ok(SendStream {
            reader,
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
                snapshots.sort_by_key(|s| s.createtxg);
                Ok(TargetInfo {
                    exists: true,
                    snapshots,
                })
            }
        }
    }

    async fn check_clean(&self, latest: &SnapshotName) -> Result<()> {
        let args = vec![
            "diff".into(),
            "-H".into(),
            latest.full_name(),
            latest.dataset.clone(),
        ];
        let output = self.run_capture(&self.zfs_bin, &args).await?;
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

        let mut child = Command::new(&self.zfs_bin)
            .arg("receive")
            .arg("-u")
            .arg(dataset)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to execute {:?} receive", self.zfs_bin))?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("zfs receive did not provide stdin"))?;
        let stderr_task = tokio::spawn(read_bounded_stderr(child.stderr.take(), self.stderr_limit));

        let copy_result = tokio::io::copy(stream.as_mut(), &mut stdin).await;
        drop(stdin);

        let status = match copy_result {
            Ok(_) => child
                .wait()
                .await
                .context("failed waiting for zfs receive")?,
            Err(copy_err) => {
                let pid = child
                    .id()
                    .ok_or_else(|| anyhow!("zfs receive missing pid"))?;
                if let Err(kill_err) = child.kill().await {
                    if kill_err.kind() != std::io::ErrorKind::InvalidInput {
                        return Err(kill_err)
                            .with_context(|| format!("failed killing zfs receive pid {pid}"));
                    }
                }
                let _ = child.wait().await;
                let stderr = stderr_task
                    .await
                    .context("stderr collector join failed")??
                    .text;
                bail!(
                    "failed streaming input into zfs receive: {copy_err}; stderr: {}",
                    stderr.trim()
                );
            }
        };

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
