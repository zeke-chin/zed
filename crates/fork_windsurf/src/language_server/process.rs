//! Starting the real language server binary, the way `extension.js` does
//! (`fork/spike/lib/ls_driver.py`):
//! - a local "extension server" (Connect over HTTP) that the language server
//!   calls back with its ports (`LanguageServerStarted`). Every other call it
//!   makes there gets an empty answer. Calls without our CSRF token are refused,
//!   since the reported port is where requests with the API key go;
//! - a Unix socket passed as `--parent_pipe_path`: the server connects to it and
//!   exits as soon as the connection closes, so it doesn't outlive Zed even
//!   when Zed crashes;
//! - the API key in a `Metadata` message on stdin (`--stdin_initial_metadata`);
//! - its database and settings under Zed's data directory instead of `~/.codeium`,
//!   so it doesn't share state with a running Devin or Windsurf app.

use std::{
    collections::VecDeque,
    io::Write as _,
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures::{
    AsyncBufReadExt as _, AsyncRead, AsyncWriteExt as _, FutureExt as _, StreamExt as _,
    channel::oneshot, future::BoxFuture,
};
use parking_lot::Mutex;
use prost::Message as _;
use serde_json::Value;
use util::command::{Child, Stdio};

use super::{
    LaunchOptions, Launcher, RunningServer, ServerInfo, Spawner,
    http::{self, CSRF_HEADER},
    lsp::LspConnection,
    request,
};
use crate::{
    native::connect::{ConnectError, encode_frame},
    proto::exa::extension_server_pb::{
        LanguageServerStartedRequest, LanguageServerStartedResponse,
    },
};

const LOG_FILE_NAME: &str = "language_server.log";
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const LOG_TAIL_LINES: usize = 20;
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(200);
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);
const STARTED_PATH_SUFFIX: &str = "/LanguageServerStarted";

pub struct ProcessLauncher;

impl Launcher for ProcessLauncher {
    fn launch(
        &self,
        options: LaunchOptions,
    ) -> BoxFuture<'static, Result<Arc<dyn RunningServer>, String>> {
        async move {
            let server = launch(options).await?;
            Ok(server as Arc<dyn RunningServer>)
        }
        .boxed()
    }
}

pub fn arguments(
    options: &LaunchOptions,
    api_server_url: &str,
    extension_server_port: u16,
    parent_pipe: &Path,
) -> Vec<String> {
    let data_dir = &options.data_dir;
    vec![
        "--api_server_url".to_string(),
        api_server_url.to_string(),
        "--run_child".to_string(),
        "--enable_lsp".to_string(),
        "--extension_server_port".to_string(),
        extension_server_port.to_string(),
        "--ide_name".to_string(),
        "windsurf".to_string(),
        "--random_port".to_string(),
        "--inference_api_server_url".to_string(),
        "https://inference.codeium.com".to_string(),
        "--database_dir".to_string(),
        data_dir.join("database").to_string_lossy().into_owned(),
        // Tab doesn't use the code index (`fork/spike/结论.md` 问题 13), and
        // indexing a large workspace costs memory and CPU.
        "--enable_index_service=false".to_string(),
        "--codeium_dir".to_string(),
        data_dir.join("codeium").to_string_lossy().into_owned(),
        "--parent_pipe_path".to_string(),
        parent_pipe.to_string_lossy().into_owned(),
        "--windsurf_version".to_string(),
        options.windsurf_version.clone(),
        "--stdin_initial_metadata".to_string(),
        "--detect_proxy=false".to_string(),
    ]
}

async fn launch(options: LaunchOptions) -> Result<Arc<ProcessServer>, String> {
    for directory in ["database", "codeium"] {
        std::fs::create_dir_all(options.data_dir.join(directory)).map_err(|error| {
            format!(
                "couldn't create {}: {error}",
                options.data_dir.join(directory).display()
            )
        })?;
    }
    let csrf_token = uuid::Uuid::new_v4().to_string();
    let (extension_server, started) = ExtensionServer::start(csrf_token.clone())?;
    let parent_pipe = ParentPipe::start()?;
    let log_path = options.data_dir.join(LOG_FILE_NAME);
    let log = Arc::new(Mutex::new(LogSink::create(&log_path)));

    let api_server_url = "https://server.codeium.com";
    let mut command = util::command::new_command(&options.binary);
    command
        .args(arguments(
            &options,
            api_server_url,
            extension_server.port,
            &parent_pipe.path,
        ))
        .env("WINDSURF_CSRF_TOKEN", &csrf_token)
        .current_dir(&options.data_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(app_root) = &options.product.app_root {
        command.env("CODEIUM_EDITOR_APP_ROOT", app_root);
    }
    if let Some(proxy) = &options.proxy {
        for name in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
            command.env(name, proxy);
        }
        command.env("NO_PROXY", "127.0.0.1,localhost");
        command.env("no_proxy", "127.0.0.1,localhost");
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("couldn't run {}: {error}", options.binary.to_string_lossy()))?;
    let pid = child.id();
    log::info!(
        "windsurf: starting the language server {} (pid {pid}), data in {}",
        options.binary.display(),
        options.data_dir.display()
    );

    if let Some(mut stdin) = child.stdin.take() {
        let metadata = options.initial_metadata.encode_to_vec();
        if let Err(error) = stdin.write_all(&metadata).await {
            log::warn!("windsurf: couldn't pass the metadata to the language server: {error}");
        }
        stdin.close().await.ok();
    }
    if let Some(stdout) = child.stdout.take() {
        copy_output(stdout, log.clone(), &options.spawner);
    }
    if let Some(stderr) = child.stderr.take() {
        copy_output(stderr, log.clone(), &options.spawner);
    }

    let started = wait_for_start(&mut child, started, &options, &log).await;
    let started = match started {
        Ok(started) => started,
        Err(error) => {
            child.kill().ok();
            return Err(error);
        }
    };
    let csrf_token = if started.csrf_token.is_empty() {
        csrf_token
    } else {
        started.csrf_token.clone()
    };
    let connect_port = u16::try_from(started.language_server_port)
        .map_err(|_| format!("invalid port {}", started.language_server_port))?;
    let lsp_port = u16::try_from(started.lsp_port)
        .map_err(|_| format!("invalid LSP port {}", started.lsp_port))?;

    let lsp = match LspConnection::connect(lsp_port) {
        Ok(lsp) => lsp,
        Err(error) => {
            child.kill().ok();
            return Err(format!(
                "couldn't connect to the language server's LSP port: {error}"
            ));
        }
    };
    let initialize = lsp.request(
        "initialize",
        request::initialize_params(&options.workspace_uris, &options.windsurf_version),
    );
    let deadline = (options.timer)(INITIALIZE_TIMEOUT).fuse();
    let initialize = initialize.fuse();
    futures::pin_mut!(deadline, initialize);
    let initialized: Result<Value, String> = futures::select_biased! {
        result = initialize => result.unwrap_or_else(|_| Err("the LSP connection closed".to_string())),
        _ = deadline => Err("LSP initialize timed out".to_string()),
    };
    let initialized = match initialized {
        Ok(initialized) => initialized,
        Err(error) => {
            child.kill().ok();
            return Err(format!("LSP initialize failed: {error}"));
        }
    };
    lsp.notify("initialized", serde_json::json!({}));
    let build = initialized
        .pointer("/serverInfo/version")
        .and_then(Value::as_str)
        .map(str::to_string);

    Ok(Arc::new(ProcessServer {
        info: ServerInfo {
            pid: Some(pid),
            binary: options.binary.clone(),
            version: options.product.codeium_version.clone(),
            build,
            data_dir: options.data_dir.clone(),
            log_path: Some(log_path),
        },
        connect_port,
        csrf_token,
        lsp,
        child: Mutex::new(Some(child)),
        extension_server: Mutex::new(Some(extension_server)),
        parent_pipe: Mutex::new(Some(parent_pipe)),
        terminated: AtomicBool::new(false),
    }))
}

async fn wait_for_start(
    child: &mut Child,
    started: oneshot::Receiver<LanguageServerStartedRequest>,
    options: &LaunchOptions,
    log: &Arc<Mutex<LogSink>>,
) -> Result<LanguageServerStartedRequest, String> {
    let deadline = (options.timer)(options.launch_timeout).fuse();
    let started = started.fuse();
    futures::pin_mut!(deadline, started);
    loop {
        let tick = (options.timer)(EXIT_POLL_INTERVAL).fuse();
        futures::pin_mut!(tick);
        futures::select_biased! {
            started = started => {
                return started.map_err(|_| "the extension server stopped".to_string());
            }
            _ = deadline => {
                return Err(format!(
                    "the language server didn't report its ports within {} s{}",
                    options.launch_timeout.as_secs(),
                    log.lock().tail_suffix()
                ));
            }
            _ = tick => {
                if let Ok(Some(status)) = child.try_status() {
                    // Give the output a moment to reach the log.
                    (options.timer)(EXIT_POLL_INTERVAL).await;
                    return Err(format!(
                        "the language server exited during startup ({status}){}",
                        log.lock().tail_suffix()
                    ));
                }
            }
        }
    }
}

fn copy_output(
    output: impl AsyncRead + Unpin + Send + 'static,
    log: Arc<Mutex<LogSink>>,
    spawner: &Spawner,
) {
    spawner(
        async move {
            let mut lines = futures::io::BufReader::new(output).lines();
            while let Some(Ok(line)) = lines.next().await {
                log.lock().push(&line);
            }
        }
        .boxed(),
    );
}

/// The server's output: written to a log file (truncated on each start and
/// capped) and the last lines kept for error messages.
struct LogSink {
    file: Option<std::fs::File>,
    written: u64,
    tail: VecDeque<String>,
}

impl LogSink {
    fn create(path: &Path) -> Self {
        let file = std::fs::File::create(path)
            .map_err(|error| {
                log::warn!(
                    "windsurf: couldn't create the language server log {}: {error}",
                    path.display()
                )
            })
            .ok();
        Self {
            file,
            written: 0,
            tail: VecDeque::new(),
        }
    }

    fn push(&mut self, line: &str) {
        if self.written < MAX_LOG_BYTES
            && let Some(file) = &mut self.file
        {
            if writeln!(file, "{line}").is_err() {
                self.file = None;
            }
            self.written += line.len() as u64 + 1;
        }
        self.tail.push_back(line.to_string());
        while self.tail.len() > LOG_TAIL_LINES {
            self.tail.pop_front();
        }
    }

    fn tail_suffix(&self) -> String {
        let tail: Vec<&str> = self
            .tail
            .iter()
            .map(String::as_str)
            .filter(|line| !line.trim().is_empty())
            .collect();
        if tail.is_empty() {
            String::new()
        } else {
            format!(": {}", tail[tail.len().saturating_sub(5)..].join(" | "))
        }
    }
}

pub struct ProcessServer {
    info: ServerInfo,
    connect_port: u16,
    csrf_token: String,
    lsp: LspConnection,
    child: Mutex<Option<Child>>,
    extension_server: Mutex<Option<ExtensionServer>>,
    parent_pipe: Mutex<Option<ParentPipe>>,
    terminated: AtomicBool,
}

impl RunningServer for ProcessServer {
    fn info(&self) -> ServerInfo {
        self.info.clone()
    }

    fn call(
        &self,
        method: &'static str,
        body: Vec<u8>,
    ) -> BoxFuture<'static, Result<Vec<u8>, ConnectError>> {
        let port = self.connect_port;
        let csrf_token = self.csrf_token.clone();
        async move { http::unary(port, &csrf_token, method, &body).await }.boxed()
    }

    fn notify(&self, method: &'static str, params: Value) {
        self.lsp.notify(method, params);
    }

    fn is_alive(&self) -> bool {
        if self.terminated.load(Ordering::SeqCst) || self.lsp.is_closed() {
            return false;
        }
        self.child
            .lock()
            .as_mut()
            .is_some_and(|child| matches!(child.try_status(), Ok(None)))
    }

    fn terminate(&self) {
        if self.terminated.swap(true, Ordering::SeqCst) {
            return;
        }
        self.lsp.close();
        if let Some(mut child) = self.child.lock().take()
            && matches!(child.try_status(), Ok(None))
            && let Err(error) = child.kill()
        {
            log::warn!(
                "windsurf: couldn't stop the language server (pid {}): {error}",
                child.id()
            );
        }
        self.parent_pipe.lock().take();
        self.extension_server.lock().take();
    }
}

impl Drop for ProcessServer {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// The Connect endpoint the language server reports to.
struct ExtensionServer {
    server: Arc<tiny_http::Server>,
    port: u16,
}

impl ExtensionServer {
    fn start(
        csrf_token: String,
    ) -> Result<(Self, oneshot::Receiver<LanguageServerStartedRequest>), String> {
        let server = tiny_http::Server::http("127.0.0.1:0")
            .map_err(|error| format!("couldn't start the extension server: {error}"))?;
        let port = server
            .server_addr()
            .to_ip()
            .map(|address| address.port())
            .ok_or("the extension server has no TCP port")?;
        let server = Arc::new(server);
        let (sender, receiver) = oneshot::channel();
        std::thread::Builder::new()
            .name("windsurf-extension-server".to_string())
            .spawn({
                let server = server.clone();
                move || {
                    let mut sender = Some(sender);
                    for request in server.incoming_requests() {
                        handle_extension_request(request, &csrf_token, &mut sender);
                    }
                }
            })
            .map_err(|error| format!("couldn't start the extension server: {error}"))?;
        Ok((Self { server, port }, receiver))
    }
}

impl Drop for ExtensionServer {
    fn drop(&mut self) {
        self.server.unblock();
    }
}

fn handle_extension_request(
    mut request: tiny_http::Request,
    csrf_token: &str,
    started: &mut Option<oneshot::Sender<LanguageServerStartedRequest>>,
) {
    let header = |name: &'static str| {
        request
            .headers()
            .iter()
            .find(|header| header.field.equiv(name))
            .map(|header| header.value.as_str().to_string())
    };
    let authorized = header(CSRF_HEADER).as_deref() == Some(csrf_token);
    let content_type = header("Content-Type").unwrap_or_else(|| "application/proto".to_string());
    if !authorized {
        log::warn!(
            "windsurf: refused a call to the extension server without the CSRF token: {}",
            request.url()
        );
        request
            .respond(tiny_http::Response::empty(403))
            .map_err(|error| log::debug!("windsurf: extension server: {error}"))
            .ok();
        return;
    }
    let mut body = Vec::new();
    if let Err(error) = request.as_reader().read_to_end(&mut body) {
        log::debug!("windsurf: extension server: reading a request failed: {error}");
    }
    if request.url().ends_with(STARTED_PATH_SUFFIX) {
        match LanguageServerStartedRequest::decode(body.as_slice()) {
            Ok(message) => {
                if let Some(sender) = started.take() {
                    sender.send(message).ok();
                }
            }
            Err(error) => log::warn!("windsurf: couldn't decode LanguageServerStarted: {error}"),
        }
    }
    let payload = if content_type.starts_with("application/connect") {
        encode_frame(b"{}", false, true)
    } else if request.url().ends_with(STARTED_PATH_SUFFIX) {
        LanguageServerStartedResponse::default().encode_to_vec()
    } else {
        Vec::new()
    };
    let mut response = tiny_http::Response::from_data(payload);
    if let Ok(header) = tiny_http::Header::from_bytes("Content-Type", content_type.as_bytes()) {
        response.add_header(header);
    }
    request
        .respond(response)
        .map_err(|error| log::debug!("windsurf: extension server: {error}"))
        .ok();
}

/// The Unix socket the language server watches to notice that Zed is gone.
struct ParentPipe {
    path: PathBuf,
    stopped: Arc<AtomicBool>,
    connections: Arc<Mutex<Vec<UnixStream>>>,
}

impl ParentPipe {
    fn start() -> Result<Self, String> {
        // Socket paths are limited to about 100 bytes, so it goes in the
        // system temporary directory rather than Zed's data directory.
        let path = std::env::temp_dir().join(format!(
            "zed_windsurf_{}",
            &uuid::Uuid::new_v4().simple().to_string()[..16]
        ));
        let listener = UnixListener::bind(&path)
            .map_err(|error| format!("couldn't create {}: {error}", path.display()))?;
        let stopped = Arc::new(AtomicBool::new(false));
        let connections: Arc<Mutex<Vec<UnixStream>>> = Arc::default();
        std::thread::Builder::new()
            .name("windsurf-parent-pipe".to_string())
            .spawn({
                let stopped = stopped.clone();
                let connections = connections.clone();
                move || {
                    for stream in listener.incoming() {
                        if stopped.load(Ordering::SeqCst) {
                            break;
                        }
                        match stream {
                            Ok(stream) => connections.lock().push(stream),
                            Err(error) => {
                                log::debug!("windsurf: parent pipe: {error}");
                                break;
                            }
                        }
                    }
                }
            })
            .map_err(|error| format!("couldn't watch {}: {error}", path.display()))?;
        Ok(Self {
            path,
            stopped,
            connections,
        })
    }
}

impl Drop for ParentPipe {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.connections.lock().clear();
        // Wakes the accepting thread so that it sees `stopped` and exits.
        UnixStream::connect(&self.path)
            .and_then(|mut stream| stream.flush())
            .ok();
        std::fs::remove_file(&self.path).ok();
    }
}
