//! Tests of the language server bridge: the HTTP and LSP transports against
//! fake TCP servers, the backend against an in-memory server (start, sync,
//! timeouts, errors, restarts), the delegate end to end with the backend
//! selected, and an ignored test against the real binary.

use std::{
    collections::VecDeque,
    io::{BufReader, Read as _, Write as _},
    net::TcpListener,
    sync::Arc,
    time::Duration,
};

use edit_prediction_types::{
    EditPrediction, EditPredictionDelegate as _, EditPredictionDiscardReason,
    EditPredictionRequestTrigger, SuggestionDisplayType,
};
use fs::FakeFs;
use futures::{FutureExt as _, future::BoxFuture};
use gpui::{AppContext as _, Entity, TestAppContext, UpdateGlobal as _};
use language::{Buffer, Point, ToOffset as _};
use parking_lot::Mutex;
use pretty_assertions::assert_eq;
use project::Project;
use prost::Message as _;
use serde_json::{Value, json};
use settings::SettingsStore;
use util::path;

use super::*;
use crate::{
    Status, Windsurf, WindsurfDelegate,
    backend::{DocumentContext, LanguageServerContext, TabRequestInput},
    native::connect::ConnectError,
    proto::exa::{
        codeium_common_pb::{DocumentPosition, Language, SupercompleteTriggerCondition},
        diff_action_pb::{CharacterDiff, CharacterDiffChange, DiffChangeType},
        language_server_pb::{
            HandleStreamingTabV2Request, HandleStreamingTabV2Response,
            ProvideCompletionFeedbackRequest, RefreshContextForIdeActionRequest,
            handle_streaming_tab_v2_response::{Diff, NoOp, Suggestion, TabJump},
        },
    },
};

const API_KEY: &str = "devin-session-token$test-api-key";

/// A timer for tests that run without gpui (on `smol::block_on`).
#[cfg(unix)]
fn thread_timer() -> crate::native::connect::Timer {
    Arc::new(|duration| {
        let (sender, receiver) = futures::channel::oneshot::channel::<()>();
        std::thread::spawn(move || {
            std::thread::sleep(duration);
            sender.send(()).ok();
        });
        receiver.map(|_| ()).boxed()
    })
}

#[cfg(unix)]
fn run_command(program: &str, arguments: &[&str]) -> Option<std::process::Output> {
    smol::block_on(
        smol::process::Command::new(program)
            .args(arguments)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output(),
    )
    .ok()
}

// ---------------------------------------------------------------------------
// Transports against fake TCP servers.

/// Serves one connection: records the raw request, answers with `response`.
fn serve_once(response: Vec<u8>) -> (u16, std::thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("address").port();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let read = stream.read(&mut buffer).expect("read");
            request.extend_from_slice(&buffer[..read]);
            if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&request[..header_end]).to_string();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .and_then(|length| length.trim().parse::<usize>().ok())
                    .unwrap_or_default();
                if request.len() >= header_end + 4 + length {
                    break;
                }
            }
            if read == 0 {
                break;
            }
        }
        stream.write_all(&response).expect("write");
        request
    });
    (port, handle)
}

#[test]
fn test_unary_call_sends_connect_headers_and_reads_the_body() {
    let (port, server) = serve_once(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/proto\r\nContent-Length: 3\r\n\r\nabc"
            .to_vec(),
    );
    let body = smol::block_on(http::unary(port, "csrf-token", HEARTBEAT, b"hello"))
        .expect("the call should succeed");
    assert_eq!(body, b"abc");
    let request = String::from_utf8(server.join().expect("server thread")).expect("utf-8");
    assert!(
        request.starts_with(
            "POST /exa.language_server_pb.LanguageServerService/Heartbeat HTTP/1.1\r\n"
        )
    );
    assert!(request.contains("\r\nContent-Type: application/proto\r\n"));
    assert!(request.contains("\r\nx-codeium-csrf-token: csrf-token\r\n"));
    assert!(request.contains("\r\nConnect-Protocol-Version: 1\r\n"));
    assert!(request.ends_with("\r\n\r\nhello"));
}

#[test]
fn test_unary_call_decodes_chunked_bodies_and_errors() {
    let (port, server) = serve_once(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n"
            .to_vec(),
    );
    let body = smol::block_on(http::unary(port, "csrf", HEARTBEAT, b"")).expect("chunked body");
    assert_eq!(body, b"abcde");
    server.join().expect("server thread");

    let error_body = r#"{"code":"unauthenticated","message":"bad key"}"#;
    let (port, server) = serve_once(
        format!(
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{error_body}",
            error_body.len()
        )
        .into_bytes(),
    );
    let error = smol::block_on(http::unary(port, "csrf", HEARTBEAT, b"")).expect_err("401");
    assert_eq!(error, ConnectError::Unauthenticated("bad key".to_string()));
    server.join().expect("server thread");

    let unused = TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("address")
        .port();
    let error = smol::block_on(http::unary(unused, "csrf", HEARTBEAT, b"")).expect_err("refused");
    assert!(matches!(error, ConnectError::Network(_)), "{error:?}");
}

#[test]
fn test_lsp_connection_answers_server_requests() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("address").port();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut writer = stream.try_clone().expect("clone");
        let mut reader = BufReader::new(stream);
        let initialize = lsp::read_message(&mut reader)
            .expect("read")
            .expect("initialize");
        writer
            .write_all(&lsp::frame(&json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "workspace/configuration",
                "params": { "items": [{}, {}] },
            })))
            .expect("write");
        let configuration = lsp::read_message(&mut reader)
            .expect("read")
            .expect("configuration answer");
        writer
            .write_all(&lsp::frame(&json!({
                "jsonrpc": "2.0",
                "id": initialize["id"],
                "result": { "serverInfo": { "name": "codeium", "version": "build" } },
            })))
            .expect("write");
        let notification = lsp::read_message(&mut reader)
            .expect("read")
            .expect("notification");
        (initialize, configuration, notification)
    });

    let connection = lsp::LspConnection::connect(port).expect("connect");
    let result = smol::block_on(connection.request("initialize", json!({ "rootUri": null })))
        .expect("answered")
        .expect("result");
    assert_eq!(result["serverInfo"]["version"], "build");
    connection.notify(
        "textDocument/didOpen",
        json!({ "textDocument": { "uri": "file:///a" } }),
    );
    let (initialize, configuration, notification) = server.join().expect("server thread");
    assert_eq!(initialize["method"], "initialize");
    assert_eq!(
        configuration,
        json!({ "jsonrpc": "2.0", "id": 7, "result": [null, null] })
    );
    assert_eq!(notification["method"], "textDocument/didOpen");
    connection.close();
    assert!(connection.is_closed());
}

#[cfg(unix)]
#[test]
fn test_launch_reports_a_binary_that_exits_with_its_output() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir().expect("temporary directory");
    let binary = directory.path().join("language_server_macos_arm");
    std::fs::write(
        &binary,
        "#!/bin/sh\necho 'flag provided but not defined' >&2\nexit 2\n",
    )
    .expect("write");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let options = LaunchOptions {
        binary,
        product: ProductInfo::default(),
        data_dir: directory.path().join("data"),
        initial_metadata: Default::default(),
        workspace_uris: Vec::new(),
        proxy: None,
        windsurf_version: "3.10.48".to_string(),
        launch_timeout: Duration::from_secs(20),
        timer: thread_timer(),
        spawner: Arc::new(|future| smol::spawn(future).detach()),
    };
    let result = smol::block_on(process::ProcessLauncher.launch(options));
    let Err(error) = result else {
        panic!("a binary that exits can't start");
    };
    assert!(error.contains("exited during startup"), "{error}");
    assert!(error.contains("flag provided but not defined"), "{error}");
    assert!(directory.path().join("data/database").is_dir());
}

// ---------------------------------------------------------------------------
// The backend against an in-memory server.

enum FakeReply {
    Answer(HandleStreamingTabV2Response),
    Error(ConnectError),
    /// Never answers, like the real server when the cloud fails.
    Hang,
}

#[derive(Default)]
struct FakeServerRecord {
    alive: bool,
    terminated: bool,
    calls: Vec<(&'static str, Vec<u8>)>,
    notifications: Vec<(&'static str, Value)>,
}

#[derive(Default)]
struct FakeLanguageServerState {
    servers: Vec<FakeServerRecord>,
    replies: VecDeque<FakeReply>,
    failing_launches: usize,
    launch_options: Vec<(PathBuf, Vec<String>)>,
}

#[derive(Clone, Default)]
struct FakeLanguageServer(Arc<Mutex<FakeLanguageServerState>>);

impl FakeLanguageServer {
    fn reply(&self, reply: FakeReply) {
        self.0.lock().replies.push_back(reply);
    }

    fn launches(&self) -> usize {
        self.0.lock().servers.len()
    }

    fn calls(&self, server: usize) -> Vec<&'static str> {
        self.0.lock().servers[server]
            .calls
            .iter()
            .map(|(method, _)| *method)
            .collect()
    }

    fn requests<T: prost::Message + Default>(&self, server: usize, method: &str) -> Vec<T> {
        self.0.lock().servers[server]
            .calls
            .iter()
            .filter(|(called, _)| *called == method)
            .map(|(_, body)| T::decode(body.as_slice()).expect("decodable request"))
            .collect()
    }

    fn notifications(&self, server: usize) -> Vec<(&'static str, Value)> {
        self.0.lock().servers[server].notifications.clone()
    }

    fn kill(&self, server: usize) {
        self.0.lock().servers[server].alive = false;
    }

    fn terminated(&self, server: usize) -> bool {
        self.0.lock().servers[server].terminated
    }
}

struct FakeRunningServer {
    index: usize,
    state: FakeLanguageServer,
}

impl RunningServer for FakeRunningServer {
    fn info(&self) -> ServerInfo {
        ServerInfo {
            pid: Some(1000 + self.index as u32),
            binary: PathBuf::from("/fake/language_server_macos_arm"),
            version: Some("1.48.2".to_string()),
            build: Some("fake".to_string()),
            data_dir: PathBuf::from("/fake/data"),
            log_path: None,
        }
    }

    fn call(
        &self,
        method: &'static str,
        body: Vec<u8>,
    ) -> BoxFuture<'static, Result<Vec<u8>, ConnectError>> {
        let mut state = self.state.0.lock();
        let alive = state.servers[self.index].alive;
        state.servers[self.index].calls.push((method, body));
        if !alive {
            return futures::future::ready(Err(ConnectError::Network(
                "connection refused".to_string(),
            )))
            .boxed();
        }
        if method != HANDLE_STREAMING_TAB {
            return futures::future::ready(Ok(Vec::new())).boxed();
        }
        match state.replies.pop_front() {
            Some(FakeReply::Answer(response)) => {
                futures::future::ready(Ok(response.encode_to_vec())).boxed()
            }
            Some(FakeReply::Error(error)) => futures::future::ready(Err(error)).boxed(),
            Some(FakeReply::Hang) => futures::future::pending().boxed(),
            None => futures::future::ready(Ok(HandleStreamingTabV2Response {
                suggestion: Some(Suggestion::Noop(NoOp {})),
                ..Default::default()
            }
            .encode_to_vec()))
            .boxed(),
        }
    }

    fn notify(&self, method: &'static str, params: Value) {
        self.state.0.lock().servers[self.index]
            .notifications
            .push((method, params));
    }

    fn is_alive(&self) -> bool {
        self.state.0.lock().servers[self.index].alive
    }

    fn terminate(&self) {
        let mut state = self.state.0.lock();
        state.servers[self.index].alive = false;
        state.servers[self.index].terminated = true;
    }
}

impl Launcher for FakeLanguageServer {
    fn launch(
        &self,
        options: LaunchOptions,
    ) -> BoxFuture<'static, Result<Arc<dyn RunningServer>, String>> {
        let mut state = self.0.lock();
        state
            .launch_options
            .push((options.binary.clone(), options.workspace_uris.clone()));
        assert_eq!(options.initial_metadata.api_key, API_KEY);
        if state.failing_launches > 0 {
            state.failing_launches -= 1;
            return futures::future::ready(Err("boom".to_string())).boxed();
        }
        state.servers.push(FakeServerRecord {
            alive: true,
            ..Default::default()
        });
        let server = FakeRunningServer {
            index: state.servers.len() - 1,
            state: self.clone(),
        };
        futures::future::ready(Ok(Arc::new(server) as Arc<dyn RunningServer>)).boxed()
    }
}

fn existing_binary() -> String {
    std::env::current_exe()
        .expect("test binary")
        .to_string_lossy()
        .into_owned()
}

fn backend_for(fake: &FakeLanguageServer, cx: &TestAppContext) -> LanguageServerBackend {
    let timer_executor = cx.executor();
    let clock_executor = cx.executor();
    let spawn_executor = cx.executor();
    let mut config = LanguageServerConfig::new(PathBuf::from("/fake/data"));
    config.heartbeat_interval = None;
    let backend = LanguageServerBackend::new(
        Arc::new(fake.clone()),
        config,
        Arc::new(move |duration| timer_executor.timer(duration).boxed()),
        Arc::new(move || clock_executor.now()),
        Arc::new(move |future| spawn_executor.spawn(future).detach()),
    );
    backend.set_settings(ServerSettings {
        api_key: Some(API_KEY.to_string()),
        binary_path: Some(existing_binary()),
        proxy: None,
    });
    backend
}

fn document_context(text: &str, cursor_offset: usize) -> DocumentContext {
    DocumentContext {
        text: text.to_string(),
        absolute_uri: "file:///work/vector.py".to_string(),
        workspace_uri: "file:///work".to_string(),
        language: Language::Python,
        editor_language: "python".to_string(),
        cursor_offset,
        selection: String::new(),
        selection_start_line: 0,
        selection_end_line: 0,
        is_dirty: true,
        cursor_position: None,
    }
}

fn tab_input(text: &str, cursor_offset: usize) -> TabRequestInput {
    TabRequestInput {
        document: document_context(text, cursor_offset),
        diagnostics: Vec::new(),
        steps: Vec::new(),
        mainline_trajectory_id: String::new(),
        cached_hashes: Vec::new(),
        feedback: Vec::new(),
        trigger_id: "trigger".to_string(),
        language_server: LanguageServerContext {
            trigger: SupercompleteTriggerCondition::Typing,
            clipboard: "copied".to_string(),
            other_documents: Vec::new(),
            disable_tab_jump: true,
            tab_size: 4,
            insert_spaces: true,
        },
    }
}

fn text_buffer(text: &str) -> language::TextBuffer {
    language::TextBuffer::new(
        language::ReplicaId::LOCAL,
        language::BufferId::new(1).expect("buffer id"),
        text,
    )
}

async fn get_tab(
    backend: &LanguageServerBackend,
    input: TabRequestInput,
    cx: &mut TestAppContext,
) -> TabExchange {
    let task = cx.executor().spawn(backend.get_tab(String::new(), input));
    cx.run_until_parked();
    task.await
}

async fn start(backend: &LanguageServerBackend, cx: &mut TestAppContext) {
    let task = cx.executor().spawn(backend.ensure_started());
    cx.run_until_parked();
    assert!(task.await.is_ok(), "the server should start");
}

fn diff_reply(start_line: u64, end_line: u64, changes: &[(DiffChangeType, &str)]) -> FakeReply {
    diff_reply_for("file:///work/vector.py", start_line, end_line, changes)
}

fn diff_reply_for(
    uri: &str,
    start_line: u64,
    end_line: u64,
    changes: &[(DiffChangeType, &str)],
) -> FakeReply {
    FakeReply::Answer(HandleStreamingTabV2Response {
        completion_id: "completion-1".to_string(),
        prompt_id: "prompt-1".to_string(),
        suggestion: Some(Suggestion::Diff(Diff {
            path: uri.to_string(),
            selection_start_line: start_line,
            selection_end_line: end_line,
            character_diff: Some(CharacterDiff {
                changes: changes
                    .iter()
                    .map(|(change_type, text)| CharacterDiffChange {
                        text: text.to_string(),
                        r#type: *change_type as i32,
                    })
                    .collect(),
            }),
            ..Default::default()
        })),
        ..Default::default()
    })
}

#[gpui::test]
async fn test_backend_starts_syncs_and_predicts(cx: &mut TestAppContext) {
    let fake = FakeLanguageServer::default();
    let backend = backend_for(&fake, cx);
    let mut buffer = text_buffer("def add(a, b):\n    return a\n");
    backend.update_document(
        "file:///work/vector.py",
        "file:///work",
        "python",
        buffer.snapshot().clone(),
    );
    assert_eq!(fake.launches(), 0, "the server starts lazily");
    assert_eq!(backend.status(), ServerStatus::Idle);

    // The first request starts the server; without an edit it isn't asked.
    let exchange = get_tab(&backend, tab_input(&buffer.text(), 0), cx).await;
    assert_eq!(fake.launches(), 1);
    assert!(matches!(backend.status(), ServerStatus::Running(_)));
    let stream = exchange.result.expect("no error");
    assert!(stream.noop);
    assert_eq!(fake.calls(0), vec![REFRESH_CONTEXT, REFRESH_CONTEXT]);
    let refreshes: Vec<RefreshContextForIdeActionRequest> = fake.requests(0, REFRESH_CONTEXT);
    assert_eq!(refreshes[0].ide_action, IdeAction::Startup as i32);
    assert_eq!(
        refreshes[0].workspace_uris,
        vec!["file:///work".to_string()]
    );
    assert_eq!(
        refreshes[1].ide_action,
        IdeAction::ActiveEditorChanged as i32
    );
    // The workspace went with `initialize`; the document follows.
    assert_eq!(
        fake.0.lock().launch_options[0].1,
        vec!["file:///work".to_string()]
    );
    let notifications = fake.notifications(0);
    assert_eq!(notifications[0].0, "textDocument/didOpen");
    assert_eq!(
        notifications[0].1["textDocument"]["text"],
        "def add(a, b):\n    return a\n"
    );

    // An edit becomes an incremental didChange, after which requests go out.
    buffer.edit([(27..27, " +")]);
    backend.update_document(
        "file:///work/vector.py",
        "file:///work",
        "python",
        buffer.snapshot().clone(),
    );
    let notifications = fake.notifications(0);
    let (method, params) = notifications.last().expect("didChange");
    assert_eq!(*method, "textDocument/didChange");
    assert_eq!(params["textDocument"]["version"], 2);
    assert_eq!(
        params["contentChanges"],
        json!([{ "range": { "start": { "line": 1, "character": 12 }, "end": { "line": 1, "character": 12 } }, "text": " +" }])
    );

    fake.reply(diff_reply(
        1,
        2,
        &[
            (DiffChangeType::Unchanged, "    return a +"),
            (DiffChangeType::Insert, " b"),
        ],
    ));
    let text = buffer.text();
    let exchange = get_tab(&backend, tab_input(&text, text.len() - 1), cx).await;
    let stream = exchange.result.expect("a prediction");
    assert_eq!(stream.old_text, "    return a +");
    assert_eq!(stream.new_text, "    return a + b");
    assert_eq!(stream.line_range, Some((1, 2)));
    assert_eq!(stream.completion_id, "completion-1");
    let requests: Vec<HandleStreamingTabV2Request> = fake.requests(0, HANDLE_STREAMING_TAB);
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    let metadata = request.metadata.as_ref().expect("metadata");
    assert_eq!(metadata.api_key, API_KEY);
    assert_eq!(metadata.trigger_id, "trigger");
    let document = request.document.as_ref().expect("document");
    assert_eq!(document.text, text);
    assert_eq!(document.cursor_offset as usize, text.len() - 1);
    assert_eq!(request.clipboard_entry, "copied");
    assert!(request.disable_tab_jump);
    assert_eq!(
        request.supercomplete_trigger_condition,
        SupercompleteTriggerCondition::Typing as i32
    );
    let described = exchange.described_request.expect("described request");
    assert_eq!(
        described["metadata"]["api_key"],
        format!("<redacted, {} chars>", API_KEY.len())
    );
    // Same document: no second ACTIVE_EDITOR_CHANGED.
    assert_eq!(
        fake.requests::<RefreshContextForIdeActionRequest>(0, REFRESH_CONTEXT)
            .len(),
        2
    );
}

#[gpui::test]
async fn test_backend_times_out_and_passes_errors_on(cx: &mut TestAppContext) {
    let fake = FakeLanguageServer::default();
    let backend = backend_for(&fake, cx);
    let mut buffer = text_buffer("x = 1\n");
    backend.update_document(
        "file:///work/vector.py",
        "file:///work",
        "python",
        buffer.snapshot().clone(),
    );
    start(&backend, cx).await;
    buffer.edit([(5..5, "2")]);
    backend.update_document(
        "file:///work/vector.py",
        "file:///work",
        "python",
        buffer.snapshot().clone(),
    );

    fake.reply(FakeReply::Hang);
    let finished: Arc<Mutex<Option<TabExchange>>> = Arc::default();
    cx.executor()
        .spawn({
            let call = backend.get_tab(String::new(), tab_input(&buffer.text(), 6));
            let finished = finished.clone();
            async move {
                let exchange = call.await;
                *finished.lock() = Some(exchange);
            }
        })
        .detach();
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(11));
    cx.run_until_parked();
    assert!(
        finished.lock().is_none(),
        "still waiting before the deadline"
    );
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    let exchange = finished.lock().take().expect("finished after the deadline");
    let error = exchange.result.expect_err("timed out");
    assert!(
        matches!(&error, ConnectError::Network(message) if message.contains("didn't answer")),
        "{error:?}"
    );
    assert!(error.is_retryable());

    fake.reply(FakeReply::Error(ConnectError::Unauthenticated(
        "bad key".to_string(),
    )));
    let exchange = get_tab(&backend, tab_input(&buffer.text(), 6), cx).await;
    assert_eq!(
        exchange.result.expect_err("error"),
        ConnectError::Unauthenticated("bad key".to_string())
    );
    assert_eq!(fake.launches(), 1);
}

#[gpui::test]
async fn test_backend_restarts_a_dead_server_and_reopens_documents(cx: &mut TestAppContext) {
    let fake = FakeLanguageServer::default();
    let backend = backend_for(&fake, cx);
    let mut buffer = text_buffer("x = 1\n");
    backend.update_document(
        "file:///work/vector.py",
        "file:///work",
        "python",
        buffer.snapshot().clone(),
    );
    buffer.edit([(5..5, "2")]);
    get_tab(&backend, tab_input(&buffer.text(), 6), cx).await;
    backend.update_document(
        "file:///work/vector.py",
        "file:///work",
        "python",
        buffer.snapshot().clone(),
    );
    assert_eq!(fake.launches(), 1);

    fake.kill(0);
    buffer.edit([(6..6, "3")]);
    backend.update_document(
        "file:///work/vector.py",
        "file:///work",
        "python",
        buffer.snapshot().clone(),
    );
    fake.reply(diff_reply(0, 1, &[(DiffChangeType::Unchanged, "x = 123")]));
    let exchange = get_tab(&backend, tab_input(&buffer.text(), 7), cx).await;
    assert_eq!(fake.launches(), 2, "a dead server is started again");
    assert!(fake.terminated(0));
    // The new server gets the document again, but has seen no edit yet.
    let notifications = fake.notifications(1);
    let opened = notifications
        .iter()
        .find(|(method, _)| *method == "textDocument/didOpen")
        .expect("didOpen");
    assert_eq!(opened.1["textDocument"]["text"], "x = 123\n");
    assert!(exchange.result.expect("no error").noop);
    assert_eq!(fake.calls(1), vec![REFRESH_CONTEXT, REFRESH_CONTEXT]);

    buffer.edit([(7..7, "4")]);
    backend.update_document(
        "file:///work/vector.py",
        "file:///work",
        "python",
        buffer.snapshot().clone(),
    );
    let exchange = get_tab(&backend, tab_input(&buffer.text(), 8), cx).await;
    assert!(exchange.result.is_ok());
    assert_eq!(
        fake.requests::<HandleStreamingTabV2Request>(1, HANDLE_STREAMING_TAB)
            .len(),
        1
    );
    assert!(backend.generation() >= 2);
}

#[gpui::test]
async fn test_backend_backs_off_after_failed_starts(cx: &mut TestAppContext) {
    let fake = FakeLanguageServer::default();
    fake.0.lock().failing_launches = 2;
    let backend = backend_for(&fake, cx);

    let exchange = get_tab(&backend, tab_input("x", 1), cx).await;
    assert!(matches!(exchange.result, Err(ConnectError::Network(_))));
    assert!(matches!(backend.status(), ServerStatus::Failed { .. }));
    assert_eq!(fake.0.lock().launch_options.len(), 1);

    // Within the backoff, nothing is launched.
    get_tab(&backend, tab_input("x", 1), cx).await;
    assert_eq!(fake.0.lock().launch_options.len(), 1);

    cx.executor().advance_clock(Duration::from_secs(1));
    get_tab(&backend, tab_input("x", 1), cx).await;
    assert_eq!(fake.0.lock().launch_options.len(), 2);
    // The second failure doubles the wait.
    cx.executor().advance_clock(Duration::from_secs(1));
    get_tab(&backend, tab_input("x", 1), cx).await;
    assert_eq!(fake.0.lock().launch_options.len(), 2);
    cx.executor().advance_clock(Duration::from_secs(1));
    get_tab(&backend, tab_input("x", 1), cx).await;
    assert_eq!(fake.0.lock().launch_options.len(), 3);
    assert_eq!(fake.launches(), 1);
    assert!(matches!(backend.status(), ServerStatus::Running(_)));
}

#[gpui::test]
async fn test_backend_reports_a_missing_binary(cx: &mut TestAppContext) {
    let fake = FakeLanguageServer::default();
    let backend = backend_for(&fake, cx);
    backend.set_settings(ServerSettings {
        api_key: Some(API_KEY.to_string()),
        binary_path: Some("/nonexistent/language_server_macos_arm".to_string()),
        proxy: None,
    });
    let exchange = get_tab(&backend, tab_input("x", 1), cx).await;
    let error = exchange.result.expect_err("not found");
    assert!(!error.is_retryable());
    assert!(
        matches!(backend.status(), ServerStatus::NotFound(message) if message.contains("/nonexistent"))
    );
    assert_eq!(fake.0.lock().launch_options.len(), 0);
}

#[gpui::test]
async fn test_backend_stops_the_server_when_settings_change(cx: &mut TestAppContext) {
    let fake = FakeLanguageServer::default();
    let backend = backend_for(&fake, cx);
    start(&backend, cx).await;
    backend.set_settings(ServerSettings {
        api_key: Some("devin-session-token$another".to_string()),
        binary_path: Some(existing_binary()),
        proxy: None,
    });
    cx.run_until_parked();
    assert!(fake.terminated(0));
    assert_eq!(fake.calls(0).last(), Some(&EXIT));
    assert_eq!(backend.status(), ServerStatus::Idle);
}

// ---------------------------------------------------------------------------
// The delegate with the language server backend selected.

const SOURCE: &str =
    "def area(width, height):\n    return width * height\n\n\ndef main():\n    print(area(2, 3))\n";

struct Fixture {
    fake: FakeLanguageServer,
    windsurf: Entity<Windsurf>,
    buffer: Entity<Buffer>,
    delegate: Entity<WindsurfDelegate>,
}

fn set_user_settings(settings: Value, cx: &mut TestAppContext) {
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(&settings.to_string(), cx)
                .expect("user settings should load");
        });
    });
}

fn language_server_settings() -> Value {
    json!({ "fork": { "windsurf": {
        "api_key": API_KEY,
        "backend": "language_server",
        "language_server_path": existing_binary(),
    } } })
}

async fn setup(cx: &mut TestAppContext) -> Fixture {
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });
    set_user_settings(language_server_settings(), cx);
    let fake = FakeLanguageServer::default();
    let windsurf = cx.update(|cx| {
        let factory: crate::LanguageServerFactory = Arc::new({
            let fake = fake.clone();
            move |cx| {
                let timer_executor = cx.background_executor().clone();
                let clock_executor = cx.background_executor().clone();
                let spawn_executor = cx.background_executor().clone();
                let mut config = LanguageServerConfig::new(PathBuf::from("/fake/data"));
                config.heartbeat_interval = None;
                LanguageServerBackend::new(
                    Arc::new(fake.clone()),
                    config,
                    Arc::new(move |duration| timer_executor.timer(duration).boxed()),
                    Arc::new(move || clock_executor.now()),
                    Arc::new(move |future| spawn_executor.spawn(future).detach()),
                )
            }
        });
        let native = crate::native::NativeBackend::new(
            http_client::FakeHttpClient::with_404_response(),
            {
                let executor = cx.background_executor().clone();
                Arc::new(move |duration| executor.timer(duration).boxed())
            },
            crate::native::ClientConfig::default(),
        );
        crate::init_with_backends(Arc::new(native), factory, cx);
        Windsurf::global(cx).expect("windsurf should be initialized")
    });
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/root"), json!({ "src": { "shapes.py": SOURCE } }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/root/src/shapes.py"), cx)
        })
        .await
        .expect("buffer should open");
    windsurf.update(cx, |windsurf, cx| windsurf.track_buffer(&buffer, cx));
    let delegate = cx.new(|cx| {
        let mut delegate = WindsurfDelegate::new(windsurf.clone(), cx);
        delegate.set_project(Some(project.downgrade()));
        delegate
    });
    Fixture {
        fake,
        windsurf,
        buffer,
        delegate,
    }
}

fn shapes_uri() -> String {
    crate::context::file_uri(std::path::Path::new(path!("/root/src/shapes.py")))
}

impl Fixture {
    fn edit(&self, offset: usize, text: &str, cx: &mut TestAppContext) {
        self.buffer.update(cx, |buffer, cx| {
            buffer.edit([(offset..offset, text)], None, cx);
        });
    }

    fn refresh_at(
        &self,
        offset: usize,
        trigger: EditPredictionRequestTrigger,
        cx: &mut TestAppContext,
    ) {
        let buffer = self.buffer.clone();
        self.delegate.update(cx, |delegate, cx| {
            let cursor = buffer.read(cx).anchor_before(offset);
            delegate.refresh(buffer, cursor, Duration::ZERO, trigger, cx)
        });
        cx.executor().advance_clock(crate::delegate::DEBOUNCE);
        cx.run_until_parked();
    }

    fn suggest_at(&self, offset: usize, cx: &mut TestAppContext) -> Option<EditPrediction> {
        let buffer = self.buffer.clone();
        self.delegate.update(cx, |delegate, cx| {
            let cursor = buffer.read(cx).anchor_before(offset);
            delegate.suggest(&buffer, cursor, cx)
        })
    }

    fn edits(
        &self,
        prediction: Option<EditPrediction>,
        cx: &mut TestAppContext,
    ) -> Vec<(Point, Point, String)> {
        let Some(EditPrediction::Local { edits, .. }) = prediction else {
            panic!("expected a local prediction");
        };
        self.buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            edits
                .into_iter()
                .map(|(range, text)| {
                    (
                        snapshot.offset_to_point(range.start.to_offset(&snapshot)),
                        snapshot.offset_to_point(range.end.to_offset(&snapshot)),
                        text.to_string(),
                    )
                })
                .collect()
        })
    }

    fn text(&self, cx: &mut TestAppContext) -> String {
        self.buffer.read_with(cx, |buffer, _| buffer.text())
    }
}

#[gpui::test]
async fn test_delegate_shows_language_server_predictions_and_reports_feedback(
    cx: &mut TestAppContext,
) {
    let fixture = setup(cx).await;
    cx.run_until_parked();
    assert_eq!(fixture.fake.launches(), 0);
    let offset = SOURCE.find("width,").expect("width") + "width".len();
    fixture.edit(offset, "_px", cx);
    let text = fixture.text(cx);
    let cursor = offset + "_px".len();

    // The first request starts the server, which has seen no edit yet.
    fixture.refresh_at(cursor, EditPredictionRequestTrigger::BufferEdit, cx);
    assert_eq!(fixture.fake.launches(), 1);
    assert!(fixture.suggest_at(cursor, cx).is_none());
    assert_eq!(
        fixture
            .windsurf
            .read_with(cx, |windsurf, cx| windsurf.status(cx)),
        Status::Ready
    );

    fixture.edit(cursor, "_", cx);
    fixture.buffer.update(cx, |buffer, cx| {
        buffer.edit([(cursor..cursor + 1, "")], None, cx);
    });
    let notifications = fixture.fake.notifications(0);
    assert!(
        notifications
            .iter()
            .any(|(method, _)| *method == "textDocument/didChange"),
        "edits are passed on: {notifications:?}"
    );

    fixture.fake.reply(diff_reply_for(
        &shapes_uri(),
        1,
        2,
        &[
            (DiffChangeType::Unchanged, "    return width"),
            (DiffChangeType::Insert, "_px"),
            (DiffChangeType::Unchanged, " * height"),
        ],
    ));
    fixture.refresh_at(cursor, EditPredictionRequestTrigger::BufferEdit, cx);
    let requests: Vec<HandleStreamingTabV2Request> = fixture.fake.requests(0, HANDLE_STREAMING_TAB);
    assert_eq!(requests.len(), 1);
    let document = requests[0].document.as_ref().expect("document");
    assert_eq!(document.text, text);
    assert_eq!(document.cursor_offset as usize, cursor);
    assert_eq!(document.absolute_uri, shapes_uri());
    let editor_options = requests[0].editor_options.as_ref().expect("editor options");
    assert_eq!(editor_options.tab_size, 4);
    assert!(!requests[0].disable_tab_jump);

    let prediction = fixture.suggest_at(cursor, cx);
    assert_eq!(
        fixture.edits(prediction, cx),
        vec![(Point::new(1, 16), Point::new(1, 16), "_px".to_string())]
    );

    // Accepting reports the language server's completion id.
    fixture.delegate.update(cx, |delegate, cx| {
        delegate.did_show(SuggestionDisplayType::GhostText, cx);
        delegate.accept(cx);
    });
    cx.run_until_parked();
    let feedback: Vec<ProvideCompletionFeedbackRequest> =
        fixture.fake.requests(0, PROVIDE_COMPLETION_FEEDBACK);
    assert_eq!(feedback.len(), 1);
    assert!(feedback[0].is_accepted);
    assert_eq!(feedback[0].completion_id, "completion-1");
    assert_eq!(feedback[0].prompt_id, "prompt-1");

    // Nothing native is sent: no prefetch, no Tab Jump request.
    assert_eq!(
        fixture
            .windsurf
            .read_with(cx, |windsurf, _| windsurf.stats().predictive_requests),
        0
    );
}

#[gpui::test]
async fn test_delegate_shows_language_server_tab_jump(cx: &mut TestAppContext) {
    let fixture = setup(cx).await;
    let offset = SOURCE.find("height)").expect("height") + "height".len();
    fixture.edit(offset, "s", cx);
    fixture.refresh_at(offset + 1, EditPredictionRequestTrigger::BufferEdit, cx);
    fixture.edit(offset + 1, "!", cx);
    fixture.buffer.update(cx, |buffer, cx| {
        buffer.edit([(offset + 1..offset + 2, "")], None, cx);
    });

    // Row 1, UTF-16 column 25: after `height` in the body.
    fixture
        .fake
        .reply(FakeReply::Answer(HandleStreamingTabV2Response {
            completion_id: "jump-1".to_string(),
            prompt_id: "prompt-jump".to_string(),
            suggestion: Some(Suggestion::TabJump(TabJump {
                path: shapes_uri(),
                jump_position: Some(DocumentPosition { row: 1, col: 25 }),
                is_import: false,
            })),
            ..Default::default()
        }));
    fixture.refresh_at(offset + 1, EditPredictionRequestTrigger::BufferEdit, cx);
    let Some(EditPrediction::Jump { target, .. }) = fixture.suggest_at(offset + 1, cx) else {
        panic!("expected a jump");
    };
    let target_point = fixture.buffer.read_with(cx, |buffer, _| {
        buffer
            .snapshot()
            .offset_to_point(target.to_offset(&buffer.snapshot()))
    });
    assert_eq!(target_point, Point::new(1, 25));

    // Arriving at the target reports the jump as accepted and asks for the
    // edit there with TAB_JUMP_ACCEPT.
    let target_offset = fixture.buffer.read_with(cx, |buffer, _| {
        buffer.snapshot().point_to_offset(Point::new(1, 25))
    });
    assert!(fixture.suggest_at(target_offset, cx).is_none());
    cx.run_until_parked();
    let requests: Vec<HandleStreamingTabV2Request> = fixture.fake.requests(0, HANDLE_STREAMING_TAB);
    let last = requests.last().expect("a request at the target");
    assert_eq!(
        last.supercomplete_trigger_condition,
        SupercompleteTriggerCondition::TabJumpAccept as i32
    );
    assert_eq!(
        last.document.as_ref().expect("document").cursor_offset as usize,
        target_offset
    );
    let feedback: Vec<ProvideCompletionFeedbackRequest> =
        fixture.fake.requests(0, PROVIDE_COMPLETION_FEEDBACK);
    assert_eq!(feedback.len(), 1);
    assert_eq!(feedback[0].completion_id, "jump-1");
    assert!(feedback[0].is_accepted);

    // A rejected jump is reported as rejected.
    fixture
        .fake
        .reply(FakeReply::Answer(HandleStreamingTabV2Response {
            completion_id: "jump-2".to_string(),
            suggestion: Some(Suggestion::TabJump(TabJump {
                path: String::new(),
                jump_position: Some(DocumentPosition { row: 0, col: 4 }),
                is_import: false,
            })),
            ..Default::default()
        }));
    fixture.edit(target_offset, "x", cx);
    fixture.refresh_at(
        target_offset + 1,
        EditPredictionRequestTrigger::BufferEdit,
        cx,
    );
    assert!(matches!(
        fixture.suggest_at(target_offset + 1, cx),
        Some(EditPrediction::Jump { .. })
    ));
    fixture.delegate.update(cx, |delegate, cx| {
        delegate.discard(EditPredictionDiscardReason::Rejected, cx)
    });
    cx.run_until_parked();
    let feedback: Vec<ProvideCompletionFeedbackRequest> =
        fixture.fake.requests(0, PROVIDE_COMPLETION_FEEDBACK);
    assert_eq!(feedback.len(), 2);
    assert_eq!(feedback[1].completion_id, "jump-2");
    assert!(!feedback[1].is_accepted);
    assert!(feedback[1].is_intentional_reject);
}

#[gpui::test]
async fn test_switching_backends_stops_the_server(cx: &mut TestAppContext) {
    let fixture = setup(cx).await;
    fixture.edit(0, "#", cx);
    fixture.refresh_at(1, EditPredictionRequestTrigger::BufferEdit, cx);
    assert_eq!(fixture.fake.launches(), 1);
    assert!(
        fixture
            .windsurf
            .read_with(cx, |windsurf, _| windsurf.uses_language_server())
    );

    set_user_settings(
        json!({ "fork": { "windsurf": { "api_key": API_KEY } } }),
        cx,
    );
    cx.run_until_parked();
    assert!(
        fixture.fake.terminated(0),
        "switching to native stops the server"
    );
    assert!(
        !fixture
            .windsurf
            .read_with(cx, |windsurf, _| windsurf.uses_language_server())
    );

    // Switching back starts a new one on the next request.
    set_user_settings(language_server_settings(), cx);
    cx.run_until_parked();
    assert_eq!(fixture.fake.launches(), 1, "not before a request");
    fixture.edit(1, "#", cx);
    fixture.refresh_at(2, EditPredictionRequestTrigger::BufferEdit, cx);
    assert_eq!(fixture.fake.launches(), 2);
}

#[gpui::test]
async fn test_language_server_status_messages(cx: &mut TestAppContext) {
    let fixture = setup(cx).await;
    set_user_settings(
        json!({ "fork": { "windsurf": {
            "api_key": API_KEY,
            "backend": "language_server",
            "language_server_path": "/nonexistent/language_server_macos_arm",
        } } }),
        cx,
    );
    fixture.edit(0, "#", cx);
    fixture.refresh_at(1, EditPredictionRequestTrigger::BufferEdit, cx);
    let status = fixture
        .windsurf
        .read_with(cx, |windsurf, cx| windsurf.status(cx));
    let Status::Error(crate::ErrorStatus::LanguageServerNotFound(message)) = &status else {
        panic!("expected not found, got {status:?}");
    };
    assert!(message.contains("/nonexistent"));
    let (label, _, _) = crate::status::describe(&status);
    assert_eq!(label, "Windsurf: language server not found");

    fixture.fake.0.lock().failing_launches = 1;
    set_user_settings(language_server_settings(), cx);
    fixture.edit(1, "#", cx);
    fixture.refresh_at(2, EditPredictionRequestTrigger::BufferEdit, cx);
    let status = fixture
        .windsurf
        .read_with(cx, |windsurf, cx| windsurf.status(cx));
    assert!(
        matches!(&status, Status::Error(crate::ErrorStatus::LanguageServerFailed { message, .. }) if message == "boom"),
        "{status:?}"
    );
}

// ---------------------------------------------------------------------------
// The real binary. Run on a Mac with Devin installed:
//   WINDSURF_LS_ENV=<path to fork/spike/.env> cargo test -p fork_windsurf live_language_server -- --ignored --nocapture
// It starts its own server (with its own data directory), sends a handful of
// requests at least two seconds apart, and stops the server again.

#[cfg(unix)]
fn api_key_from_env_file() -> Option<String> {
    let path = std::env::var("WINDSURF_LS_ENV").ok()?;
    let content = std::fs::read_to_string(path).ok()?;
    content.lines().find_map(|line| {
        let value = line.strip_prefix("WINDSURF_API_KEY=")?;
        let value = value.trim().trim_matches('"').trim_matches('\'');
        (!value.is_empty()).then(|| value.to_string())
    })
}

#[cfg(unix)]
fn process_is_running(pid: u32) -> bool {
    run_command("kill", &["-0", &pid.to_string()]).is_some_and(|output| output.status.success())
}

#[cfg(unix)]
fn resident_kilobytes(pid: u32) -> Option<u64> {
    let output = run_command("ps", &["-o", "rss=", "-p", &pid.to_string()])?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

#[cfg(unix)]
fn walkdir(root: &std::path::Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path.clone());
            }
            found.push(path);
        }
    }
    found.sort();
    found
}

#[cfg(unix)]
#[test]
#[ignore = "starts the real language server and sends requests to the Windsurf cloud"]
fn live_language_server() {
    let Some(api_key) = api_key_from_env_file() else {
        panic!("set WINDSURF_LS_ENV to a file with WINDSURF_API_KEY=");
    };
    let data_dir = tempfile::tempdir().expect("temporary directory");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_path = workspace
        .path()
        .canonicalize()
        .expect("canonical workspace");
    run_command("git", &["init", "-q", &workspace_path.to_string_lossy()]).expect("git init");
    let file_path = workspace_path.join("vector.py");
    let source = "import math\n\n\nclass Vector:\n    def __init__(self, x, y):\n        self.x = x\n        self.y = y\n\n    def length(self):\n        return math.sqrt(self.x * self.x + self.y * self.y)\n\n    def scale(self, factor):\n        return Vector(self.x * factor, self.y * factor)\n\n\ndef add(a, b):\n    return Vector(a.x + b.x, a.y + b.y)\n\n\ndef main():\n    v = Vector(3, 4)\n    print(v.length())\n    w = add(v, Vector(1, 1))\n    print(w.length())\n";
    std::fs::write(&file_path, source).expect("write");
    let uri = crate::context::file_uri(&file_path);
    let workspace_uri = crate::context::file_uri(&workspace_path);

    let mut config = LanguageServerConfig::new(data_dir.path().to_path_buf());
    config.heartbeat_interval = Some(Duration::from_secs(1));
    let backend = LanguageServerBackend::new(
        Arc::new(process::ProcessLauncher),
        config,
        thread_timer(),
        Arc::new(std::time::Instant::now),
        Arc::new(|future| smol::spawn(future).detach()),
    );
    backend.set_settings(ServerSettings {
        api_key: Some(api_key),
        binary_path: None,
        proxy: None,
    });
    let mut buffer = text_buffer(source);
    backend.update_document(&uri, &workspace_uri, "python", buffer.snapshot().clone());

    let started_at = std::time::Instant::now();
    let server =
        smol::block_on(backend.ensure_started()).expect("the language server should start");
    let started_in = started_at.elapsed();
    let info = server.info();
    let pid = info.pid.expect("pid");
    eprintln!(
        "started {} (version {:?}, build {:?}, pid {pid}) in {} ms",
        info.binary.display(),
        info.version,
        info.build,
        started_in.as_millis()
    );
    assert!(process_is_running(pid));
    std::thread::sleep(Duration::from_secs(3));
    eprintln!(
        "resident memory after start: {:?} KiB",
        resident_kilobytes(pid)
    );
    if let Some(output) = run_command("lsof", &["-p", &pid.to_string()]) {
        let home = util::paths::home_dir().to_string_lossy().into_owned();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if line.contains(&format!("{home}/.codeium")) {
                eprintln!("opened in ~/.codeium: {line}");
            }
        }
    }

    let input_for = |text: &str, cursor_offset: usize, trigger| TabRequestInput {
        document: DocumentContext {
            text: text.to_string(),
            absolute_uri: uri.clone(),
            workspace_uri: workspace_uri.clone(),
            language: Language::Python,
            editor_language: "python".to_string(),
            cursor_offset,
            selection: String::new(),
            selection_start_line: 0,
            selection_end_line: 0,
            is_dirty: true,
            cursor_position: None,
        },
        diagnostics: Vec::new(),
        steps: Vec::new(),
        mainline_trajectory_id: String::new(),
        cached_hashes: Vec::new(),
        feedback: Vec::new(),
        trigger_id: uuid::Uuid::new_v4().to_string(),
        language_server: LanguageServerContext {
            trigger,
            tab_size: 4,
            insert_spaces: true,
            ..Default::default()
        },
    };

    // Type a line at the end, one character at a time, like the spike.
    let typed = "    u = v.sc";
    let mut offset = source.len();
    for character in typed.chars() {
        buffer.edit([(offset..offset, character.to_string())]);
        offset += character.len_utf8();
        backend.update_document(&uri, &workspace_uri, "python", buffer.snapshot().clone());
        backend.selection_changed(|| {
            let mut document = input_for(
                &buffer.text(),
                offset,
                SupercompleteTriggerCondition::Typing,
            )
            .document;
            document.cursor_offset = offset;
            Some(document)
        });
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_secs(2));

    let mut latencies = Vec::new();
    let mut predictions = 0;
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(2500));
            buffer.edit([(offset..offset, "a")]);
            offset += 1;
            backend.update_document(&uri, &workspace_uri, "python", buffer.snapshot().clone());
        }
        let exchange = smol::block_on(backend.get_tab(
            String::new(),
            input_for(
                &buffer.text(),
                offset,
                SupercompleteTriggerCondition::Typing,
            ),
        ));
        latencies.push(exchange.elapsed);
        match &exchange.result {
            Ok(stream) => {
                eprintln!(
                    "attempt {attempt}: {} ms, noop {}, tab jump {:?}, lines {:?}, filter {:?}, new text tail {:?}",
                    exchange.elapsed.as_millis(),
                    stream.noop,
                    stream.jump_position,
                    stream.line_range,
                    stream.filter_reason,
                    stream.new_text.lines().last()
                );
                if !stream.noop {
                    predictions += 1;
                }
            }
            Err(error) => eprintln!(
                "attempt {attempt}: {} ms, error {error}",
                exchange.elapsed.as_millis()
            ),
        }
    }
    eprintln!(
        "resident memory after the requests: {:?} KiB",
        resident_kilobytes(pid)
    );
    let mut files = Vec::new();
    for entry in walkdir(data_dir.path()) {
        files.push(
            entry
                .strip_prefix(data_dir.path())
                .unwrap_or(&entry)
                .to_string_lossy()
                .into_owned(),
        );
    }
    eprintln!("data directory: {files:?}");
    eprintln!(
        "latencies: {:?}",
        latencies
            .iter()
            .map(|latency| latency.as_millis())
            .collect::<Vec<_>>()
    );

    smol::block_on(backend.shutdown());
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !process_is_running(pid),
        "the language server should be stopped"
    );
    assert!(data_dir.path().join("database").is_dir());
    assert!(
        predictions > 0,
        "at least one request should get a prediction"
    );
}
