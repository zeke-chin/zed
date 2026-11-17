//! The language server bridge (stage 8): instead of talking to the cloud
//! directly, run the official Windsurf/Devin language server and let it do
//! the context, history, prefetching and cloud calls. Selected with
//! `fork.windsurf.backend = "language_server"`.
//!
//! One server per Zed process, started on the first request and shared by
//! every window. What Zed does with it, in the order the official extension
//! does it (`fork/spike/lib/ls_driver.py`):
//! - start it with the API key on stdin and wait for it to report its ports
//!   to a local "extension server" (`process.rs`);
//! - LSP `initialize`, then `RefreshContextForIdeAction(STARTUP)`, then
//!   `didOpen` for every tracked document;
//! - `didChange` for every edit; `RefreshContextForIdeAction` with
//!   `ACTIVE_EDITOR_CHANGED` when requests move to another document and
//!   `SELECTION_CHANGED` (500 ms leading debounce) when the cursor moves.
//!   Without these the server records no edits and never answers
//!   (`fork/spike/结论.md` 验收 7);
//! - `HandleStreamingTabV2` per trigger, with a deadline of our own because
//!   the server doesn't answer at all when the cloud returns an error;
//! - `ProvideCompletionFeedback` on accept and reject; `Heartbeat` every second.
//!
//! When the server dies it's started again on the next request; failed starts
//! back off up to a minute. It's stopped when the backend is switched away,
//! the key or binary changes, and when Zed quits; if Zed crashes, the server
//! notices its parent pipe closing and exits by itself.

pub mod binary;
pub mod http;
pub mod lsp;
#[cfg(unix)]
pub mod process;
pub mod request;
#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use futures::{
    FutureExt as _,
    channel::oneshot,
    future::{BoxFuture, Shared},
};
use language::TextBufferSnapshot;
use parking_lot::Mutex;
use prost::Message as _;
use serde_json::{Value, json};

use crate::{
    backend::{
        DocumentContext, FeedbackReport, OtherDocument, TabBackend, TabExchange, TabRequestInput,
    },
    native::{ClientConfig, connect::ConnectError, connect::Timer, get_tab::TabStream, metadata},
    proto::exa::{
        codeium_common_pb::{Metadata, ProviderSource},
        language_server_pb::{
            ExitRequest, HandleStreamingTabV2Response, HeartbeatRequest, IdeAction,
            ProvideCompletionFeedbackRequest,
        },
    },
};
use binary::ProductInfo;

pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;
pub type Spawner = Arc<dyn Fn(BoxFuture<'static, ()>) + Send + Sync>;

pub const HANDLE_STREAMING_TAB: &str = "HandleStreamingTabV2";
pub const REFRESH_CONTEXT: &str = "RefreshContextForIdeAction";
pub const PROVIDE_COMPLETION_FEEDBACK: &str = "ProvideCompletionFeedback";
pub const HEARTBEAT: &str = "Heartbeat";
pub const EXIT: &str = "Exit";

const EXIT_DEADLINE: Duration = Duration::from_millis(500);
const MIN_RESTART_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RESTART_BACKOFF: Duration = Duration::from_secs(60);
const SELECTION_DEBOUNCE: Duration = Duration::from_millis(500);
/// Heartbeats that may fail in a row before the server is considered gone.
const MAX_MISSED_HEARTBEATS: u32 = 3;
/// Total size of the other documents sent with a request (`extension.js`).
pub const MAX_OTHER_DOCUMENTS_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct LanguageServerConfig {
    /// Identity values for the metadata; the IDE version is replaced by the
    /// one of the app the binary comes from.
    pub client: ClientConfig,
    /// Where the server keeps its database and settings, instead of the
    /// `~/.codeium` the official app uses.
    pub data_dir: PathBuf,
    /// How long to wait for `HandleStreamingTabV2`. The server's own cloud
    /// deadline is about 9.3 s; it doesn't answer at all when the cloud fails.
    pub request_timeout: Duration,
    /// Deadline of the other calls.
    pub call_timeout: Duration,
    /// How long a start may take until the server reports its ports.
    pub launch_timeout: Duration,
    /// `None` turns heartbeats off (tests).
    pub heartbeat_interval: Option<Duration>,
}

impl LanguageServerConfig {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            client: ClientConfig::default(),
            data_dir,
            request_timeout: Duration::from_secs(12),
            call_timeout: Duration::from_secs(3),
            launch_timeout: Duration::from_secs(60),
            heartbeat_interval: Some(Duration::from_secs(1)),
        }
    }
}

/// Everything a launcher needs to start a server.
pub struct LaunchOptions {
    pub binary: PathBuf,
    pub product: ProductInfo,
    pub data_dir: PathBuf,
    /// Written to the server's stdin (`--stdin_initial_metadata`); has the key.
    pub initial_metadata: Metadata,
    pub workspace_uris: Vec<String>,
    /// Zed's proxy setting, passed on as `HTTPS_PROXY`.
    pub proxy: Option<String>,
    pub windsurf_version: String,
    pub launch_timeout: Duration,
    pub timer: Timer,
    pub spawner: Spawner,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerInfo {
    pub pid: Option<u32>,
    pub binary: PathBuf,
    /// `codeiumVersion` of the app the binary came with.
    pub version: Option<String>,
    /// The build the server reports in LSP `initialize`.
    pub build: Option<String>,
    pub data_dir: PathBuf,
    pub log_path: Option<PathBuf>,
}

pub trait Launcher: Send + Sync {
    fn launch(
        &self,
        options: LaunchOptions,
    ) -> BoxFuture<'static, Result<Arc<dyn RunningServer>, String>>;
}

/// A started server. `process.rs` implements it for the real binary; tests
/// have an in-memory one.
pub trait RunningServer: Send + Sync {
    fn info(&self) -> ServerInfo;
    /// A unary Connect call to `LanguageServerService/<method>`. No deadline
    /// of its own.
    fn call(
        &self,
        method: &'static str,
        body: Vec<u8>,
    ) -> BoxFuture<'static, Result<Vec<u8>, ConnectError>>;
    /// An LSP notification.
    fn notify(&self, method: &'static str, params: Value);
    fn is_alive(&self) -> bool;
    /// Stops the process now (only ever the process this server started).
    fn terminate(&self);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartError {
    /// No binary; carries what to tell the user.
    NotFound(String),
    Failed(String),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(message) | Self::Failed(message) => formatter.write_str(message),
        }
    }
}

/// For the status bar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerStatus {
    /// Not started yet, or stopped.
    Idle,
    Starting,
    Running(ServerInfo),
    NotFound(String),
    Failed {
        message: String,
        retry_at: Option<Instant>,
    },
}

/// What a start needs from the settings.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ServerSettings {
    pub api_key: Option<String>,
    pub binary_path: Option<String>,
    pub proxy: Option<String>,
}

struct TrackedDocument {
    workspace_uri: String,
    language_id: String,
    snapshot: TextBufferSnapshot,
    /// The generation of the server that has this document open, and the
    /// state it was last told about.
    synced: Option<(u64, TextBufferSnapshot)>,
    version: i32,
}

struct ActiveServer {
    generation: u64,
    server: Arc<dyn RunningServer>,
    info: ServerInfo,
    /// Whether this server saw an edit. Before that it answers nothing
    /// (the cloud rejects an empty history), so no request is sent.
    edited: bool,
    active_uri: Option<String>,
    last_selection_refresh: Option<Instant>,
    workspace_uris: BTreeSet<String>,
}

type StartFuture = Shared<BoxFuture<'static, Result<(), StartError>>>;

#[derive(Default)]
struct State {
    settings: ServerSettings,
    generation: u64,
    server: Option<ActiveServer>,
    starting: Option<StartFuture>,
    failures: u32,
    retry_at: Option<Instant>,
    last_error: Option<StartError>,
    documents: BTreeMap<String, TrackedDocument>,
    workspace_uris: BTreeSet<String>,
    product: Option<ProductInfo>,
}

struct Inner {
    launcher: Arc<dyn Launcher>,
    config: LanguageServerConfig,
    timer: Timer,
    clock: Clock,
    spawner: Spawner,
    state: Mutex<State>,
}

/// The `TabBackend` that goes through the official language server.
#[derive(Clone)]
pub struct LanguageServerBackend(Arc<Inner>);

impl LanguageServerBackend {
    pub fn new(
        launcher: Arc<dyn Launcher>,
        config: LanguageServerConfig,
        timer: Timer,
        clock: Clock,
        spawner: Spawner,
    ) -> Self {
        Self(Arc::new(Inner {
            launcher,
            config,
            timer,
            clock,
            spawner,
            state: Mutex::default(),
        }))
    }

    pub fn status(&self) -> ServerStatus {
        let state = self.0.state.lock();
        if let Some(active) = &state.server {
            return ServerStatus::Running(active.info.clone());
        }
        if state.starting.is_some() {
            return ServerStatus::Starting;
        }
        match &state.last_error {
            Some(StartError::NotFound(message)) => ServerStatus::NotFound(message.clone()),
            Some(StartError::Failed(message)) => ServerStatus::Failed {
                message: message.clone(),
                retry_at: state.retry_at,
            },
            None => ServerStatus::Idle,
        }
    }

    pub fn info(&self) -> Option<ServerInfo> {
        self.0
            .state
            .lock()
            .server
            .as_ref()
            .map(|active| active.info.clone())
    }

    /// Applies the settings a start depends on; a running server is stopped
    /// when they change and the next request starts a new one.
    pub fn set_settings(&self, settings: ServerSettings) {
        let stopped = {
            let mut state = self.0.state.lock();
            if state.settings == settings {
                return;
            }
            state.settings = settings;
            state.failures = 0;
            state.retry_at = None;
            state.last_error = None;
            state.product = None;
            take_server(&mut state)
        };
        if let Some(stopped) = stopped {
            log::info!("windsurf: settings changed; stopping the language server");
            (self.0.spawner)(self.clone().stop_server(stopped, EXIT_DEADLINE).boxed());
        }
    }

    /// Stops the server (when running) and clears the backoff. With
    /// `start_again`, a new one is started right away.
    pub fn restart(&self, start_again: bool) -> BoxFuture<'static, ()> {
        let stopped = {
            let mut state = self.0.state.lock();
            state.failures = 0;
            state.retry_at = None;
            state.last_error = None;
            state.product = None;
            take_server(&mut state)
        };
        let this = self.clone();
        async move {
            if let Some(stopped) = stopped {
                this.clone().stop_server(stopped, EXIT_DEADLINE).await;
            }
            if start_again && let Err(error) = this.ensure_started().await {
                log::warn!("windsurf: couldn't restart the language server: {error}");
            }
        }
        .boxed()
    }

    /// Stops the server for good (switching backends).
    pub fn shutdown(&self) -> BoxFuture<'static, ()> {
        self.shutdown_within(EXIT_DEADLINE)
    }

    /// Like `shutdown`, waiting at most `exit_deadline` for the server to
    /// confirm `Exit` before ending the process (quitting Zed).
    pub fn shutdown_within(&self, exit_deadline: Duration) -> BoxFuture<'static, ()> {
        let stopped = take_server(&mut self.0.state.lock());
        let this = self.clone();
        async move {
            if let Some(stopped) = stopped {
                this.stop_server(stopped, exit_deadline).await;
            }
        }
        .boxed()
    }

    /// Ends the process immediately, without asking it to exit.
    pub fn kill(&self) {
        if let Some(stopped) = take_server(&mut self.0.state.lock()) {
            stopped.terminate();
        }
    }

    async fn stop_server(self, server: Arc<dyn RunningServer>, exit_deadline: Duration) {
        let exit = server.call(EXIT, ExitRequest::default().encode_to_vec());
        if let Err(error) = self.with_deadline(exit_deadline, exit).await {
            log::debug!("windsurf: the language server didn't confirm exiting: {error}");
        }
        server.terminate();
        log::info!("windsurf: stopped the language server");
    }

    /// Tells the server about a document's current text: `didOpen` the first
    /// time, `didChange` with the edits since then afterwards.
    pub fn update_document(
        &self,
        uri: &str,
        workspace_uri: &str,
        language_id: &str,
        snapshot: TextBufferSnapshot,
    ) {
        let mut state = self.0.state.lock();
        let state = &mut *state;
        if state.workspace_uris.insert(workspace_uri.to_string())
            && let Some(active) = &mut state.server
        {
            add_workspace(active, workspace_uri);
        }
        let document = state
            .documents
            .entry(uri.to_string())
            .or_insert_with(|| TrackedDocument {
                workspace_uri: workspace_uri.to_string(),
                language_id: language_id.to_string(),
                snapshot: snapshot.clone(),
                synced: None,
                version: 0,
            });
        document.snapshot = snapshot;
        document.language_id = language_id.to_string();
        if let Some(active) = &mut state.server {
            sync_document(active, uri, document);
        }
    }

    pub fn close_document(&self, uri: &str) {
        let mut state = self.0.state.lock();
        let Some(document) = state.documents.remove(uri) else {
            return;
        };
        if let Some(active) = &state.server
            && document
                .synced
                .as_ref()
                .is_some_and(|(generation, _)| *generation == active.generation)
        {
            active
                .server
                .notify("textDocument/didClose", request::did_close_params(uri));
        }
    }

    /// `SELECTION_CHANGED`, at most every 500 ms (leading edge), only while a
    /// server runs. `document` is only called when it's sent.
    pub fn selection_changed(&self, document: impl FnOnce() -> Option<DocumentContext>) {
        let now = (self.0.clock)();
        let (server, workspace_uris) = {
            let mut state = self.0.state.lock();
            let state = &mut *state;
            let Some(active) = &mut state.server else {
                return;
            };
            if active
                .last_selection_refresh
                .is_some_and(|last| now.saturating_duration_since(last) < SELECTION_DEBOUNCE)
            {
                return;
            }
            active.last_selection_refresh = Some(now);
            (
                active.server.clone(),
                state.workspace_uris.iter().cloned().collect::<Vec<_>>(),
            )
        };
        let Some(document) = document() else {
            return;
        };
        let request = request::refresh_request(
            self.metadata(None),
            IdeAction::SelectionChanged,
            Some(&document),
            &[],
            workspace_uris,
        );
        let this = self.clone();
        (self.0.spawner)(
            async move {
                let call = server.call(REFRESH_CONTEXT, request.encode_to_vec());
                if let Err(error) = this.with_deadline(this.0.config.call_timeout, call).await {
                    log::debug!("windsurf: SELECTION_CHANGED failed: {error}");
                }
            }
            .boxed(),
        );
    }

    fn metadata(&self, trigger_id: Option<&str>) -> Metadata {
        let state = self.0.state.lock();
        let api_key = state.settings.api_key.clone().unwrap_or_default();
        self.metadata_for_state(&state, &api_key, trigger_id)
    }

    /// The official extension's metadata, with the versions of the app the
    /// binary came from when known.
    fn metadata_for_state(
        &self,
        state: &State,
        api_key: &str,
        trigger_id: Option<&str>,
    ) -> Metadata {
        let mut metadata = metadata(&self.0.config.client, api_key, None, trigger_id);
        if let Some(product) = &state.product {
            if let Some(version) = &product.windsurf_version {
                metadata.ide_version = version.clone();
            }
            if let Some(version) = &product.codeium_version {
                metadata.extension_version = version.clone();
            }
            if let Some(app_root) = &product.app_root {
                metadata.extension_path = app_root
                    .join("extensions/windsurf")
                    .to_string_lossy()
                    .into_owned();
            }
        }
        metadata
    }

    async fn with_deadline<T>(
        &self,
        timeout: Duration,
        call: impl Future<Output = Result<T, ConnectError>>,
    ) -> Result<T, ConnectError> {
        let deadline = (self.0.timer)(timeout).fuse();
        let call = call.fuse();
        futures::pin_mut!(deadline, call);
        futures::select_biased! {
            result = call => result,
            _ = deadline => Err(ConnectError::Network(format!(
                "the language server didn't answer within {} ms",
                timeout.as_millis()
            ))),
        }
    }

    /// Returns the running server, starting it if needed. Concurrent callers
    /// share one start, which keeps going even when they're dropped.
    pub fn ensure_started(&self) -> BoxFuture<'static, Result<Arc<dyn RunningServer>, StartError>> {
        let this = self.clone();
        async move {
            loop {
                let starting = {
                    let mut state = this.0.state.lock();
                    if let Some(active) = &state.server {
                        if active.server.is_alive() {
                            return Ok(active.server.clone());
                        }
                        log::warn!(
                            "windsurf: the language server (pid {:?}) exited; starting it again",
                            active.info.pid
                        );
                        if let Some(dead) = take_server(&mut state) {
                            dead.terminate();
                        }
                    }
                    if let Some(starting) = state.starting.clone() {
                        starting
                    } else {
                        let now = (this.0.clock)();
                        if let Some(retry_at) = state.retry_at
                            && now < retry_at
                            && let Some(error) = state.last_error.clone()
                        {
                            return Err(error);
                        }
                        let starting = this.start(&mut state);
                        state.starting = Some(starting.clone());
                        starting
                    }
                };
                starting.await?;
            }
        }
        .boxed()
    }

    fn record_failure(&self, state: &mut State, error: StartError) {
        state.failures += 1;
        let backoff = MIN_RESTART_BACKOFF
            .saturating_mul(1 << state.failures.saturating_sub(1).min(6))
            .min(MAX_RESTART_BACKOFF);
        state.retry_at = Some((self.0.clock)() + backoff);
        state.last_error = Some(error);
    }

    fn start(&self, state: &mut State) -> StartFuture {
        let (sender, receiver) = oneshot::channel();
        let starting = receiver
            .map(|result| {
                result.unwrap_or_else(|_| {
                    Err(StartError::Failed(
                        "the language server start was cancelled".to_string(),
                    ))
                })
            })
            .boxed()
            .shared();
        let launch = self.launch_options(state);
        let this = self.clone();
        (self.0.spawner)(
            async move {
                let result = match launch {
                    Ok(options) => this.clone().launch(options).await,
                    Err(error) => Err(error),
                };
                let result = {
                    let mut state = this.0.state.lock();
                    state.starting = None;
                    match result {
                        Ok(server) => {
                            state.failures = 0;
                            state.retry_at = None;
                            state.last_error = None;
                            state.generation += 1;
                            let generation = state.generation;
                            let info = server.info();
                            log::info!(
                                "windsurf: language server started (pid {:?}, {}, version {})",
                                info.pid,
                                info.binary.display(),
                                info.version.as_deref().unwrap_or("unknown")
                            );
                            let mut active = ActiveServer {
                                generation,
                                server,
                                info,
                                edited: false,
                                active_uri: None,
                                last_selection_refresh: None,
                                workspace_uris: state.workspace_uris.clone(),
                            };
                            for (uri, document) in &mut state.documents {
                                sync_document(&mut active, uri, document);
                            }
                            state.server = Some(active);
                            this.start_heartbeat(generation);
                            Ok(())
                        }
                        Err(error) => {
                            log::warn!("windsurf: couldn't start the language server: {error}");
                            this.record_failure(&mut state, error.clone());
                            Err(error)
                        }
                    }
                };
                sender.send(result).ok();
            }
            .boxed(),
        );
        starting
    }

    fn launch_options(&self, state: &mut State) -> Result<LaunchOptions, StartError> {
        let api_key = state
            .settings
            .api_key
            .clone()
            .ok_or_else(|| StartError::Failed("no API key".to_string()))?;
        let binary = binary::locate(state.settings.binary_path.as_deref())
            .map_err(|error| StartError::NotFound(error.to_string()))?;
        let product = binary::product_info(&binary);
        state.product = Some(product.clone());
        let windsurf_version = product
            .windsurf_version
            .clone()
            .unwrap_or_else(|| self.0.config.client.ide_version.to_string());
        Ok(LaunchOptions {
            binary,
            product,
            data_dir: self.0.config.data_dir.clone(),
            initial_metadata: self.metadata_for_state(state, &api_key, None),
            workspace_uris: state.workspace_uris.iter().cloned().collect(),
            proxy: state.settings.proxy.clone(),
            windsurf_version,
            launch_timeout: self.0.config.launch_timeout,
            timer: self.0.timer.clone(),
            spawner: self.0.spawner.clone(),
        })
    }

    async fn launch(self, options: LaunchOptions) -> Result<Arc<dyn RunningServer>, StartError> {
        let server = self
            .0
            .launcher
            .launch(options)
            .await
            .map_err(StartError::Failed)?;
        let startup = request::refresh_request(
            self.metadata(None),
            IdeAction::Startup,
            None,
            &[],
            self.0.state.lock().workspace_uris.iter().cloned().collect(),
        );
        let call = server.call(REFRESH_CONTEXT, startup.encode_to_vec());
        if let Err(error) = self.with_deadline(self.0.config.call_timeout, call).await {
            server.terminate();
            return Err(StartError::Failed(format!(
                "the language server didn't accept STARTUP: {error}"
            )));
        }
        Ok(server)
    }

    fn start_heartbeat(&self, generation: u64) {
        let Some(interval) = self.0.config.heartbeat_interval else {
            return;
        };
        let this = self.clone();
        (self.0.spawner)(
            async move {
                let mut missed = 0;
                loop {
                    (this.0.timer)(interval).await;
                    let server = {
                        let state = this.0.state.lock();
                        match &state.server {
                            Some(active) if active.generation == generation => {
                                active.server.clone()
                            }
                            _ => return,
                        }
                    };
                    let request = HeartbeatRequest {
                        metadata: Some(this.metadata(None)),
                    };
                    let call = server.call(HEARTBEAT, request.encode_to_vec());
                    match this.with_deadline(this.0.config.call_timeout, call).await {
                        Ok(_) => missed = 0,
                        Err(error) => {
                            missed += 1;
                            log::debug!("windsurf: language server heartbeat failed: {error}");
                        }
                    }
                    if missed >= MAX_MISSED_HEARTBEATS || !server.is_alive() {
                        let mut state = this.0.state.lock();
                        if state
                            .server
                            .as_ref()
                            .is_some_and(|active| active.generation == generation)
                        {
                            log::warn!(
                                "windsurf: the language server stopped answering; it's started again on the next request"
                            );
                            if let Some(dead) = take_server(&mut state) {
                                dead.terminate();
                            }
                        }
                        return;
                    }
                }
            }
            .boxed(),
        );
    }

    async fn exchange(self, input: TabRequestInput) -> TabExchange {
        let started_at = Instant::now();
        let failed = |error: ConnectError, described_request: Option<Value>| TabExchange {
            request: None,
            result: Err(error),
            elapsed: started_at.elapsed(),
            first_delta_after: None,
            request_bytes: 0,
            resent_full_trajectory: false,
            described_request,
        };
        let server = match self.ensure_started().await {
            Ok(server) => server,
            Err(StartError::NotFound(message)) => {
                return failed(
                    ConnectError::Server {
                        code: "language_server_not_found".to_string(),
                        message,
                    },
                    None,
                );
            }
            Err(StartError::Failed(message)) => {
                return failed(
                    ConnectError::Network(format!("the language server isn't running: {message}")),
                    None,
                );
            }
        };

        let (edited, switch_to) = {
            let mut state = self.0.state.lock();
            let state = &mut *state;
            let Some(active) = &mut state.server else {
                return failed(
                    ConnectError::Network("the language server stopped".to_string()),
                    None,
                );
            };
            for (uri, document) in &mut state.documents {
                sync_document(active, uri, document);
            }
            let switch_to = (active.active_uri.as_deref() != Some(&input.document.absolute_uri))
                .then(|| state.workspace_uris.iter().cloned().collect::<Vec<_>>());
            if switch_to.is_some() {
                active.active_uri = Some(input.document.absolute_uri.clone());
            }
            (active.edited, switch_to)
        };
        if let Some(workspace_uris) = switch_to {
            let refresh = request::refresh_request(
                self.metadata(None),
                IdeAction::ActiveEditorChanged,
                Some(&input.document),
                &input.language_server.other_documents,
                workspace_uris,
            );
            let call = server.call(REFRESH_CONTEXT, refresh.encode_to_vec());
            if let Err(error) = self.with_deadline(self.0.config.call_timeout, call).await {
                log::debug!("windsurf: ACTIVE_EDITOR_CHANGED failed: {error}");
            }
        }
        if !edited {
            return TabExchange {
                request: None,
                result: Ok(TabStream {
                    noop: true,
                    filter_reason: Some("no edit since the language server started".to_string()),
                    ..Default::default()
                }),
                elapsed: started_at.elapsed(),
                first_delta_after: None,
                request_bytes: 0,
                resent_full_trajectory: false,
                described_request: None,
            };
        }

        let tab_request = request::tab_request(self.metadata(Some(&input.trigger_id)), &input);
        let described_request = Some(request::redacted_tab_request(&tab_request));
        let body = tab_request.encode_to_vec();
        let request_bytes = body.len();
        let call = server.call(HANDLE_STREAMING_TAB, body);
        let result = self
            .with_deadline(self.0.config.request_timeout, call)
            .await
            .and_then(|bytes| {
                HandleStreamingTabV2Response::decode(bytes.as_slice()).map_err(|error| {
                    ConnectError::Protocol(format!("decoding HandleStreamingTabV2: {error}"))
                })
            })
            .map(request::tab_stream);
        TabExchange {
            request: None,
            result,
            elapsed: started_at.elapsed(),
            first_delta_after: None,
            request_bytes,
            resent_full_trajectory: false,
            described_request,
        }
    }

    /// `ProvideCompletionFeedback`. Not sent when no server runs: a new one
    /// wouldn't know the completion.
    pub fn provide_completion_feedback(
        &self,
        report: FeedbackReport,
        document: Option<DocumentContext>,
    ) -> BoxFuture<'static, Result<(), ConnectError>> {
        let server = self
            .0
            .state
            .lock()
            .server
            .as_ref()
            .map(|active| active.server.clone());
        let request = ProvideCompletionFeedbackRequest {
            metadata: Some(self.metadata(None)),
            is_accepted: report.accepted,
            feedback_delay_ms: report.feedback_delay_ms,
            completion_id: report.completion_id,
            prompt_id: report.prompt_id,
            source: ProviderSource::Supercomplete as i32,
            document: document.as_ref().map(request::document),
            is_intentional_reject: !report.accepted,
            ..Default::default()
        };
        let this = self.clone();
        async move {
            let Some(server) = server else {
                return Err(ConnectError::Network(
                    "the language server isn't running".to_string(),
                ));
            };
            let call = server.call(PROVIDE_COMPLETION_FEEDBACK, request.encode_to_vec());
            this.with_deadline(this.0.config.call_timeout, call)
                .await
                .map(|_| ())
        }
        .boxed()
    }

    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.0.state.lock().generation
    }
}

fn take_server(state: &mut State) -> Option<Arc<dyn RunningServer>> {
    state.starting = None;
    state.server.take().map(|active| active.server)
}

fn add_workspace(active: &mut ActiveServer, workspace_uri: &str) {
    if active.workspace_uris.insert(workspace_uri.to_string()) {
        active.server.notify(
            "workspace/didChangeWorkspaceFolders",
            json!({ "event": { "added": [request::workspace_folder(workspace_uri)], "removed": [] } }),
        );
    }
}

fn sync_document(active: &mut ActiveServer, uri: &str, document: &mut TrackedDocument) {
    add_workspace(active, &document.workspace_uri);
    match &document.synced {
        Some((generation, previous)) if *generation == active.generation => {
            if previous.version() == document.snapshot.version() {
                return;
            }
            let changes = request::content_changes(previous, &document.snapshot);
            document.synced = Some((active.generation, document.snapshot.clone()));
            if changes.is_empty() {
                return;
            }
            document.version += 1;
            active.edited = true;
            active.server.notify(
                "textDocument/didChange",
                request::did_change_params(uri, document.version, changes),
            );
        }
        _ => {
            document.version = 1;
            active.server.notify(
                "textDocument/didOpen",
                request::did_open_params(
                    uri,
                    &document.language_id,
                    document.version,
                    document.snapshot.text(),
                ),
            );
            document.synced = Some((active.generation, document.snapshot.clone()));
        }
    }
}

impl TabBackend for LanguageServerBackend {
    fn get_tab(&self, _api_key: String, input: TabRequestInput) -> BoxFuture<'static, TabExchange> {
        self.clone().exchange(input).boxed()
    }

    fn forget_credentials(&self) -> BoxFuture<'static, ()> {
        futures::future::ready(()).boxed()
    }

    fn provide_feedback(
        &self,
        _api_key: String,
        report: FeedbackReport,
    ) -> BoxFuture<'static, Result<(), ConnectError>> {
        self.provide_completion_feedback(report, None)
    }
}

/// Other open documents for a request, the most relevant first, until
/// `MAX_OTHER_DOCUMENTS_BYTES`.
pub fn limit_other_documents(documents: Vec<OtherDocument>) -> Vec<OtherDocument> {
    let mut total = 0;
    documents
        .into_iter()
        .filter(|document| {
            total += document.text.len();
            total <= MAX_OTHER_DOCUMENTS_BYTES
        })
        .collect()
}
