pub mod backend;
pub mod clipboard;
pub mod context;
mod credentials;
mod delegate;
pub mod diff;
pub mod feedback;
pub mod language_server;
pub mod native;
mod prefetch;
pub mod proto;
mod status;
pub mod tab_jump;
pub mod trajectory;
pub mod trajectory_cache;

#[cfg(test)]
mod tests;

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use collections::{HashMap, HashSet};
use editor::{EditPredictionRequestTrigger, Editor};
use futures::{FutureExt as _, future::BoxFuture};
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, Global, Subscription, WeakEntity, Window,
    actions,
};
use language::{
    Anchor, Buffer, BufferEvent, BufferSnapshot, ToOffset as _,
    language_settings::{EditPredictionProvider, LanguageSettings, all_language_settings},
};
use settings::{RegisterSetting, Settings, SettingsContent, SettingsStore, WindsurfBackend};
use workspace::Workspace;

use backend::{
    FeedbackReport, LanguageServerContext, OtherDocument, TabBackend, TabExchange, TabRequestInput,
};
use clipboard::{CLIPBOARD_WINDOW, PendingCopy};
use context::SelectionWindow;
pub use credentials::{ImportApiKey, OpenApiKeySetting};
pub use delegate::WindsurfDelegate;
use feedback::{DocumentInfo, MAX_FEEDBACK_ENTRIES, MIN_REPORT_INTERVAL, PredictionRecord};
use language_server::{
    LanguageServerBackend, LanguageServerConfig, ServerSettings, ServerStatus,
    limit_other_documents,
};
use native::{ClientConfig, NativeBackend, connect::ConnectError};
use proto::exa::{
    codeium_common_pb::SupercompleteTriggerCondition, prompt_pb::SupercompleteFeedback,
};
use trajectory::Trajectory;
use trajectory_cache::TrajectoryCache;

actions!(
    windsurf,
    [
        /// Opens the most recent Windsurf request and its response in a new tab.
        ShowLastRequest,
        /// Turns Windsurf edit predictions on or off.
        ToggleEnabled,
        /// Stops the Windsurf language server and starts it again (language server backend).
        RestartLanguageServer,
        /// Shows the path, version and data directory of the Windsurf language server.
        ShowLanguageServerInfo,
    ]
);

#[derive(Clone, PartialEq, RegisterSetting)]
pub struct WindsurfSettings {
    pub enabled: bool,
    pub backend: WindsurfBackend,
    /// Only ever comes from user settings: project settings files don't accept the
    /// `fork` key, so a key committed to a project repository is never read.
    pub api_key: Option<String>,
    pub tab_jump: bool,
    pub language_server_path: Option<String>,
}

impl Settings for WindsurfSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let windsurf = content
            .fork
            .as_ref()
            .and_then(|fork| fork.windsurf.as_ref());
        Self {
            enabled: windsurf
                .and_then(|windsurf| windsurf.enabled)
                .unwrap_or(true),
            backend: windsurf
                .and_then(|windsurf| windsurf.backend)
                .unwrap_or_default(),
            api_key: windsurf
                .and_then(|windsurf| windsurf.api_key.clone())
                .filter(|api_key| !api_key.trim().is_empty()),
            tab_jump: windsurf
                .and_then(|windsurf| windsurf.tab_jump)
                .unwrap_or(true),
            language_server_path: windsurf
                .and_then(|windsurf| windsurf.language_server_path.clone())
                .filter(|path| !path.trim().is_empty()),
        }
    }
}

impl std::fmt::Debug for WindsurfSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WindsurfSettings")
            .field("enabled", &self.enabled)
            .field("backend", &self.backend)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("tab_jump", &self.tab_jump)
            .field("language_server_path", &self.language_server_path)
            .finish()
    }
}

const MAX_BACKOFF: Duration = Duration::from_secs(60);
const MIN_BACKOFF: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// `fork.windsurf.enabled` is false.
    Disabled,
    MissingApiKey,
    Ready,
    Requesting,
    /// The language server backend is starting the server.
    StartingLanguageServer,
    Error(ErrorStatus),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErrorStatus {
    /// The key was rejected; no requests are sent until it changes.
    Unauthenticated(String),
    RateLimited {
        message: String,
        retry_in: Duration,
    },
    Network {
        message: String,
        retry_in: Duration,
    },
    Protocol(String),
    Server(String),
    /// No language server binary (language server backend).
    LanguageServerNotFound(String),
    LanguageServerFailed {
        message: String,
        retry_in: Duration,
    },
    /// The language server backend can't serve files of a remote project.
    RemoteProject,
}

pub(crate) enum RequestOutcome {
    Predicted(usize),
    /// The language server predicted a jump to where the next edit is.
    TabJump,
    NoPrediction,
    Discarded(&'static str),
    /// A predictive response, kept until the buffer reaches the state it assumed.
    Prefetched,
    Failed,
}

pub(crate) enum Preparation {
    Send(PreparedRequest, BufferSnapshot),
    WaitUntil(Instant),
    Skip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestKind {
    /// Sent for a trigger (typing, an explicit request, …).
    Trigger,
    /// Sent right after a prediction, as if it had been accepted.
    Predictive,
}

impl RequestKind {
    fn name(self) -> &'static str {
        match self {
            Self::Trigger => "trigger",
            Self::Predictive => "predictive",
        }
    }
}

pub(crate) struct PreparedRequest {
    pub backend: Arc<dyn TabBackend>,
    pub api_key: String,
    pub input: TabRequestInput,
    pub window: SelectionWindow,
    pub abs_path: String,
    pub details: RequestDetails,
}

/// What `finish_request` needs to know about a request besides its result.
#[derive(Clone)]
pub(crate) struct RequestDetails {
    pub kind: RequestKind,
    /// The call that was made, for the log.
    pub method: &'static str,
    /// Fingerprints of the steps sent, to match the returned hashes.
    pub fingerprints: Vec<u64>,
    pub cached_steps: usize,
    pub context_elapsed: Duration,
    pub document: DocumentInfo,
}

/// Counters for the log and `windsurf: show last request`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// `GetTab` requests sent for triggers.
    pub requests: u64,
    /// Predictive `GetTab` requests.
    pub predictive_requests: u64,
    /// Triggers answered by trimming the shown prediction locally because
    /// the user typed along it.
    pub local_cache_hits: u64,
    /// Triggers answered by a predictive request.
    pub predictive_hits: u64,
    /// `GetTab` requests resent in full after `MissingInformation`.
    pub full_resends: u64,
    pub feedback_reports: u64,
}

impl Stats {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "requests": self.requests,
            "predictive_requests": self.predictive_requests,
            "local_cache_hits": self.local_cache_hits,
            "predictive_hits": self.predictive_hits,
            "full_resends": self.full_resends,
            "feedback_reports": self.feedback_reports,
        })
    }
}

/// The most recent request, for `windsurf: show last request`.
pub struct LastRequest {
    pub sent_at: SystemTime,
    pub request: Option<serde_json::Value>,
    pub response: serde_json::Value,
}

/// Creates the language server backend when it gets selected. Injected so
/// that tests can use a fake server.
pub type LanguageServerFactory = Arc<dyn Fn(&App) -> LanguageServerBackend>;

/// The language server backend while it's selected.
struct LanguageServerState {
    backend: LanguageServerBackend,
    /// Buffers whose edits are passed on, with their URI.
    documents: HashMap<EntityId, TrackedDocument>,
}

struct TrackedDocument {
    uri: Option<String>,
    _subscriptions: [Subscription; 2],
}

/// A Tab Jump from the language server waiting for the cursor to arrive at
/// its target, which triggers the request for the edit there.
#[derive(Clone)]
pub(crate) struct PendingJump {
    pub buffer: WeakEntity<Buffer>,
    pub target: Anchor,
    pub completion_id: String,
    pub prompt_id: String,
    pub shown_at: Instant,
}

/// State shared by every editor's delegate: the edit history, error and
/// backoff state, and what the status bar shows.
pub struct Windsurf {
    backend: Arc<dyn TabBackend>,
    language_server_factory: LanguageServerFactory,
    language_server: Option<LanguageServerState>,
    /// The last request was skipped because the file is in a remote project,
    /// which the language server can't serve.
    remote_project_skipped: bool,
    pending_jump: Option<PendingJump>,
    trajectory: Trajectory,
    trajectory_cache: TrajectoryCache,
    pending_copy: Option<PendingCopy>,
    last_copied: Option<String>,
    feedback: VecDeque<SupercompleteFeedback>,
    last_feedback_report: Option<Instant>,
    stats: Stats,
    /// Whether `fork.edit_prediction_provider` selects Windsurf.
    selected: bool,
    next_request_id: u64,
    in_flight: HashSet<u64>,
    error: Option<ConnectError>,
    retry_at: Option<Instant>,
    backoff: Duration,
    last_request: Option<LastRequest>,
    applied_settings: Option<(bool, Option<String>, WindsurfBackend)>,
}

struct GlobalWindsurf(Entity<Windsurf>);

impl Global for GlobalWindsurf {}

impl Windsurf {
    pub fn new(
        backend: Arc<dyn TabBackend>,
        language_server_factory: LanguageServerFactory,
    ) -> Self {
        Self {
            backend,
            language_server_factory,
            language_server: None,
            remote_project_skipped: false,
            pending_jump: None,
            trajectory: Trajectory::default(),
            trajectory_cache: TrajectoryCache::default(),
            pending_copy: None,
            last_copied: None,
            feedback: VecDeque::new(),
            last_feedback_report: None,
            stats: Stats::default(),
            selected: true,
            next_request_id: 0,
            in_flight: HashSet::default(),
            error: None,
            retry_at: None,
            backoff: Duration::ZERO,
            last_request: None,
            applied_settings: None,
        }
    }

    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalWindsurf>()
            .map(|global| global.0.clone())
    }

    pub fn last_request(&self) -> Option<&LastRequest> {
        self.last_request.as_ref()
    }

    pub fn trajectory(&self) -> &Trajectory {
        &self.trajectory
    }

    pub(crate) fn backend(&self) -> Arc<dyn TabBackend> {
        self.backend.clone()
    }

    /// The language server backend, while it's selected.
    pub fn language_server(&self) -> Option<&LanguageServerBackend> {
        self.language_server.as_ref().map(|state| &state.backend)
    }

    pub(crate) fn uses_language_server(&self) -> bool {
        self.language_server.is_some()
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn cached_hash_count(&self) -> usize {
        self.trajectory_cache.len()
    }

    pub fn feedback_context(&self) -> Vec<SupercompleteFeedback> {
        self.feedback.iter().cloned().collect()
    }

    pub(crate) fn count_local_cache_hit(&mut self) {
        self.stats.local_cache_hits += 1;
    }

    pub(crate) fn count_predictive_hit(&mut self) {
        self.stats.predictive_hits += 1;
    }

    pub fn status(&self, cx: &App) -> Status {
        let settings = WindsurfSettings::get_global(cx);
        if !settings.enabled {
            return Status::Disabled;
        }
        if settings.api_key.is_none() {
            return Status::MissingApiKey;
        }
        let now = cx.background_executor().now();
        if let Some(state) = &self.language_server {
            if self.remote_project_skipped {
                return Status::Error(ErrorStatus::RemoteProject);
            }
            match state.backend.status() {
                ServerStatus::NotFound(message) => {
                    return Status::Error(ErrorStatus::LanguageServerNotFound(message));
                }
                ServerStatus::Failed { message, retry_at } => {
                    return Status::Error(ErrorStatus::LanguageServerFailed {
                        message,
                        retry_in: retry_at
                            .map(|retry_at| retry_at.saturating_duration_since(now))
                            .unwrap_or_default(),
                    });
                }
                ServerStatus::Starting => return Status::StartingLanguageServer,
                ServerStatus::Idle | ServerStatus::Running(_) => {}
            }
        }
        let retry_in = self
            .retry_at
            .map(|retry_at| retry_at.saturating_duration_since(now))
            .unwrap_or_default();
        match &self.error {
            Some(ConnectError::Unauthenticated(message)) => {
                Status::Error(ErrorStatus::Unauthenticated(message.clone()))
            }
            Some(ConnectError::RateLimited(message)) => Status::Error(ErrorStatus::RateLimited {
                message: message.clone(),
                retry_in,
            }),
            Some(ConnectError::Network(message)) => Status::Error(ErrorStatus::Network {
                message: message.clone(),
                retry_in,
            }),
            _ if !self.in_flight.is_empty() => Status::Requesting,
            Some(ConnectError::Protocol(message)) => {
                Status::Error(ErrorStatus::Protocol(message.clone()))
            }
            Some(ConnectError::Server { code, message }) => {
                Status::Error(ErrorStatus::Server(format!("{code}: {message}")))
            }
            None => Status::Ready,
        }
    }

    fn set_selected(&mut self, selected: bool, cx: &mut Context<Self>) {
        if self.selected != selected {
            self.selected = selected;
            if !selected && let Some(state) = &self.language_server {
                cx.background_spawn(state.backend.shutdown()).detach();
            }
            cx.notify();
        }
    }

    /// Whether requests may be sent at all. Errors and backoff are checked
    /// when a request is about to be sent.
    pub fn can_request(&self, cx: &App) -> bool {
        let settings = WindsurfSettings::get_global(cx);
        self.selected && settings.enabled && settings.api_key.is_some()
    }

    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        let settings = WindsurfSettings::get_global(cx).clone();
        let applied = (settings.enabled, settings.api_key.clone(), settings.backend);
        let previous = self.applied_settings.replace(applied.clone());
        if previous.is_some_and(|previous| previous != applied) {
            self.error = None;
            self.retry_at = None;
            self.backoff = Duration::ZERO;
            self.pending_jump = None;
            cx.background_spawn(self.backend.forget_credentials())
                .detach();
            cx.notify();
        }
        let wants_language_server = settings.enabled
            && settings.api_key.is_some()
            && settings.backend == WindsurfBackend::LanguageServer;
        if wants_language_server {
            if self.language_server.is_none() {
                log::info!("windsurf: using the language server backend");
                self.language_server = Some(LanguageServerState {
                    backend: (self.language_server_factory)(cx),
                    documents: HashMap::default(),
                });
                for buffer in self.trajectory.tracked_buffers().collect::<Vec<_>>() {
                    self.track_language_server_document(&buffer, cx);
                }
            }
            if let Some(state) = &self.language_server {
                state.backend.set_settings(ServerSettings {
                    api_key: settings.api_key.clone(),
                    binary_path: settings.language_server_path,
                    proxy: cx.http_client().proxy().map(|proxy| proxy.to_string()),
                });
            }
        } else if let Some(state) = self.language_server.take() {
            log::info!(
                "windsurf: the language server backend is no longer selected; stopping the server"
            );
            self.remote_project_skipped = false;
            cx.background_spawn(state.backend.shutdown()).detach();
            cx.notify();
        }
    }

    /// Stops the language server when Zed quits. Only waits briefly; a server
    /// that's still there exits once its parent pipe closes with Zed.
    fn on_quit(&mut self, _cx: &mut Context<Self>) -> BoxFuture<'static, ()> {
        match self.language_server.take() {
            Some(state) => state.backend.shutdown_within(Duration::from_millis(150)),
            None => futures::future::ready(()).boxed(),
        }
    }

    pub fn restart_language_server(&mut self, cx: &mut Context<Self>) {
        let Some(state) = &self.language_server else {
            return;
        };
        self.error = None;
        self.retry_at = None;
        self.backoff = Duration::ZERO;
        let restart = state.backend.restart(true);
        cx.spawn(async move |this, cx| {
            restart.await;
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
        cx.notify();
    }

    pub fn track_buffer(&mut self, buffer: &Entity<Buffer>, cx: &mut Context<Self>) {
        self.trajectory.track(buffer, cx);
        self.track_language_server_document(buffer, cx);
    }

    /// Passes the buffer's edits on to the language server, while that
    /// backend is selected.
    fn track_language_server_document(&mut self, buffer: &Entity<Buffer>, cx: &mut Context<Self>) {
        let Some(state) = &mut self.language_server else {
            return;
        };
        let buffer_id = buffer.entity_id();
        if state.documents.contains_key(&buffer_id) {
            return;
        }
        let edited = cx.subscribe(buffer, |this, buffer, event: &BufferEvent, cx| {
            if matches!(
                event,
                BufferEvent::Edited { .. } | BufferEvent::Reloaded | BufferEvent::FileHandleChanged
            ) {
                this.sync_language_server_document(&buffer, cx);
            }
        });
        let released = cx.observe_release(buffer, move |this, _, _| {
            if let Some(state) = &mut this.language_server
                && let Some(document) = state.documents.remove(&buffer_id)
                && let Some(uri) = document.uri
            {
                state.backend.close_document(&uri);
            }
        });
        state.documents.insert(
            buffer_id,
            TrackedDocument {
                uri: None,
                _subscriptions: [edited, released],
            },
        );
        self.sync_language_server_document(buffer, cx);
    }

    fn sync_language_server_document(&mut self, buffer: &Entity<Buffer>, cx: &App) {
        let Some(state) = &mut self.language_server else {
            return;
        };
        let buffer_id = buffer.entity_id();
        let buffer = buffer.read(cx);
        if !is_local(buffer, cx) {
            return;
        }
        let Some(location) = context::buffer_location(buffer, cx) else {
            return;
        };
        let uri = location.uri();
        if let Some(document) = state.documents.get_mut(&buffer_id) {
            document.uri = Some(uri.clone());
        }
        let (_, editor_language) = context::language_for(
            buffer
                .language()
                .map(|language| language.name().0.to_string()),
        );
        state.backend.update_document(
            &uri,
            &location.workspace_uri(),
            &editor_language,
            buffer.text_snapshot(),
        );
    }

    /// `SELECTION_CHANGED` for the language server, when the cursor moved.
    pub(crate) fn selection_changed(&self, buffer: &Entity<Buffer>, cursor: Anchor, cx: &App) {
        let Some(state) = &self.language_server else {
            return;
        };
        state.backend.selection_changed(|| {
            let buffer = buffer.read(cx);
            if !is_local(buffer, cx) {
                return None;
            }
            let location = context::buffer_location(buffer, cx)?;
            Some(
                context::document_context(&buffer.snapshot(), cursor, &location, buffer.is_dirty())
                    .document,
            )
        });
    }

    pub(crate) fn set_pending_jump(&mut self, jump: Option<PendingJump>) {
        self.pending_jump = jump;
    }

    /// Takes the pending Tab Jump when the cursor arrived at its target.
    pub(crate) fn take_landed_jump(
        &mut self,
        buffer: &Entity<Buffer>,
        cursor: Anchor,
        cx: &App,
    ) -> Option<PendingJump> {
        let jump = self.pending_jump.as_ref()?;
        if jump.buffer.upgrade().as_ref() != Some(buffer) {
            return None;
        }
        let snapshot = buffer.read(cx).snapshot();
        if jump.target.to_offset(&snapshot) != cursor.to_offset(&snapshot) {
            return None;
        }
        self.pending_jump.take()
    }

    pub(crate) fn next_request_id(&mut self) -> u64 {
        self.next_request_id += 1;
        self.next_request_id
    }

    pub(crate) fn cancel_request(&mut self, request_id: u64, cx: &mut Context<Self>) {
        if self.in_flight.remove(&request_id) {
            cx.notify();
        }
    }

    /// Collects the context for a request. Nothing is sent when Windsurf isn't
    /// configured, the key was rejected, or there are no edits yet (the server
    /// rejects an empty trajectory); while backing off, the caller waits.
    pub(crate) fn prepare_request(
        &mut self,
        request_id: u64,
        buffer: &Entity<Buffer>,
        cursor: Anchor,
        trigger_id: &str,
        trigger: SupercompleteTriggerCondition,
        cx: &mut Context<Self>,
    ) -> Preparation {
        if !self.can_request(cx) || matches!(self.error, Some(ConnectError::Unauthenticated(_))) {
            return Preparation::Skip;
        }
        if let Some(retry_at) = self.retry_at
            && cx.background_executor().now() < retry_at
        {
            return Preparation::WaitUntil(retry_at);
        }
        let Some(api_key) = WindsurfSettings::get_global(cx).api_key.clone() else {
            return Preparation::Skip;
        };
        if self.language_server.is_some() {
            return self.prepare_language_server_request(
                request_id, buffer, cursor, trigger_id, trigger, api_key, cx,
            );
        }
        let started_at = Instant::now();
        self.trajectory.track(buffer, cx);
        self.trajectory.commit(cx);
        self.add_pending_copy(cx);
        if self.trajectory.is_empty() {
            return Preparation::Skip;
        }

        let buffer = buffer.read(cx);
        let Some(location) = context::buffer_location(buffer, cx) else {
            return Preparation::Skip;
        };
        let snapshot = buffer.snapshot();
        let document = context::document_context(&snapshot, cursor, &location, buffer.is_dirty());
        let cursor_row = snapshot.offset_to_point(cursor.to_offset(&snapshot)).row;
        let diagnostics =
            context::diagnostics_near(&snapshot, cursor_row, &document.document.absolute_uri);
        let steps = self.trajectory.steps();
        let prepared = self.prepared_request(
            RequestKind::Trigger,
            api_key,
            document,
            diagnostics,
            steps,
            &location,
            trigger_id,
            started_at,
        );
        self.in_flight.insert(request_id);
        cx.notify();
        Preparation::Send(prepared, snapshot)
    }

    /// The language server keeps the history itself; the request carries the
    /// document, the other open documents, diagnostics and the clipboard.
    #[allow(clippy::too_many_arguments)]
    fn prepare_language_server_request(
        &mut self,
        request_id: u64,
        buffer: &Entity<Buffer>,
        cursor: Anchor,
        trigger_id: &str,
        trigger: SupercompleteTriggerCondition,
        api_key: String,
        cx: &mut Context<Self>,
    ) -> Preparation {
        let started_at = Instant::now();
        let remote = is_remote(buffer.read(cx), cx);
        if remote != self.remote_project_skipped {
            self.remote_project_skipped = remote;
            if remote {
                log::info!(
                    "windsurf: the language server backend doesn't work in remote projects; no predictions there"
                );
            }
            cx.notify();
        }
        if remote {
            return Preparation::Skip;
        }
        self.trajectory.track(buffer, cx);
        self.track_language_server_document(buffer, cx);
        self.sync_language_server_document(buffer, cx);
        let Some(backend) = self.language_server().cloned() else {
            return Preparation::Skip;
        };

        let clipboard = self
            .pending_copy
            .take()
            .filter(|copy| {
                cx.background_executor()
                    .now()
                    .saturating_duration_since(copy.copied_at)
                    <= CLIPBOARD_WINDOW
            })
            .map(|copy| copy.content)
            .unwrap_or_default();
        let other_documents = limit_other_documents(
            self.trajectory
                .tracked_buffers()
                .filter(|tracked| tracked != buffer)
                .filter_map(|tracked| {
                    let tracked = tracked.read(cx);
                    if !is_local(tracked, cx) {
                        return None;
                    }
                    let location = context::buffer_location(tracked, cx)?;
                    let (_, editor_language) = context::language_for(
                        tracked
                            .language()
                            .map(|language| language.name().0.to_string()),
                    );
                    Some(OtherDocument {
                        uri: location.uri(),
                        workspace_uri: location.workspace_uri(),
                        editor_language,
                        text: tracked.text(),
                        is_dirty: tracked.is_dirty(),
                    })
                })
                .collect(),
        );

        let buffer_entity = buffer;
        let buffer = buffer_entity.read(cx);
        let Some(location) = context::buffer_location(buffer, cx) else {
            return Preparation::Skip;
        };
        let snapshot = buffer.snapshot();
        let document = context::document_context(&snapshot, cursor, &location, buffer.is_dirty());
        let cursor_row = snapshot.offset_to_point(cursor.to_offset(&snapshot)).row;
        let diagnostics =
            context::diagnostics_near(&snapshot, cursor_row, &document.document.absolute_uri);
        let language_settings = LanguageSettings::for_buffer(buffer, cx);
        let settings = WindsurfSettings::get_global(cx);
        let language_server = LanguageServerContext {
            trigger,
            clipboard,
            other_documents,
            disable_tab_jump: !settings.tab_jump,
            tab_size: language_settings.tab_size.get(),
            insert_spaces: !language_settings.hard_tabs,
        };
        let details = RequestDetails {
            kind: RequestKind::Trigger,
            method: language_server::HANDLE_STREAMING_TAB,
            fingerprints: Vec::new(),
            cached_steps: 0,
            context_elapsed: started_at.elapsed(),
            document: DocumentInfo::of(&document.document),
        };
        self.stats.requests += 1;
        let prepared = PreparedRequest {
            backend: Arc::new(backend),
            api_key,
            input: TabRequestInput {
                document: document.document,
                diagnostics,
                steps: Vec::new(),
                mainline_trajectory_id: String::new(),
                cached_hashes: Vec::new(),
                feedback: Vec::new(),
                trigger_id: trigger_id.to_string(),
                language_server,
            },
            window: document.window,
            abs_path: location.abs_path.to_string_lossy().into_owned(),
            details,
        };
        self.in_flight.insert(request_id);
        cx.notify();
        Preparation::Send(prepared, snapshot)
    }

    /// Collects the context for a predictive request: the buffer as if
    /// `assumed_text` (the buffer with the prediction applied) were its text,
    /// and the trajectory with the accepting edit as one more step. Returns
    /// `None` instead of waiting when requests can't be sent right now.
    pub(crate) fn prepare_predictive_request(
        &mut self,
        buffer: &Entity<Buffer>,
        assumed_text: &str,
        assumed_cursor: usize,
        trigger_id: &str,
        cx: &mut Context<Self>,
    ) -> Option<PreparedRequest> {
        if !self.can_request(cx)
            || self.error.is_some()
            || self.retry_at.is_some()
            || self.uses_language_server()
        {
            return None;
        }
        let api_key = WindsurfSettings::get_global(cx).api_key.clone()?;
        let started_at = Instant::now();
        let assumed_step = self.trajectory.assumed_step(buffer, assumed_text, cx)?;
        let buffer = buffer.read(cx);
        let location = context::buffer_location(buffer, cx)?;
        let mut steps = self.trajectory.steps();
        steps.push(assumed_step);
        let document = context::document_context_for_text(
            assumed_text.to_string(),
            assumed_cursor,
            buffer
                .language()
                .map(|language| language.name().0.to_string()),
            &location,
            true,
            Some(feedback::row_and_column(assumed_text, assumed_cursor)),
        );
        Some(self.prepared_request(
            RequestKind::Predictive,
            api_key,
            document,
            Vec::new(),
            steps,
            &location,
            trigger_id,
            started_at,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn prepared_request(
        &mut self,
        kind: RequestKind,
        api_key: String,
        document: context::DocumentSnapshot,
        diagnostics: Vec<proto::exa::codeium_common_pb::CodeDiagnostic>,
        steps: Vec<proto::exa::cortex_pb::CortexTrajectoryStep>,
        location: &context::BufferLocation,
        trigger_id: &str,
        started_at: Instant,
    ) -> PreparedRequest {
        let fingerprints: Vec<u64> = steps.iter().map(trajectory_cache::fingerprint).collect();
        let cached_hashes = self.trajectory_cache.hashes_for(&fingerprints);
        let details = RequestDetails {
            kind,
            method: "GetTab",
            cached_steps: cached_hashes.len(),
            fingerprints,
            context_elapsed: started_at.elapsed(),
            document: DocumentInfo::of(&document.document),
        };
        match kind {
            RequestKind::Trigger => self.stats.requests += 1,
            RequestKind::Predictive => self.stats.predictive_requests += 1,
        }
        PreparedRequest {
            backend: self.backend.clone(),
            api_key,
            input: TabRequestInput {
                document: document.document,
                diagnostics,
                steps,
                mainline_trajectory_id: self.trajectory.mainline_id().to_string(),
                cached_hashes,
                feedback: self.feedback_context(),
                trigger_id: trigger_id.to_string(),
                language_server: LanguageServerContext::default(),
            },
            window: document.window,
            abs_path: location.abs_path.to_string_lossy().into_owned(),
            details,
        }
    }

    /// Adds the clipboard step for a copy made within the last five seconds.
    fn add_pending_copy(&mut self, cx: &App) {
        let Some(copy) = self.pending_copy.take() else {
            return;
        };
        let now = cx.background_executor().now();
        if now.saturating_duration_since(copy.copied_at) <= CLIPBOARD_WINDOW {
            self.trajectory.insert(
                copy.insert_at,
                clipboard::clipboard_step(&copy.content, SystemTime::now()),
                cx,
            );
        }
    }

    /// Records a copy or cut. Splits the edits made so far from those that
    /// follow, like other IDE actions.
    pub fn record_copy(&mut self, content: String, cx: &mut Context<Self>) {
        if !clipboard::is_sendable(&content) {
            log::debug!(
                "windsurf: not sending the clipboard: empty, longer than {} characters or looks like a secret",
                clipboard::MAX_CLIPBOARD_CHARS
            );
            self.pending_copy = None;
            self.last_copied = Some(content);
            return;
        }
        self.trajectory.commit(cx);
        self.pending_copy = Some(PendingCopy {
            content: content.clone(),
            copied_at: cx.background_executor().now(),
            insert_at: self.trajectory.len(),
        });
        self.last_copied = Some(content);
    }

    /// Records the clipboard of a paste when it wasn't copied with a key
    /// binding in Zed (e.g. copied in another application). The paste itself
    /// isn't committed yet, so the clipboard step goes before it.
    pub fn record_paste(&mut self, content: String, cx: &mut Context<Self>) {
        if self.last_copied.as_deref() == Some(content.as_str()) {
            return;
        }
        if clipboard::is_sendable(&content) {
            self.pending_copy = Some(PendingCopy {
                content: content.clone(),
                copied_at: cx.background_executor().now(),
                insert_at: self.trajectory.len(),
            });
        }
        self.last_copied = Some(content);
    }

    /// Records that a shown prediction was accepted or intentionally
    /// rejected; see `feedback.rs`. For an acceptance, `before_accept` is the
    /// buffer just before the prediction was applied, so that the user's
    /// typing and the accepted edit become separate steps.
    pub(crate) fn record_feedback(
        &mut self,
        buffer: &Entity<Buffer>,
        before_accept: Option<&BufferSnapshot>,
        record: &PredictionRecord,
        accepted: bool,
        feedback_delay: Duration,
        cx: &mut Context<Self>,
    ) {
        if !self.can_request(cx) {
            return;
        }
        if self.uses_language_server() {
            self.report_language_server_feedback(
                &record.completion_id,
                &record.prompt_id,
                accepted,
                feedback_delay,
                buffer,
                cx,
            );
            return;
        }
        match before_accept {
            Some(snapshot) if accepted => self.trajectory.commit_buffer_at(buffer, snapshot, cx),
            _ => self.trajectory.commit(cx),
        }
        let now = SystemTime::now();
        let feedback_delay_ms = feedback_delay.as_millis() as i64;
        let (text, is_dirty) = {
            let buffer = buffer.read(cx);
            (buffer.text(), buffer.is_dirty())
        };
        self.trajectory.push(
            record.feedback_step(accepted, text, is_dirty, feedback_delay_ms, now),
            cx,
        );
        if accepted {
            self.trajectory.commit(cx);
        }
        self.feedback
            .push_back(record.context_entry(accepted, feedback::unix_millis(now)));
        while self.feedback.len() > MAX_FEEDBACK_ENTRIES {
            self.feedback.pop_front();
        }
        self.report_feedback(record, accepted, feedback_delay_ms, now, cx);
    }

    /// `ProvideCompletionFeedback`: the language server records the feedback
    /// in its own history and reports it to the cloud. Every accept and reject
    /// is sent, like the official extension does.
    pub(crate) fn report_language_server_feedback(
        &mut self,
        completion_id: &str,
        prompt_id: &str,
        accepted: bool,
        feedback_delay: Duration,
        buffer: &Entity<Buffer>,
        cx: &mut Context<Self>,
    ) {
        let Some(backend) = self.language_server().cloned() else {
            return;
        };
        if completion_id.is_empty() {
            return;
        }
        let document = {
            let buffer = buffer.read(cx);
            context::buffer_location(buffer, cx).map(|location| {
                context::document_context_for_text(
                    buffer.text(),
                    0,
                    buffer
                        .language()
                        .map(|language| language.name().0.to_string()),
                    &location,
                    buffer.is_dirty(),
                    None,
                )
                .document
            })
        };
        self.stats.feedback_reports += 1;
        let report = FeedbackReport {
            prompt_id: prompt_id.to_string(),
            completion_id: completion_id.to_string(),
            accepted,
            from_cache: false,
            feedback_delay_ms: feedback_delay.as_millis() as i64,
            timestamp_unix_ms: feedback::unix_millis(SystemTime::now()),
        };
        let call = backend.provide_completion_feedback(report, document);
        cx.background_spawn(async move {
            match call.await {
                Ok(()) => log::debug!("windsurf: ProvideCompletionFeedback sent"),
                Err(error) => log::warn!("windsurf: ProvideCompletionFeedback failed: {error}"),
            }
        })
        .detach();
    }

    fn report_feedback(
        &mut self,
        record: &PredictionRecord,
        accepted: bool,
        feedback_delay_ms: i64,
        now: SystemTime,
        cx: &mut Context<Self>,
    ) {
        let instant = cx.background_executor().now();
        if self
            .last_feedback_report
            .is_some_and(|last| instant.saturating_duration_since(last) < MIN_REPORT_INTERVAL)
        {
            return;
        }
        let Some(api_key) = WindsurfSettings::get_global(cx).api_key.clone() else {
            return;
        };
        if matches!(self.error, Some(ConnectError::Unauthenticated(_))) {
            return;
        }
        self.last_feedback_report = Some(instant);
        self.stats.feedback_reports += 1;
        let report = FeedbackReport {
            prompt_id: record.prompt_id.clone(),
            completion_id: record.completion_id.clone(),
            accepted,
            from_cache: record.from_cache,
            feedback_delay_ms,
            timestamp_unix_ms: feedback::unix_millis(now),
        };
        let call = self.backend.provide_feedback(api_key, report);
        cx.background_spawn(async move {
            match call.await {
                Ok(()) => log::debug!("windsurf: ProvideFeedback sent"),
                Err(error) => log::warn!("windsurf: ProvideFeedback failed: {error}"),
            }
        })
        .detach();
    }

    /// Records the result. Returns when to retry, for rate limits and network
    /// errors.
    pub(crate) fn finish_request(
        &mut self,
        request_id: u64,
        details: &RequestDetails,
        exchange: &TabExchange,
        outcome: RequestOutcome,
        cx: &mut Context<Self>,
    ) -> Option<Instant> {
        self.in_flight.remove(&request_id);
        let outcome_text = match &outcome {
            RequestOutcome::Predicted(edit_count) => format!("predicted {edit_count} edit(s)"),
            RequestOutcome::TabJump => "Tab Jump".to_string(),
            RequestOutcome::NoPrediction => "no prediction".to_string(),
            RequestOutcome::Discarded(reason) => format!("discarded: {reason}"),
            RequestOutcome::Prefetched => "kept for when the prediction is accepted".to_string(),
            RequestOutcome::Failed => "failed".to_string(),
        };
        if exchange.resent_full_trajectory {
            self.stats.full_resends += 1;
        }
        let new_steps = details.fingerprints.len() - details.cached_steps;
        let request_text = if exchange.resent_full_trajectory {
            format!(
                "{} bytes, all {} steps after the server lost {} cached",
                exchange.request_bytes,
                details.fingerprints.len(),
                details.cached_steps
            )
        } else {
            format!(
                "{} bytes, {} cached + {new_steps} new steps",
                exchange.request_bytes, details.cached_steps
            )
        };
        let retry_at = match &exchange.result {
            Ok(stream) => {
                log::info!(
                    "windsurf: {} ({}) finished in {} ms (context {} ms, first delta after {} ms, {} deltas, stop reason {}; request {request_text}): {outcome_text}",
                    details.method,
                    details.kind.name(),
                    exchange.elapsed.as_millis(),
                    details.context_elapsed.as_millis(),
                    exchange
                        .first_delta_after
                        .map(|duration| duration.as_millis().to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    stream.deltas,
                    stream.stop_reason.as_deref().unwrap_or("-"),
                );
                match &stream.cache_hashes {
                    Some(hashes) => self.trajectory_cache.record(&details.fingerprints, hashes),
                    None => self.trajectory_cache.clear(),
                }
                self.error = None;
                self.retry_at = None;
                self.backoff = Duration::ZERO;
                None
            }
            Err(error) => {
                log::warn!(
                    "windsurf: {} ({}) failed after {} ms (request {request_text}): {error}",
                    details.method,
                    details.kind.name(),
                    exchange.elapsed.as_millis()
                );
                if matches!(error, ConnectError::Protocol(_)) {
                    self.trajectory_cache.clear();
                }
                let retry_at = error.is_retryable().then(|| {
                    self.backoff = (self.backoff * 2).clamp(MIN_BACKOFF, MAX_BACKOFF);
                    cx.background_executor().now() + self.backoff
                });
                self.retry_at = retry_at;
                self.error = Some(error.clone());
                retry_at
            }
        };
        log::debug!("windsurf: totals: {}", self.stats.to_json());
        self.last_request = Some(LastRequest {
            sent_at: SystemTime::now(),
            request: exchange
                .request
                .as_ref()
                .map(redacted_request_json)
                .or_else(|| exchange.described_request.clone()),
            response: response_summary(exchange, details, &outcome_text),
        });
        cx.notify();
        retry_at
    }
}

fn response_summary(
    exchange: &TabExchange,
    details: &RequestDetails,
    outcome: &str,
) -> serde_json::Value {
    let mut summary = serde_json::json!({
        "kind": details.kind.name(),
        "method": details.method,
        "outcome": outcome,
        "elapsed_ms": exchange.elapsed.as_millis() as u64,
        "context_ms": details.context_elapsed.as_millis() as u64,
        "first_delta_ms": exchange.first_delta_after.map(|duration| duration.as_millis() as u64),
        "request_bytes": exchange.request_bytes,
        "cached_steps": details.cached_steps,
        "sent_steps": details.fingerprints.len() - details.cached_steps,
        "resent_full_trajectory": exchange.resent_full_trajectory,
    });
    match &exchange.result {
        Ok(stream) => {
            summary["old_text"] = stream.old_text.clone().into();
            summary["new_text"] = stream.new_text.clone().into();
            summary["path"] = stream.path.clone().into();
            summary["stop_reason"] = stream.stop_reason.clone().into();
            summary["deltas"] = stream.deltas.into();
            summary["noop"] = stream.noop.into();
            summary["tab_jump"] = stream.tab_jump.into();
            summary["request_id"] = stream.request_id.clone().into();
            if !stream.completion_id.is_empty() {
                summary["completion_id"] = stream.completion_id.clone().into();
                summary["prompt_id"] = stream.prompt_id.clone().into();
            }
            if let Some((start_line, end_line)) = stream.line_range {
                summary["line_range"] = serde_json::json!([start_line, end_line]);
            }
            if let Some((row, column)) = stream.jump_position {
                summary["jump_position"] =
                    serde_json::json!({ "row": row, "column_utf16": column });
            }
            if let Some(reason) = &stream.filter_reason {
                summary["filter_reason"] = reason.clone().into();
            }
        }
        Err(error) => summary["error"] = error.to_string().into(),
    }
    summary
}

/// The request as JSON with the key and token replaced by their lengths.
pub fn redacted_request_json(
    request: &proto::exa::api_server_pb::GetTabRequest,
) -> serde_json::Value {
    let mut value = serde_json::to_value(request).unwrap_or_else(
        |error| serde_json::json!({ "error": format!("couldn't serialize the request: {error}") }),
    );
    if let Some(metadata) = value
        .get_mut("metadata")
        .and_then(|metadata| metadata.as_object_mut())
    {
        for field in ["api_key", "user_jwt"] {
            if let Some(entry) = metadata.get_mut(field) {
                let length = entry.as_str().map(str::len).unwrap_or_default();
                *entry = format!("<redacted, {length} chars>").into();
            }
        }
    }
    value
}

/// Whether the buffer's file is on this machine; the language server can't
/// serve remote projects.
fn is_local(buffer: &Buffer, cx: &App) -> bool {
    project::File::from_dyn(buffer.file()).is_some_and(|file| file.worktree.read(cx).is_local())
}

fn is_remote(buffer: &Buffer, cx: &App) -> bool {
    project::File::from_dyn(buffer.file()).is_some_and(|file| !file.worktree.read(cx).is_local())
}

pub fn init(cx: &mut App) {
    credentials::init(cx);
    let executor = cx.background_executor().clone();
    let backend = NativeBackend::new(
        cx.http_client(),
        Arc::new(move |duration| executor.timer(duration).boxed()),
        ClientConfig::default(),
    );
    init_with_backends(Arc::new(backend), language_server_factory(), cx);
}

/// The data directory of the language server Zed starts: its own, so that it
/// never shares state with a Devin or Windsurf app.
pub fn language_server_data_dir() -> std::path::PathBuf {
    paths::data_dir().join("windsurf-ls")
}

fn language_server_factory() -> LanguageServerFactory {
    Arc::new(|cx: &App| {
        let timer_executor = cx.background_executor().clone();
        let clock_executor = cx.background_executor().clone();
        let spawn_executor = cx.background_executor().clone();
        #[cfg(unix)]
        let launcher: Arc<dyn language_server::Launcher> =
            Arc::new(language_server::process::ProcessLauncher);
        #[cfg(not(unix))]
        let launcher: Arc<dyn language_server::Launcher> = Arc::new(UnsupportedLauncher);
        LanguageServerBackend::new(
            launcher,
            LanguageServerConfig::new(language_server_data_dir()),
            Arc::new(move |duration| timer_executor.timer(duration).boxed()),
            Arc::new(move || clock_executor.now()),
            Arc::new(move |future| spawn_executor.spawn(future).detach()),
        )
    })
}

#[cfg(not(unix))]
struct UnsupportedLauncher;

#[cfg(not(unix))]
impl language_server::Launcher for UnsupportedLauncher {
    fn launch(
        &self,
        _options: language_server::LaunchOptions,
    ) -> BoxFuture<'static, Result<Arc<dyn language_server::RunningServer>, String>> {
        futures::future::ready(Err(
            "the language server backend is only implemented for macOS and Linux".to_string(),
        ))
        .boxed()
    }
}

/// Like `init` with the given native backend; the language server backend
/// starts the real binary.
pub fn init_with_backend(backend: Arc<dyn TabBackend>, cx: &mut App) {
    init_with_backends(backend, language_server_factory(), cx);
}

pub fn init_with_backends(
    backend: Arc<dyn TabBackend>,
    language_server_factory: LanguageServerFactory,
    cx: &mut App,
) {
    tab_jump::init(cx);
    let windsurf = cx.new(|cx| {
        let mut windsurf = Windsurf::new(backend, language_server_factory);
        windsurf.apply_settings(cx);
        cx.on_app_quit(Windsurf::on_quit).detach();
        windsurf
    });
    cx.set_global(GlobalWindsurf(windsurf.clone()));
    clipboard::observe_copies(windsurf.clone(), cx).detach();
    cx.observe_global::<SettingsStore>(move |cx| {
        windsurf.update(cx, |windsurf, cx| windsurf.apply_settings(cx));
    })
    .detach();
}

/// Called by `fork_ext` whenever `fork.edit_prediction_provider` may have changed.
pub fn set_selected(selected: bool, cx: &mut App) {
    if let Some(windsurf) = Windsurf::global(cx) {
        windsurf.update(cx, |windsurf, cx| windsurf.set_selected(selected, cx));
    }
}

/// Whether editors should get the Windsurf delegate: enabled and a key is
/// set, with either backend.
pub fn should_provide_predictions(cx: &App) -> bool {
    let settings = WindsurfSettings::get_global(cx);
    settings.enabled && settings.api_key.is_some()
}

pub fn has_delegate(editor: &Editor) -> bool {
    editor.edit_prediction_provider().is_some_and(|provider| {
        provider.name()
            == <WindsurfDelegate as edit_prediction_types::EditPredictionDelegate>::name()
    })
}

pub fn assign_delegate(
    editor: &mut Editor,
    trigger: EditPredictionRequestTrigger,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> bool {
    let Some(windsurf) = Windsurf::global(cx) else {
        return false;
    };
    let official_provider = all_language_settings(None, cx).edit_predictions.provider;
    if official_provider != EditPredictionProvider::None {
        static LOGGED: AtomicBool = AtomicBool::new(false);
        if !LOGGED.swap(true, Ordering::Relaxed) {
            log::info!(
                "windsurf: `fork.edit_prediction_provider` takes precedence over `edit_predictions.provider` ({official_provider:?}); set the latter to \"none\""
            );
        }
    }
    for buffer in editor.buffer().read(cx).all_buffers() {
        windsurf.update(cx, |windsurf, cx| windsurf.track_buffer(&buffer, cx));
    }
    let project = editor.project().map(|project| project.downgrade());
    let delegate = cx.new(|cx| {
        let mut delegate = WindsurfDelegate::new(windsurf, cx);
        delegate.set_project(project);
        delegate
    });
    editor.set_edit_prediction_provider(Some(delegate), trigger, window, cx);
    true
}

pub fn remove_delegate(
    editor: &mut Editor,
    trigger: EditPredictionRequestTrigger,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    if has_delegate(editor) {
        editor.set_edit_prediction_provider::<WindsurfDelegate>(None, trigger, window, cx);
    }
}

pub fn status_items(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    workspace.register_action(|workspace, _: &ShowLastRequest, window, cx| {
        status::show_last_request(workspace, window, cx)
    });
    workspace.register_action(|_, _: &ToggleEnabled, _, cx| status::toggle_enabled(cx));
    workspace.register_action(|_, _: &RestartLanguageServer, _, cx| {
        if let Some(windsurf) = Windsurf::global(cx) {
            windsurf.update(cx, |windsurf, cx| windsurf.restart_language_server(cx));
        }
    });
    workspace.register_action(|workspace, _: &ShowLanguageServerInfo, window, cx| {
        status::show_language_server_info(workspace, window, cx)
    });
    let status_bar = workspace.status_bar().clone();
    cx.defer_in(window, move |_, window, cx| {
        let item = cx.new(status::WindsurfStatusItem::new);
        status_bar.update(cx, |status_bar, cx| {
            status_bar.add_right_item(item, window, cx);
        });
    });
}
