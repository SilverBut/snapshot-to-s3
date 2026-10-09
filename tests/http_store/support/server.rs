use anyhow::Result;
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

/// Serves one response and captures the complete request, including its body.
pub(crate) async fn fixture(response: &'static str) -> Result<(String, JoinHandle<Vec<u8>>)> {
    fixture_sequence(vec![response.to_owned()]).await
}

/// Serves ordered responses and captures bodies using case-insensitive content lengths.
pub(crate) async fn fixture_sequence(
    responses: Vec<String>,
) -> Result<(String, JoinHandle<Vec<u8>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let mut captured = Vec::new();
        for response in responses {
            let (stream, _) = listener.accept().await.expect("accept fixture request");
            let mut reader = BufReader::new(stream);
            let mut request = Vec::new();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader
                    .read_line(&mut line)
                    .await
                    .expect("read request line");
                request.extend_from_slice(line.as_bytes());
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        content_length = value.trim().parse().expect("parse content length");
                    }
                }
            }
            let mut body = vec![0; content_length];
            reader
                .read_exact(&mut body)
                .await
                .expect("read request body");
            request.extend_from_slice(&body);
            captured.extend_from_slice(&request);
            reader
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .expect("write fixture response");
        }
        captured
    });
    Ok((format!("http://{address}"), task))
}

/// Captures request heads and sends independently delayed response pieces.
pub(crate) async fn paced_fixture(
    responses: Vec<Vec<(Duration, String)>>,
) -> Result<(String, JoinHandle<Vec<u8>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let mut captured = Vec::new();
        let mut writers = Vec::new();
        for pieces in responses {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                captured.extend_from_slice(line.as_bytes());
                if line == "\r\n" {
                    break;
                }
            }
            writers.push(tokio::spawn(async move {
                for (delay, piece) in pieces {
                    tokio::time::sleep(delay).await;
                    if reader.get_mut().write_all(piece.as_bytes()).await.is_err() {
                        break;
                    }
                }
            }));
        }
        for writer in writers {
            writer.await.unwrap();
        }
        captured
    });
    Ok((format!("http://{address}"), server))
}

/// Consumes a completion body using the exact lowercase header prefix, optionally capturing its head.
pub(crate) async fn read_completion_request(
    reader: &mut BufReader<TcpStream>,
    mut captured_head: Option<&mut String>,
) {
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        if let Some(request) = captured_head.as_mut() {
            request.push_str(&line);
        }
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("content-length: ") {
            length = value.trim().parse::<usize>().unwrap();
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await.unwrap();
}
