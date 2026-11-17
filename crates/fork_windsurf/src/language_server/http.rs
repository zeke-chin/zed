//! Unary Connect calls (`application/proto`) to the language server's local
//! port, over a minimal HTTP/1.1 client. Zed's HTTP client isn't used because
//! it routes every request through the proxy from the user's settings, which
//! can't reach `127.0.0.1`. One connection per call (`Connection: close`).

use futures::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::native::connect::ConnectError;

pub const SERVICE_PATH: &str = "/exa.language_server_pb.LanguageServerService/";
pub const CSRF_HEADER: &str = "x-codeium-csrf-token";

const MAX_RESPONSE_LEN: usize = 64 * 1024 * 1024;
const MAX_ERROR_BODY_LEN: usize = 64 * 1024;

pub fn request_bytes(port: u16, csrf_token: &str, method: &str, body: &[u8]) -> Vec<u8> {
    let mut request = format!(
        "POST {SERVICE_PATH}{method} HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Content-Type: application/proto\r\n\
         Connect-Protocol-Version: 1\r\n\
         {CSRF_HEADER}: {csrf_token}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(body);
    request
}

/// Sends `body` to `method` and returns the response body. Has no deadline of
/// its own; callers race it against a timer.
pub async fn unary(
    port: u16,
    csrf_token: &str,
    method: &str,
    body: &[u8],
) -> Result<Vec<u8>, ConnectError> {
    let mut stream = smol::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|error| {
            ConnectError::Network(format!("couldn't reach the language server: {error}"))
        })?;
    stream
        .write_all(&request_bytes(port, csrf_token, method, body))
        .await
        .map_err(|error| {
            ConnectError::Network(format!("sending to the language server: {error}"))
        })?;
    let mut response = Vec::new();
    (&mut stream)
        .take(MAX_RESPONSE_LEN as u64 + 1)
        .read_to_end(&mut response)
        .await
        .map_err(|error| {
            ConnectError::Network(format!("reading from the language server: {error}"))
        })?;
    if response.len() > MAX_RESPONSE_LEN {
        return Err(ConnectError::Protocol(
            "the language server's response is too large".to_string(),
        ));
    }
    let (status, mut body) = parse_response(&response).map_err(ConnectError::Protocol)?;
    if status != 200 {
        body.truncate(MAX_ERROR_BODY_LEN);
        return Err(ConnectError::from_http_status(
            http_client::StatusCode::from_u16(status)
                .unwrap_or(http_client::StatusCode::INTERNAL_SERVER_ERROR),
            &body,
        ));
    }
    Ok(body)
}

/// Splits a complete HTTP/1.1 response into its status and (de-chunked) body.
pub fn parse_response(response: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or("the response has no complete header")?;
    let head = std::str::from_utf8(&response[..header_end])
        .map_err(|_| "the response header isn't UTF-8".to_string())?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| format!("unexpected status line {status_line:?}"))?;
    let mut chunked = false;
    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.eq_ignore_ascii_case("chunked");
        } else if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse::<usize>().ok();
        }
    }
    let body = &response[header_end + 4..];
    let body = if chunked {
        dechunk(body)?
    } else if let Some(length) = content_length {
        body.get(..length)
            .ok_or("the response body is shorter than its Content-Length")?
            .to_vec()
    } else {
        body.to_vec()
    };
    Ok((status, body))
}

fn dechunk(mut body: &[u8]) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    loop {
        let line_end = body
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or("a chunk header is incomplete")?;
        let size_text = std::str::from_utf8(&body[..line_end])
            .map_err(|_| "a chunk header isn't UTF-8".to_string())?;
        let size_text = size_text.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| format!("invalid chunk size {size_text:?}"))?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Ok(output);
        }
        let chunk = body.get(..size).ok_or("a chunk is incomplete")?;
        output.extend_from_slice(chunk);
        body = body
            .get(size + 2..)
            .ok_or("a chunk is missing its line break")?;
    }
}
