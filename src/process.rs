use anyhow::{bail, Context, Result};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStderr, ChildStdout};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_STDERR_LIMIT: usize = 64 * 1024;
pub const DEFAULT_STDOUT_LIMIT: usize = 64 * 1024;

pub struct BoundedText {
    pub text: String,
    pub truncated: bool,
}

async fn read_bounded_text<R>(
    reader: Option<R>,
    limit: usize,
    read_error: &'static str,
    truncated_marker: &'static str,
) -> Result<BoundedText>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut reader = match reader {
        Some(reader) => reader,
        None => {
            return Ok(BoundedText {
                text: String::new(),
                truncated: false,
            });
        }
    };

    let mut buf = vec![0u8; 8192];
    let mut collected = Vec::new();
    let mut truncated = false;

    loop {
        let n = reader.read(&mut buf).await.context(read_error)?;
        if n == 0 {
            break;
        }

        let spare = limit.saturating_sub(collected.len());
        if spare > 0 {
            let copy = spare.min(n);
            collected.extend_from_slice(&buf[..copy]);
        }
        if n > spare {
            truncated = true;
        }
    }

    let mut text = String::from_utf8_lossy(&collected).into_owned();
    if truncated {
        text.push_str(truncated_marker);
    }
    Ok(BoundedText { text, truncated })
}

pub async fn read_bounded_stdout(stdout: Option<ChildStdout>, limit: usize) -> Result<BoundedText> {
    read_bounded_text(
        stdout,
        limit,
        "failed reading child stdout",
        "\n[stdout truncated]",
    )
    .await
}

pub async fn read_bounded_stderr(stderr: Option<ChildStderr>, limit: usize) -> Result<BoundedText> {
    read_bounded_text(
        stderr,
        limit,
        "failed reading child stderr",
        "\n[stderr truncated]",
    )
    .await
}

pub async fn kill_and_reap(child: &mut Child) -> Result<()> {
    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("child process has no pid"))?;

    if let Err(error) = child.kill().await {
        if error.kind() != std::io::ErrorKind::InvalidInput {
            return Err(error).with_context(|| format!("failed killing pid {pid}"));
        }
    }

    child
        .wait()
        .await
        .with_context(|| format!("failed waiting pid {pid} after kill"))?;
    Ok(())
}

pub fn spawn_completion_task(
    mut child: Child,
    cancel: CancellationToken,
    stderr_limit: usize,
    label: &'static str,
) -> JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let stderr_task = tokio::spawn(read_bounded_stderr(child.stderr.take(), stderr_limit));

        let status = tokio::select! {
            _ = cancel.cancelled() => {
                kill_and_reap(&mut child).await?;
                let _ = stderr_task.await.context("stderr collector join failed")??;
                bail!("{label} cancelled");
            }
            status = child.wait() => status.context("failed waiting for child process")?,
        };

        let stderr = stderr_task
            .await
            .context("stderr collector join failed")??
            .text;

        if !status.success() {
            bail!(
                "{label} failed with status {status}; stderr: {}",
                stderr.trim()
            );
        }

        Ok(())
    })
}
