//! The language server's LSP port (JSON-RPC over TCP). It's how the official
//! extension tells the server about open documents and edits
//! (`didOpen` / `didChange`), from which the server builds its edit history
//! (`fork/spike/结论.md` 问题 9). Only `initialize` is a request; everything
//! else is a notification. Requests from the server get an empty result, as
//! in `fork/spike/lib/ls_driver.py`.
//!
//! Reading and writing happen on two threads so that sending a notification
//! never blocks the caller, which is usually the main thread.

use std::{
    collections::HashMap,
    io::{BufReader, Write as _},
    net::{Shutdown, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
};

use futures::channel::oneshot;
use parking_lot::Mutex;
use serde_json::{Value, json};

const MAX_MESSAGE_LEN: usize = 64 * 1024 * 1024;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

pub struct LspConnection {
    outgoing: mpsc::Sender<Vec<u8>>,
    pending: Pending,
    next_id: AtomicU64,
    closed: Arc<AtomicBool>,
    stream: TcpStream,
}

impl LspConnection {
    pub fn connect(port: u16) -> std::io::Result<Self> {
        let stream = TcpStream::connect(("127.0.0.1", port))?;
        stream.set_nodelay(true)?;
        let reader = stream.try_clone()?;
        let mut writer = stream.try_clone()?;
        let (outgoing, receiver) = mpsc::channel::<Vec<u8>>();
        let pending: Pending = Arc::default();
        let closed = Arc::new(AtomicBool::new(false));

        std::thread::Builder::new()
            .name("windsurf-lsp-writer".to_string())
            .spawn({
                let closed = closed.clone();
                move || {
                    for message in receiver {
                        if let Err(error) = writer.write_all(&message) {
                            log::debug!("windsurf: LSP connection closed while writing: {error}");
                            closed.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                }
            })?;
        std::thread::Builder::new()
            .name("windsurf-lsp-reader".to_string())
            .spawn({
                let outgoing = outgoing.clone();
                let pending = pending.clone();
                let closed = closed.clone();
                move || {
                    let mut reader = BufReader::new(reader);
                    loop {
                        match read_message(&mut reader) {
                            Ok(Some(message)) => handle_incoming(message, &outgoing, &pending),
                            Ok(None) => break,
                            Err(error) => {
                                log::debug!("windsurf: LSP connection ended: {error}");
                                break;
                            }
                        }
                    }
                    closed.store(true, Ordering::SeqCst);
                    for (_, sender) in pending.lock().drain() {
                        sender
                            .send(Err("the LSP connection closed".to_string()))
                            .ok();
                    }
                }
            })?;

        Ok(Self {
            outgoing,
            pending,
            next_id: AtomicU64::new(0),
            closed,
            stream,
        })
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Sends a request; the receiver gets its `result`, or the error message.
    pub fn request(&self, method: &str, params: Value) -> oneshot::Receiver<Result<Value, String>> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().insert(id, sender);
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        receiver
    }

    fn send(&self, message: &Value) {
        if self.outgoing.send(frame(message)).is_err() {
            self.closed.store(true, Ordering::SeqCst);
        }
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.stream.shutdown(Shutdown::Both).ok();
    }
}

impl Drop for LspConnection {
    fn drop(&mut self) {
        self.close();
    }
}

pub fn frame(message: &Value) -> Vec<u8> {
    let body = message.to_string();
    let mut framed = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    framed.extend_from_slice(body.as_bytes());
    framed
}

/// Reads one framed message; `None` at a clean end of stream.
pub fn read_message(reader: &mut impl std::io::BufRead) -> std::io::Result<Option<Value>> {
    let mut content_length = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    let length = content_length.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "missing Content-Length")
    })?;
    if length > MAX_MESSAGE_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "LSP message too large",
        ));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn handle_incoming(message: Value, outgoing: &mpsc::Sender<Vec<u8>>, pending: &Pending) {
    let id = message.get("id").cloned();
    match (message.get("method").and_then(Value::as_str), id) {
        (Some(method), Some(id)) => {
            let result = if method == "workspace/configuration" {
                let count = message
                    .pointer("/params/items")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or_default();
                Value::Array(vec![Value::Null; count])
            } else {
                Value::Null
            };
            outgoing
                .send(frame(
                    &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                ))
                .ok();
        }
        (None, Some(id)) => {
            let Some(id) = id.as_u64() else {
                return;
            };
            let Some(sender) = pending.lock().remove(&id) else {
                return;
            };
            let result = match message.get("error") {
                Some(error) => Err(error.to_string()),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            sender.send(result).ok();
        }
        _ => {}
    }
}
