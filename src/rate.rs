use anyhow::{bail, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::{sleep, Duration, Instant};

pub async fn copy_limited<R, W>(
    input: &mut R,
    output: &mut W,
    bytes_per_second: Option<u64>,
) -> Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if matches!(bytes_per_second, Some(0)) {
        bail!("bytes_per_second must be greater than zero");
    }

    let mut transferred: u64 = 0;
    let mut buffer = vec![0u8; 64 * 1024];
    let start = Instant::now();

    loop {
        let read = input.read(&mut buffer).await?;
        if read == 0 {
            break;
        }

        output.write_all(&buffer[..read]).await?;
        transferred += read as u64;

        if let Some(bps) = bytes_per_second {
            let expected = Duration::from_secs_f64(transferred as f64 / bps as f64);
            let elapsed = start.elapsed();
            if expected > elapsed {
                sleep(expected - elapsed).await;
            }
        }
    }

    output.flush().await?;
    Ok(transferred)
}
