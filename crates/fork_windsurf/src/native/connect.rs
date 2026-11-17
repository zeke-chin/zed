//! Connect protocol over `http_client`: unary calls (`application/proto`) and
//! server streams (`application/connect+proto`, gzip-compressed frames). The
//! headers mirror what the official language server (connect-go) sends.

use std::{fmt, sync::Arc, time::Duration};

use async_compression::futures::bufread::{GzipDecoder, GzipEncoder};
use futures::{AsyncReadExt as _, FutureExt as _, future::BoxFuture};
use http_client::{AsyncBody, HttpClient, Method, Request, StatusCode};
use serde::Deserialize;

pub const USER_AGENT: &str = "connect-go/1.18.1 (go1.26.6)";

const FLAG_COMPRESSED: u8 = 0b01;
const FLAG_END_STREAM: u8 = 0b10;
const FRAME_HEADER_LEN: usize = 5;
const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;
const MAX_ERROR_BODY_LEN: usize = 64 * 1024;

/// Returns a future that resolves after the given duration. Injected so that
/// tests can drive time with the fake executor and the live example can run
/// without a gpui app.
pub type Timer = Arc<dyn Fn(Duration) -> BoxFuture<'static, ()> + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectError {
    /// The key or token was rejected. Not retried.
    Unauthenticated(String),
    /// The server asked us to slow down. Retried with backoff.
    RateLimited(String),
    /// Connection failures, timeouts and streams cut off midway. Retried with backoff.
    Network(String),
    /// A response we couldn't decode.
    Protocol(String),
    /// Any other error code returned by the server.
    Server { code: String, message: String },
}

impl ConnectError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::RateLimited(_) | Self::Network(_))
    }

    fn from_code(code: &str, message: String) -> Self {
        match code {
            "unauthenticated" | "permission_denied" => Self::Unauthenticated(message),
            "resource_exhausted" => Self::RateLimited(message),
            "unavailable" | "deadline_exceeded" => Self::Network(message),
            _ => Self::Server {
                code: code.to_string(),
                message,
            },
        }
    }

    pub(crate) fn from_http_status(status: StatusCode, body: &[u8]) -> Self {
        if let Ok(error) = serde_json::from_slice::<WireError>(body)
            && let Some(code) = error.code.as_deref()
        {
            return Self::from_code(code, error.message.unwrap_or_default());
        }
        let message = format!("HTTP {}", status.as_u16());
        match status.as_u16() {
            401 | 403 => Self::Unauthenticated(message),
            429 => Self::RateLimited(message),
            408 | 502 | 503 | 504 => Self::Network(message),
            _ => Self::Server {
                code: format!("http_{}", status.as_u16()),
                message,
            },
        }
    }
}

impl fmt::Display for ConnectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthenticated(message) => write!(formatter, "authentication failed: {message}"),
            Self::RateLimited(message) => write!(formatter, "rate limited: {message}"),
            Self::Network(message) => write!(formatter, "network error: {message}"),
            Self::Protocol(message) => write!(formatter, "protocol error: {message}"),
            Self::Server { code, message } => write!(formatter, "server error {code}: {message}"),
        }
    }
}

impl std::error::Error for ConnectError {}

#[derive(Deserialize)]
struct WireError {
    code: Option<String>,
    message: Option<String>,
}

#[derive(Deserialize)]
struct EndStream {
    error: Option<WireError>,
}

pub struct ConnectClient {
    http: Arc<dyn HttpClient>,
    timer: Timer,
}

impl ConnectClient {
    pub fn new(http: Arc<dyn HttpClient>, timer: Timer) -> Self {
        Self { http, timer }
    }

    pub async fn unary<Req, Resp>(
        &self,
        url: &str,
        request: &Req,
        timeout: Duration,
    ) -> Result<Resp, ConnectError>
    where
        Req: prost::Message,
        Resp: prost::Message + Default,
    {
        let http_request = Request::builder()
            .method(Method::POST)
            .uri(url)
            .header("User-Agent", USER_AGENT)
            .header("Content-Type", "application/proto")
            .header("Accept-Encoding", "gzip")
            .header("Connect-Protocol-Version", "1")
            .header("Connect-Timeout-Ms", timeout.as_millis().to_string())
            .body(AsyncBody::from(request.encode_to_vec()))
            .map_err(|error| ConnectError::Protocol(format!("invalid request: {error}")))?;
        let http = self.http.clone();
        let call = async move {
            let mut response = http
                .send(http_request)
                .await
                .map_err(|error| ConnectError::Network(format!("{error:#}")))?;
            let status = response.status();
            let gzipped = response
                .headers()
                .get("content-encoding")
                .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"gzip"));
            let mut body = Vec::new();
            response
                .body_mut()
                .read_to_end(&mut body)
                .await
                .map_err(|error| ConnectError::Network(format!("reading response: {error}")))?;
            let mut body = if gzipped { gunzip(&body).await? } else { body };
            if !status.is_success() {
                body.truncate(MAX_ERROR_BODY_LEN);
                return Err(ConnectError::from_http_status(status, &body));
            }
            Resp::decode(body.as_slice())
                .map_err(|error| ConnectError::Protocol(format!("decoding response: {error}")))
        };
        self.with_deadline(timeout, call).await
    }

    /// Runs a server-streaming call, handing each message to `on_message` as
    /// it arrives. Returns once the server sends its end-of-stream frame.
    pub async fn server_stream<Req, Resp>(
        &self,
        url: &str,
        request: &Req,
        timeout: Duration,
        mut on_message: impl FnMut(Resp) + Send,
    ) -> Result<(), ConnectError>
    where
        Req: prost::Message,
        Resp: prost::Message + Default,
    {
        let body = encode_frame(&gzip(&request.encode_to_vec()).await?, true, false);
        let http_request = Request::builder()
            .method(Method::POST)
            .uri(url)
            .header("User-Agent", USER_AGENT)
            .header("Content-Type", "application/connect+proto")
            .header("Accept-Encoding", "identity")
            .header("Connect-Protocol-Version", "1")
            .header("Connect-Timeout-Ms", timeout.as_millis().to_string())
            .header("Connect-Content-Encoding", "gzip")
            .header("Connect-Accept-Encoding", "gzip")
            .body(AsyncBody::from(body))
            .map_err(|error| ConnectError::Protocol(format!("invalid request: {error}")))?;
        let http = self.http.clone();
        let call = async move {
            let mut response = http
                .send(http_request)
                .await
                .map_err(|error| ConnectError::Network(format!("{error:#}")))?;
            let status = response.status();
            if !status.is_success() {
                let mut body = Vec::new();
                response
                    .body_mut()
                    .take(MAX_ERROR_BODY_LEN as u64)
                    .read_to_end(&mut body)
                    .await
                    .map_err(|error| {
                        ConnectError::Network(format!("reading error response: {error}"))
                    })?;
                return Err(ConnectError::from_http_status(status, &body));
            }

            let mut decoder = FrameDecoder::default();
            let mut buffer = vec![0; 16 * 1024];
            loop {
                while let Some(frame) = decoder.next_frame()? {
                    let payload = if frame.flags & FLAG_COMPRESSED != 0 {
                        gunzip(&frame.payload).await?
                    } else {
                        frame.payload
                    };
                    if frame.flags & FLAG_END_STREAM != 0 {
                        return end_stream_result(&payload);
                    }
                    let message = Resp::decode(payload.as_slice()).map_err(|error| {
                        ConnectError::Protocol(format!("decoding message: {error}"))
                    })?;
                    on_message(message);
                }
                let read = response
                    .body_mut()
                    .read(&mut buffer)
                    .await
                    .map_err(|error| {
                        ConnectError::Network(format!("stream interrupted: {error}"))
                    })?;
                if read == 0 {
                    return Err(ConnectError::Network(
                        "stream ended without an end-of-stream frame".to_string(),
                    ));
                }
                decoder.push(&buffer[..read]);
            }
        };
        self.with_deadline(timeout, call).await
    }

    async fn with_deadline<T>(
        &self,
        timeout: Duration,
        call: impl Future<Output = Result<T, ConnectError>>,
    ) -> Result<T, ConnectError> {
        let deadline = (self.timer)(timeout).fuse();
        let call = call.fuse();
        futures::pin_mut!(deadline, call);
        futures::select_biased! {
            result = call => result,
            _ = deadline => Err(ConnectError::Network(format!(
                "timed out after {} ms",
                timeout.as_millis()
            ))),
        }
    }
}

fn end_stream_result(payload: &[u8]) -> Result<(), ConnectError> {
    if payload.is_empty() {
        return Ok(());
    }
    let end: EndStream = serde_json::from_slice(payload)
        .map_err(|error| ConnectError::Protocol(format!("decoding end-of-stream: {error}")))?;
    match end.error {
        Some(error) => Err(ConnectError::from_code(
            error.code.as_deref().unwrap_or("unknown"),
            error.message.unwrap_or_default(),
        )),
        None => Ok(()),
    }
}

pub fn encode_frame(payload: &[u8], compressed: bool, end_stream: bool) -> Vec<u8> {
    let mut flags = 0;
    if compressed {
        flags |= FLAG_COMPRESSED;
    }
    if end_stream {
        flags |= FLAG_END_STREAM;
    }
    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    frame.push(flags);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

#[derive(Debug, PartialEq, Eq)]
pub struct Frame {
    pub flags: u8,
    pub payload: Vec<u8>,
}

/// Splits a byte stream into Connect envelopes (1 flag byte, 4-byte big-endian
/// length, payload), regardless of how the bytes are chunked.
#[derive(Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    pub fn next_frame(&mut self) -> Result<Option<Frame>, ConnectError> {
        let Some(header) = self.buffer.get(..FRAME_HEADER_LEN) else {
            return Ok(None);
        };
        let flags = header[0];
        let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        if length > MAX_FRAME_LEN {
            return Err(ConnectError::Protocol(format!(
                "frame of {length} bytes exceeds the limit"
            )));
        }
        if self.buffer.len() < FRAME_HEADER_LEN + length {
            return Ok(None);
        }
        let payload = self.buffer[FRAME_HEADER_LEN..FRAME_HEADER_LEN + length].to_vec();
        self.buffer.drain(..FRAME_HEADER_LEN + length);
        Ok(Some(Frame { flags, payload }))
    }
}

pub async fn gzip(bytes: &[u8]) -> Result<Vec<u8>, ConnectError> {
    let mut output = Vec::new();
    GzipEncoder::new(bytes)
        .read_to_end(&mut output)
        .await
        .map_err(|error| ConnectError::Protocol(format!("compressing: {error}")))?;
    Ok(output)
}

pub async fn gunzip(bytes: &[u8]) -> Result<Vec<u8>, ConnectError> {
    let mut output = Vec::new();
    GzipDecoder::new(bytes)
        .read_to_end(&mut output)
        .await
        .map_err(|error| ConnectError::Protocol(format!("decompressing: {error}")))?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use futures::{AsyncRead, executor::block_on};
    use http_client::{FakeHttpClient, Response};
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::proto::exa::{
        api_server_pb::{DeltaTab, GetTabRequest, GetTabResponse, delta_tab, get_tab_response},
        auth_pb::{GetUserJwtRequest, GetUserJwtResponse},
    };

    fn never_timer() -> Timer {
        Arc::new(|_| futures::future::pending().boxed())
    }

    fn instant_timer() -> Timer {
        Arc::new(|_| futures::future::ready(()).boxed())
    }

    fn delta(new_text: &str) -> GetTabResponse {
        GetTabResponse {
            message: Some(get_tab_response::Message::StreamingCompletionV2(DeltaTab {
                path: "/a.rs".to_string(),
                delta: Some(delta_tab::Delta::DeltaSupercomplete(
                    delta_tab::DeltaSupercomplete {
                        delta_new_str: new_text.to_string(),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            })),
        }
    }

    fn message_frame(message: &GetTabResponse, compressed: bool) -> Vec<u8> {
        let bytes = prost::Message::encode_to_vec(message);
        if compressed {
            encode_frame(&block_on(gzip(&bytes)).expect("gzip"), true, false)
        } else {
            encode_frame(&bytes, false, false)
        }
    }

    /// Hands out the body in fixed-size chunks to exercise frames that span reads.
    struct ChunkedReader {
        bytes: Vec<u8>,
        position: usize,
        chunk: usize,
        fail_at_end: bool,
    }

    impl AsyncRead for ChunkedReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buffer: &mut [u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            if self.position >= self.bytes.len() {
                if self.fail_at_end {
                    return std::task::Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "reset",
                    )));
                }
                return std::task::Poll::Ready(Ok(0));
            }
            let end = (self.position + self.chunk)
                .min(self.bytes.len())
                .min(self.position + buffer.len());
            let count = end - self.position;
            buffer[..count].copy_from_slice(&self.bytes[self.position..end]);
            self.position = end;
            std::task::Poll::Ready(Ok(count))
        }
    }

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn streaming_client(body: Vec<u8>, chunk: usize, fail_at_end: bool) -> ConnectClient {
        let http = FakeHttpClient::create(move |request| {
            let body = body.clone();
            async move {
                assert_eq!(
                    request.headers()["content-type"],
                    "application/connect+proto"
                );
                assert_eq!(request.headers()["connect-content-encoding"], "gzip");
                assert_eq!(request.headers()["user-agent"], USER_AGENT);
                let mut request_body = Vec::new();
                let mut incoming = request.into_body();
                incoming.read_to_end(&mut request_body).await?;
                let mut decoder = FrameDecoder::default();
                decoder.push(&request_body);
                let frame = decoder.next_frame()?.expect("request frame");
                assert_eq!(frame.flags, FLAG_COMPRESSED);
                let request = <GetTabRequest as prost::Message>::decode(
                    gunzip(&frame.payload).await?.as_slice(),
                )?;
                assert_eq!(request.prompt_id, "prompt");
                Ok(Response::builder()
                    .status(200)
                    .body(AsyncBody::from_reader(ChunkedReader {
                        bytes: body,
                        position: 0,
                        chunk,
                        fail_at_end,
                    }))?)
            }
        });
        ConnectClient::new(http, never_timer())
    }

    fn run_stream(client: &ConnectClient) -> (Vec<String>, Result<(), ConnectError>) {
        let request = GetTabRequest {
            prompt_id: "prompt".to_string(),
            ..Default::default()
        };
        let mut received = Vec::new();
        let result = block_on(client.server_stream(
            "https://example.test/GetTab",
            &request,
            Duration::from_secs(5),
            |message: GetTabResponse| {
                if let Some(get_tab_response::Message::StreamingCompletionV2(delta)) =
                    message.message
                    && let Some(delta_tab::Delta::DeltaSupercomplete(supercomplete)) = delta.delta
                {
                    received.push(supercomplete.delta_new_str);
                }
            },
        ));
        (received, result)
    }

    #[test]
    fn test_frame_decoder_handles_split_and_joined_frames() {
        let mut bytes = encode_frame(b"first", false, false);
        bytes.extend(encode_frame(b"", true, false));
        bytes.extend(encode_frame(b"{}", false, true));
        let mut decoder = FrameDecoder::default();
        let mut frames = Vec::new();
        for byte in bytes {
            decoder.push(&[byte]);
            while let Some(frame) = decoder.next_frame().expect("valid frames") {
                frames.push(frame);
            }
        }
        assert_eq!(
            frames,
            vec![
                Frame {
                    flags: 0,
                    payload: b"first".to_vec()
                },
                Frame {
                    flags: FLAG_COMPRESSED,
                    payload: Vec::new()
                },
                Frame {
                    flags: FLAG_END_STREAM,
                    payload: b"{}".to_vec()
                },
            ]
        );
    }

    #[test]
    fn test_frame_decoder_rejects_oversized_frames() {
        let mut decoder = FrameDecoder::default();
        decoder.push(&[0, 0xff, 0xff, 0xff, 0xff]);
        assert!(matches!(
            decoder.next_frame(),
            Err(ConnectError::Protocol(_))
        ));
    }

    #[test]
    fn test_stream_collects_compressed_and_plain_frames_across_chunks() {
        let mut body = message_frame(&delta("fn "), true);
        body.extend(message_frame(&delta("main"), false));
        body.extend(encode_frame(b"{}", false, true));
        for chunk in [1, 3, 7, 4096] {
            let (received, result) = run_stream(&streaming_client(body.clone(), chunk, false));
            assert_eq!(result, Ok(()));
            assert_eq!(received, vec!["fn ".to_string(), "main".to_string()]);
        }
    }

    #[test]
    fn test_stream_end_frame_error_is_classified() {
        let cases = [
            (
                r#"{"error":{"code":"unauthenticated","message":"bad key"}}"#,
                ConnectError::Unauthenticated("bad key".to_string()),
            ),
            (
                r#"{"error":{"code":"resource_exhausted","message":"slow down"}}"#,
                ConnectError::RateLimited("slow down".to_string()),
            ),
            (
                r#"{"error":{"code":"internal","message":"boom"}}"#,
                ConnectError::Server {
                    code: "internal".to_string(),
                    message: "boom".to_string(),
                },
            ),
        ];
        for (end, expected) in cases {
            let mut body = message_frame(&delta("partial"), true);
            body.extend(encode_frame(
                &block_on(gzip(end.as_bytes())).expect("gzip"),
                true,
                true,
            ));
            let (received, result) = run_stream(&streaming_client(body, 5, false));
            assert_eq!(received, vec!["partial".to_string()]);
            assert_eq!(result, Err(expected));
        }
    }

    #[test]
    fn test_stream_cut_off_midway_is_a_network_error() {
        let mut body = message_frame(&delta("one"), true);
        let second = message_frame(&delta("two"), true);
        body.extend(&second[..second.len() / 2]);
        let (received, result) = run_stream(&streaming_client(body.clone(), 4, false));
        assert_eq!(received, vec!["one".to_string()]);
        assert!(
            matches!(result, Err(ConnectError::Network(_))),
            "{result:?}"
        );

        let (_, result) = run_stream(&streaming_client(body, 4, true));
        assert!(
            matches!(result, Err(ConnectError::Network(_))),
            "{result:?}"
        );
    }

    #[test]
    fn test_stream_garbage_is_a_protocol_error() {
        let mut body = encode_frame(b"\xff\xff\xff", false, false);
        body.extend(encode_frame(b"{}", false, true));
        let (_, result) = run_stream(&streaming_client(body, 64, false));
        assert!(
            matches!(result, Err(ConnectError::Protocol(_))),
            "{result:?}"
        );
    }

    #[test]
    fn test_http_status_errors_are_classified() {
        let cases = [
            (429, "", ConnectError::RateLimited("HTTP 429".to_string())),
            (
                401,
                r#"{"code":"unauthenticated","message":"expired"}"#,
                ConnectError::Unauthenticated("expired".to_string()),
            ),
            (503, "", ConnectError::Network("HTTP 503".to_string())),
        ];
        for (status, body, expected) in cases {
            let http = FakeHttpClient::create(move |_| async move {
                Ok(Response::builder()
                    .status(status)
                    .body(AsyncBody::from(body.to_string()))?)
            });
            let client = ConnectClient::new(http, never_timer());
            let (_, result) = run_stream(&client);
            assert_eq!(result, Err(expected.clone()));
            let unary: Result<GetUserJwtResponse, _> = block_on(client.unary(
                "https://example.test/GetUserJwt",
                &GetUserJwtRequest::default(),
                Duration::from_secs(5),
            ));
            assert_eq!(unary, Err(expected));
        }
    }

    #[test]
    fn test_transport_failure_and_timeout_are_network_errors() {
        let http = FakeHttpClient::create(|_| async move { anyhow::bail!("connection refused") });
        let (_, result) = run_stream(&ConnectClient::new(http, never_timer()));
        assert!(
            matches!(result, Err(ConnectError::Network(_))),
            "{result:?}"
        );

        let dropped = Arc::new(AtomicBool::new(false));
        let http = FakeHttpClient::create({
            let dropped = dropped.clone();
            move |_| {
                let guard = DropFlag(dropped.clone());
                async move {
                    let _guard = guard;
                    futures::future::pending::<()>().await;
                    anyhow::bail!("unreachable")
                }
            }
        });
        let (_, result) = run_stream(&ConnectClient::new(http, instant_timer()));
        assert!(
            matches!(&result, Err(ConnectError::Network(message)) if message.contains("timed out")),
            "{result:?}"
        );
        assert!(
            dropped.load(Ordering::SeqCst),
            "the request should be dropped"
        );
    }

    #[test]
    fn test_unary_decodes_plain_and_gzip_bodies() {
        let response = GetUserJwtResponse {
            user_jwt: "token".to_string(),
            ..Default::default()
        };
        for gzipped in [false, true] {
            let encoded = prost::Message::encode_to_vec(&response);
            let http = FakeHttpClient::create(move |request| {
                let encoded = encoded.clone();
                async move {
                    assert_eq!(request.headers()["content-type"], "application/proto");
                    assert_eq!(request.headers()["accept-encoding"], "gzip");
                    let mut builder = Response::builder().status(200);
                    let body = if gzipped {
                        builder = builder.header("Content-Encoding", "gzip");
                        gzip(&encoded).await?
                    } else {
                        encoded
                    };
                    Ok(builder.body(AsyncBody::from(body))?)
                }
            });
            let client = ConnectClient::new(http, never_timer());
            let result: GetUserJwtResponse = block_on(client.unary(
                "https://example.test/GetUserJwt",
                &GetUserJwtRequest::default(),
                Duration::from_secs(5),
            ))
            .expect("unary call should succeed");
            assert_eq!(result, response);
        }
    }
}
