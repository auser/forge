//! Bounded whole-response reads for background model calls.
//!
//! Check the advertised length as an early refusal only. The actual decoded
//! chunks are authoritative, including errors and SSE envelopes. Never reserve
//! from an untrusted length or include response contents in read errors.

use forge_core::ForgeError;

pub(crate) async fn bounded_bytes(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, ForgeError> {
    let too_large = || ForgeError::provider("model response exceeds byte limit");
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ForgeError::provider("reading bounded model response failed"))?
    {
        if chunk.len() > max_bytes.saturating_sub(body.len()) {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub(crate) fn streaming_unsupported() -> ForgeError {
    ForgeError::provider("bounded model responses require complete, not stream_complete")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AnthropicModel, CodexModel, CredentialSource, EgressPolicy, OpenAiCompatibleModel,
        ResolvedCredential,
    };
    use forge_core::{CompletionRequest, Message, ModelCapabilities, ModelProvider};
    use std::{sync::Arc, time::Duration};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Clone, Copy, Debug)]
    enum Framing {
        Length,
        Chunked,
        Close,
        // Transfer-Encoding wins over a lying Content-Length. The reader
        // must still count decoded bytes, not trust the advertised length.
        MisleadingLength,
    }

    async fn endpoint(
        status: u16,
        body: &[u8],
        framing: Framing,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let body = body.to_vec();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 4096];
            loop {
                let n = socket.read(&mut buf).await.unwrap();
                assert_ne!(n, 0);
                request.extend_from_slice(&buf[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .map(|n| n.trim().parse::<usize>().unwrap())
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let framing_headers = match framing {
                Framing::Length => format!("Content-Length: {}\r\n", body.len()),
                Framing::Chunked => "Transfer-Encoding: chunked\r\n".into(),
                Framing::Close => String::new(),
                Framing::MisleadingLength => {
                    "Transfer-Encoding: chunked\r\nContent-Length: 1\r\n".into()
                }
            };
            let headers =
                format!("HTTP/1.1 {status} Test\r\nConnection: close\r\n{framing_headers}\r\n");
            if socket.write_all(headers.as_bytes()).await.is_err() {
                return;
            }
            if matches!(framing, Framing::Chunked | Framing::MisleadingLength) {
                for chunk in body.chunks(4096) {
                    let header = format!("{:x}\r\n", chunk.len());
                    let mut frame = header.into_bytes();
                    frame.extend_from_slice(chunk);
                    frame.extend_from_slice(b"\r\n");
                    if socket.write_all(&frame).await.is_err() {
                        return; // Refusal drops the response immediately.
                    }
                }
                let _ = socket.write_all(b"0\r\n\r\n").await;
            } else {
                let _ = socket.write_all(&body).await;
            }
        });
        (url, task)
    }

    async fn read(body: &[u8], framing: Framing, limit: usize) -> Result<Vec<u8>, ForgeError> {
        let (url, server) = endpoint(200, body, framing).await;
        let response = reqwest::Client::new().get(url).send().await.unwrap();
        let result = bounded_bytes(response, limit).await;
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn bounded_reader_counts_actual_bytes_for_every_framing() {
        for framing in [
            Framing::Length,
            Framing::Chunked,
            Framing::Close,
            Framing::MisleadingLength,
        ] {
            assert_eq!(read(b"12345", framing, 5).await.unwrap(), b"12345");
            let error = read(&vec![b'x'; 300_000], framing, 256 * 1024)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("exceeds byte limit"), "{framing:?}: {error}");
            assert!(!error.contains("xxxx"));
        }
        assert!(read(b"", Framing::Length, 0).await.unwrap().is_empty());
        assert!(read(b"x", Framing::Close, 0).await.is_err());
    }

    #[tokio::test]
    async fn advertised_oversize_is_refused_without_waiting_for_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\n\r\n")
                .await
                .unwrap();
            // Stay open, but send no body: length refusal must not await EOF.
            std::future::pending::<()>().await;
        });
        let response = reqwest::Client::new().get(url).send().await.unwrap();
        let error = tokio::time::timeout(Duration::from_secs(1), bounded_bytes(response, 512))
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("exceeds byte limit"));
        server.abort();
    }

    #[tokio::test]
    async fn truncated_body_fails_without_exposing_contents() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nPRIVATE")
                .await
                .unwrap();
        });
        let response = reqwest::Client::new().get(url).send().await.unwrap();
        let error = bounded_bytes(response, 512).await.unwrap_err().to_string();
        assert!(error.contains("reading bounded model response failed"));
        assert!(!error.contains("PRIVATE"));
        server.await.unwrap();
    }

    fn model(kind: usize, url: String, limit: Option<usize>) -> Arc<dyn ModelProvider> {
        let timeout = Duration::from_secs(3);
        match kind {
            0 => Arc::new(
                OpenAiCompatibleModel::new(
                    url,
                    "test",
                    None,
                    ModelCapabilities::default(),
                    timeout,
                    EgressPolicy::LocalOnly,
                )
                .unwrap()
                .with_response_max_bytes(limit),
            ),
            1 => Arc::new(
                AnthropicModel::new(
                    Some(url),
                    "test",
                    ResolvedCredential::api_key(
                        "local-test",
                        CredentialSource::EnvVar("TEST".into()),
                    ),
                    ModelCapabilities::default(),
                    None,
                    timeout,
                    EgressPolicy::LocalOnly,
                )
                .unwrap()
                .with_response_max_bytes(limit),
            ),
            2 => Arc::new(
                CodexModel::new(
                    Some(url),
                    "test",
                    "local-test",
                    "local-account",
                    timeout,
                    EgressPolicy::LocalOnly,
                )
                .unwrap()
                .with_response_max_bytes(limit),
            ),
            _ => unreachable!(),
        }
    }

    fn request() -> CompletionRequest {
        CompletionRequest::new("test", vec![Message::user("local test")])
    }

    fn success_body(kind: usize) -> &'static str {
        match kind {
            0 => r#"{"choices":[{"message":{"content":"ok"}}]}"#,
            1 => r#"{"content":[{"type":"text","text":"ok"}]}"#,
            2 => {
                "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}]}}\n\n"
            }
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn every_complete_adapter_bounds_success_and_error_envelopes() {
        for kind in 0..3 {
            for framing in [
                Framing::Length,
                Framing::Chunked,
                Framing::Close,
                Framing::MisleadingLength,
            ] {
                let body = success_body(kind);
                let (url, server) = endpoint(200, body.as_bytes(), framing).await;
                let response = model(kind, url, Some(body.len()))
                    .complete(request())
                    .await
                    .unwrap();
                assert_eq!(response.content, "ok");
                server.await.unwrap();
                for status in [200, 401, 500] {
                    // Whitespace padding is valid JSON / SSE. A tiny final
                    // answer must not permit an oversized wire envelope.
                    let large = format!("{}{body}", " ".repeat(300_000));
                    let (url, server) = endpoint(status, large.as_bytes(), framing).await;
                    let error = model(kind, url, Some(256 * 1024))
                        .complete(request())
                        .await
                        .unwrap_err()
                        .to_string();
                    assert!(
                        error.contains("exceeds byte limit"),
                        "kind={kind}, framing={framing:?}, status={status}: {error}"
                    );
                    server.await.unwrap();
                }
            }
        }
    }

    #[tokio::test]
    async fn ordinary_complete_remains_unlimited() {
        for kind in 0..3 {
            let body = format!("{}{}", " ".repeat(300_000), success_body(kind));
            // SSE's first data line cannot have leading whitespace.
            let body = if kind == 2 {
                body.replacen("data:", "\ndata:", 1)
            } else {
                body
            };
            let (url, server) = endpoint(200, body.as_bytes(), Framing::Length).await;
            assert_eq!(
                model(kind, url, None)
                    .complete(request())
                    .await
                    .unwrap()
                    .content,
                "ok"
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn bounded_streaming_cannot_enter_unbounded_fallback_paths() {
        for kind in 0..2 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let error = model(kind, url, Some(512))
                .stream_complete(request(), &mut |_| panic!("no delta expected"))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("require complete"));
            assert!(
                tokio::time::timeout(Duration::from_millis(20), listener.accept())
                    .await
                    .is_err()
            );
        }
        // Codex inherits the trait's stream_complete -> complete fallback;
        // its complete reader remains bounded, even though the wire is SSE.
        let (url, server) = endpoint(200, &vec![b'x'; 1024], Framing::Chunked).await;
        let error = model(2, url, Some(512))
            .stream_complete(request(), &mut |_| panic!("no delta expected"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeds byte limit"));
        server.await.unwrap();
    }
}
