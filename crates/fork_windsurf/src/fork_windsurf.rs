pub mod backend;
pub mod clipboard;
pub mod context;
mod credentials;
mod delegate;
pub mod diff;
pub mod feedback;
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

use backend::{FeedbackReport, TabBackend, TabExchange, TabRequestInput};
use clipboard::{CLIPBOARD_WINDOW, PendingCopy};
use context::SelectionWindow;
pub use credentials::{ImportApiKey, OpenApiKeySetting};
pub use delegate::WindsurfDelegate;
use feedback::{DocumentInfo, MAX_FEEDBACK_ENTRIES, MIN_REPORT_INTERVAL, PredictionRecord};
use native::{ClientConfig, NativeBackend, connect::ConnectError};
use proto::exa::prompt_pb::SupercompleteFeedback;
use trajectory::Trajectory;
use trajectory_cache::TrajectoryCache;

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

/// State shared by every editor's delegate: the edit history, error and
/// backoff state, and what the status bar shows.
pub struct Windsurf {
    backend: Arc<dyn TabBackend>,
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
    applied_settings: Option<(bool, Option<String>)>,
}

struct GlobalWindsurf(Entity<Windsurf>);

impl Global for GlobalWindsurf {}

impl Windsurf {
    pub fn new(backend: Arc<dyn TabBackend>) -> Self {
        Self {
            backend,
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
        trigger_id: &str,
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
        if !self.can_request(cx) || self.error.is_some() || self.retry_at.is_some() {
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
                    "windsurf: GetTab ({}) finished in {} ms (context {} ms, first delta after {} ms, {} deltas, stop reason {}; request {request_text}): {outcome_text}",
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
                    "windsurf: GetTab ({}) failed after {} ms (request {request_text}): {error}",
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
            request: exchange.request.as_ref().map(redacted_request_json),
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
