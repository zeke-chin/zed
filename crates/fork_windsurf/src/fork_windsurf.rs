pub mod backend;
pub mod context;
mod credentials;
mod delegate;
pub mod diff;
pub mod native;
pub mod proto;
mod status;
pub mod tab_jump;
pub mod trajectory;

#[cfg(test)]
mod tests;

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use collections::HashSet;
use editor::{EditPredictionRequestTrigger, Editor};
use futures::FutureExt as _;
use gpui::{App, AppContext as _, Context, Entity, Global, Window, actions};
use language::{
    Anchor, Buffer, BufferSnapshot, ToOffset as _,
    language_settings::{EditPredictionProvider, all_language_settings},
};
use settings::{RegisterSetting, Settings, SettingsContent, SettingsStore, WindsurfBackend};
use workspace::Workspace;

use backend::{TabBackend, TabExchange, TabRequestInput};
use context::SelectionWindow;
pub use credentials::{ImportApiKey, OpenApiKeySetting};
pub use delegate::WindsurfDelegate;
use native::{ClientConfig, NativeBackend, connect::ConnectError};
use trajectory::Trajectory;

actions!(
    windsurf,
    [
        /// Opens the most recent Windsurf request and its response in a new tab.
        ShowLastRequest,
        /// Turns Windsurf edit predictions on or off.
        ToggleEnabled,
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
}

pub(crate) enum RequestOutcome {
    Predicted(usize),
    NoPrediction,
    Discarded(&'static str),
    Failed,
}

pub(crate) enum Preparation {
    Send(PreparedRequest),
    WaitUntil(Instant),
    Skip,
}

pub(crate) struct PreparedRequest {
    pub backend: Arc<dyn TabBackend>,
    pub api_key: String,
    pub input: TabRequestInput,
    pub snapshot: BufferSnapshot,
    pub window: SelectionWindow,
    pub abs_path: String,
}

/// The most recent request, for `windsurf: show last request`.
pub struct LastRequest {
    pub sent_at: SystemTime,
    pub request: Option<serde_json::Value>,
    pub response: serde_json::Value,
}

/// State shared by every editor's delegate: the edit history, error and
/// backoff state, and what the status bar shows.
pub struct Windsurf {
    backend: Arc<dyn TabBackend>,
    trajectory: Trajectory,
    /// Whether `fork.edit_prediction_provider` selects Windsurf.
    selected: bool,
    next_request_id: u64,
    in_flight: HashSet<u64>,
    error: Option<ConnectError>,
    retry_at: Option<Instant>,
    backoff: Duration,
    last_request: Option<LastRequest>,
    applied_settings: Option<(bool, Option<String>)>,
}

struct GlobalWindsurf(Entity<Windsurf>);

impl Global for GlobalWindsurf {}

impl Windsurf {
    pub fn new(backend: Arc<dyn TabBackend>) -> Self {
        Self {
            backend,
            trajectory: Trajectory::default(),
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

    pub fn status(&self, cx: &App) -> Status {
        let settings = WindsurfSettings::get_global(cx);
        if !settings.enabled {
            return Status::Disabled;
        }
        if settings.api_key.is_none() {
            return Status::MissingApiKey;
        }
        let retry_in = self
            .retry_at
            .map(|retry_at| retry_at.saturating_duration_since(cx.background_executor().now()))
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
            cx.notify();
        }
    }

    /// Whether requests may be sent at all. Errors and backoff are checked
    /// when a request is about to be sent.
    pub fn can_request(&self, cx: &App) -> bool {
        let settings = WindsurfSettings::get_global(cx);
        self.selected
            && settings.enabled
            && settings.api_key.is_some()
            && settings.backend == WindsurfBackend::Native
    }

    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        let settings = WindsurfSettings::get_global(cx);
        let applied = (settings.enabled, settings.api_key.clone());
        let previous = self.applied_settings.replace(applied.clone());
        if previous.is_some_and(|previous| previous != applied) {
            self.error = None;
            self.retry_at = None;
            self.backoff = Duration::ZERO;
            cx.background_spawn(self.backend.forget_credentials())
                .detach();
            cx.notify();
        }
    }

    pub fn track_buffer(&mut self, buffer: &Entity<Buffer>, cx: &mut Context<Self>) {
        self.trajectory.track(buffer, cx);
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
        self.trajectory.track(buffer, cx);
        self.trajectory.commit(cx);
        if self.trajectory.steps().is_empty() {
            return Preparation::Skip;
        }

        let buffer = buffer.read(cx);
        let Some(location) = context::buffer_location(buffer, cx) else {
            return Preparation::Skip;
        };
        let snapshot = buffer.snapshot();
        let document = context::document_context(&snapshot, cursor, &location, buffer.is_dirty());
        let cursor_row = snapshot.offset_to_point(cursor.to_offset(&snapshot)).row;
        let diagnostics = context::diagnostics_near(
            &snapshot,
            cursor_row,
            &document.document.absolute_uri,
            document.document.language,
        );
        self.in_flight.insert(request_id);
        cx.notify();
        Preparation::Send(PreparedRequest {
            backend: self.backend.clone(),
            api_key,
            input: TabRequestInput {
                document: document.document,
                diagnostics,
                steps: self.trajectory.steps().to_vec(),
                mainline_trajectory_id: self.trajectory.mainline_id().to_string(),
            },
            snapshot,
            window: document.window,
            abs_path: location.abs_path.to_string_lossy().into_owned(),
        })
    }

    /// Records the result. Returns when to retry, for rate limits and network
    /// errors.
    pub(crate) fn finish_request(
        &mut self,
        request_id: u64,
        exchange: &TabExchange,
        outcome: RequestOutcome,
        cx: &mut Context<Self>,
    ) -> Option<Instant> {
        self.in_flight.remove(&request_id);
        let outcome_text = match &outcome {
            RequestOutcome::Predicted(edit_count) => format!("predicted {edit_count} edit(s)"),
            RequestOutcome::NoPrediction => "no prediction".to_string(),
            RequestOutcome::Discarded(reason) => format!("discarded: {reason}"),
            RequestOutcome::Failed => "failed".to_string(),
        };
        let retry_at = match &exchange.result {
            Ok(stream) => {
                log::info!(
                    "windsurf: GetTab finished in {} ms (first delta after {} ms, {} deltas, stop reason {}): {outcome_text}",
                    exchange.elapsed.as_millis(),
                    exchange
                        .first_delta_after
                        .map(|duration| duration.as_millis().to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    stream.deltas,
                    stream.stop_reason.as_deref().unwrap_or("-"),
                );
                self.error = None;
                self.retry_at = None;
                self.backoff = Duration::ZERO;
                None
            }
            Err(error) => {
                log::warn!(
                    "windsurf: GetTab failed after {} ms: {error}",
                    exchange.elapsed.as_millis()
                );
                let retry_at = error.is_retryable().then(|| {
                    self.backoff = (self.backoff * 2).clamp(MIN_BACKOFF, MAX_BACKOFF);
                    cx.background_executor().now() + self.backoff
                });
                self.retry_at = retry_at;
                self.error = Some(error.clone());
                retry_at
            }
        };
        self.last_request = Some(LastRequest {
            sent_at: SystemTime::now(),
            request: exchange.request.as_ref().map(redacted_request_json),
            response: response_summary(exchange, &outcome_text),
        });
        cx.notify();
        retry_at
    }
}

fn response_summary(exchange: &TabExchange, outcome: &str) -> serde_json::Value {
    let mut summary = serde_json::json!({
        "outcome": outcome,
        "elapsed_ms": exchange.elapsed.as_millis() as u64,
        "first_delta_ms": exchange.first_delta_after.map(|duration| duration.as_millis() as u64),
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

pub fn init(cx: &mut App) {
    credentials::init(cx);
    let executor = cx.background_executor().clone();
    let backend = NativeBackend::new(
        cx.http_client(),
        Arc::new(move |duration| executor.timer(duration).boxed()),
        ClientConfig::default(),
    );
    init_with_backend(Arc::new(backend), cx);
}

pub fn init_with_backend(backend: Arc<dyn TabBackend>, cx: &mut App) {
    tab_jump::init(cx);
    let windsurf = cx.new(|cx| {
        let mut windsurf = Windsurf::new(backend);
        windsurf.apply_settings(cx);
        windsurf
    });
    cx.set_global(GlobalWindsurf(windsurf.clone()));
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

/// Whether editors should get the Windsurf delegate: enabled, a key is set
/// and the native backend is selected.
pub fn should_provide_predictions(cx: &App) -> bool {
    let settings = WindsurfSettings::get_global(cx);
    let should_provide = settings.enabled && settings.api_key.is_some();
    if should_provide && settings.backend != WindsurfBackend::Native {
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            log::warn!(
                "windsurf: the language server backend isn't implemented yet; no predictions are shown"
            );
        }
        return false;
    }
    should_provide
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
    let status_bar = workspace.status_bar().clone();
    cx.defer_in(window, move |_, window, cx| {
        let item = cx.new(status::WindsurfStatusItem::new);
        status_bar.update(cx, |status_bar, cx| {
            status_bar.add_right_item(item, window, cx);
        });
    });
}
