//! The `EditPredictionDelegate` that shows Windsurf's next-edit predictions.
//!
//! A trigger is answered, in order of preference, by:
//! 1. the shown prediction, trimmed locally while the user types along it
//!    (`edit_prediction_types::interpolate_edits`, no request);
//! 2. a predictive request made right after the previous prediction, when the
//!    buffer now reads exactly as if that prediction had been accepted
//!    (`prefetch.rs`, no new request);
//! 3. a `GetTab` request after a short debounce.

use std::{
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};

use edit_prediction_types::{
    EditPrediction, EditPredictionDelegate, EditPredictionDiscardReason, EditPredictionIconSet,
    EditPredictionRequestTrigger, PredictedCursorPosition, SuggestionDisplayType,
};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EntityId, SharedString, Task, WeakEntity,
};
use language::{
    Anchor, Bias, Buffer, BufferSnapshot, EditPreview, PointUtf16, ToOffset as _, Unclipped,
};
use project::Project;
use ui::IconName;

use crate::{
    PendingJump, Preparation, PreparedRequest, RequestOutcome, Windsurf, WindsurfSettings,
    context::SelectionWindow,
    diff::{PredictionError, prediction_edits},
    feedback::{DocumentInfo, PredictionRecord, row_and_column},
    language_server::request::{line_range_bytes, trigger_condition},
    native::get_tab::TabStream,
    prefetch::{Prefetch, PrefetchUse, apply_edits},
    proto::exa::codeium_common_pb::SupercompleteTriggerCondition,
    tab_jump::{self, JumpOrigin},
};
use settings::Settings as _;

/// Typing in quick succession only sends a request once it pauses this long.
pub const DEBOUNCE: Duration = Duration::from_millis(100);

pub(crate) type PredictionEdits = Arc<[(Range<Anchor>, Arc<str>)]>;

#[derive(Clone)]
pub(crate) struct CurrentPrediction {
    pub id: SharedString,
    pub buffer: Entity<Buffer>,
    pub snapshot: BufferSnapshot,
    pub edits: PredictionEdits,
    pub edit_preview: EditPreview,
    /// Set when the edits are in another file than the one they were
    /// predicted for (Tab Jump).
    pub jump: Option<JumpOrigin>,
    /// For feedback; `None` for Tab Jump predictions, which have no `GetTab`
    /// window to report.
    pub record: Option<PredictionRecord>,
    pub shown_at: Option<Instant>,
}

impl CurrentPrediction {
    fn interpolate(&self, current: &BufferSnapshot) -> Option<Vec<(Range<Anchor>, Arc<str>)>> {
        edit_prediction_types::interpolate_edits(&self.snapshot, current, &self.edits)
            .filter(|edits| !edits.is_empty())
    }
}

/// A `GetTab` response that is ready to be shown.
pub(crate) struct ResponseForBuffer {
    pub stream: TabStream,
    pub window: SelectionWindow,
    pub abs_path: String,
    pub prompt_id: String,
    pub document: DocumentInfo,
    pub from_cache: bool,
}

/// A Tab Jump from the language server: only a position, the edit there
/// comes from the request that arriving at it triggers (`TAB_JUMP_ACCEPT`).
#[derive(Clone)]
pub(crate) struct LanguageServerJump {
    pub id: SharedString,
    /// The buffer and version the jump was predicted for.
    pub origin: Entity<Buffer>,
    pub origin_version: clock::Global,
    pub target_buffer: Entity<Buffer>,
    pub target: Anchor,
}

pub struct WindsurfDelegate {
    pub(crate) windsurf: Entity<Windsurf>,
    pub(crate) pending_request: Option<(u64, Task<()>)>,
    pub(crate) current: Option<CurrentPrediction>,
    pub(crate) language_server_jump: Option<LanguageServerJump>,
    /// The buffer as of the last `suggest`, which is the state just before
    /// an accepted prediction gets applied.
    last_suggested: Option<(EntityId, BufferSnapshot)>,
    pub(crate) prefetch: Option<Prefetch>,
    pub(crate) next_prefetch_generation: u64,
    pub(crate) project: Option<WeakEntity<Project>>,
}

impl WindsurfDelegate {
    pub fn new(windsurf: Entity<Windsurf>, cx: &mut Context<Self>) -> Self {
        cx.on_release(|this, cx| this.cancel_pending_request(cx))
            .detach();
        Self {
            windsurf,
            pending_request: None,
            current: None,
            language_server_jump: None,
            last_suggested: None,
            prefetch: None,
            next_prefetch_generation: 0,
            project: None,
        }
    }

    /// Lets Tab Jump open the files it may jump to.
    pub fn set_project(&mut self, project: Option<WeakEntity<Project>>) {
        self.project = project;
    }

    /// Takes over edits a Tab Jump from another editor left for `buffer`.
    fn adopt_landing(&mut self, buffer: &Entity<Buffer>, cx: &mut App) {
        if let Some(landing) = tab_jump::take_landing(buffer, cx) {
            self.current = Some(landing);
        }
    }

    pub(crate) fn cancel_pending_request(&mut self, cx: &mut App) {
        if let Some((request_id, _task)) = self.pending_request.take() {
            self.windsurf
                .update(cx, |windsurf, cx| windsurf.cancel_request(request_id, cx));
        }
    }

    /// Sends a `GetTab` request for the cursor after `debounce`.
    pub(crate) fn start_request(
        &mut self,
        buffer: Entity<Buffer>,
        cursor: Anchor,
        debounce: Duration,
        trigger: SupercompleteTriggerCondition,
        cx: &mut Context<Self>,
    ) {
        self.cancel_pending_request(cx);
        self.windsurf
            .update(cx, |windsurf, cx| windsurf.track_buffer(&buffer, cx));
        let request_id = self
            .windsurf
            .update(cx, |windsurf, _| windsurf.next_request_id());
        let windsurf = self.windsurf.clone();
        let task = cx.spawn(async move |this, cx| {
            if let Err(error) = Self::run_request(
                this, windsurf, buffer, cursor, request_id, debounce, trigger, cx,
            )
            .await
            {
                log::debug!("windsurf: request {request_id} ended early: {error:#}");
            }
        });
        self.pending_request = Some((request_id, task));
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_request(
        this: gpui::WeakEntity<Self>,
        windsurf: Entity<Windsurf>,
        buffer: Entity<Buffer>,
        cursor: Anchor,
        request_id: u64,
        debounce: Duration,
        trigger: SupercompleteTriggerCondition,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<()> {
        if !debounce.is_zero() {
            cx.background_executor().timer(debounce).await;
        }
        let trigger_id = uuid::Uuid::new_v4().to_string();
        loop {
            let preparation = windsurf.update(cx, |windsurf, cx| {
                windsurf.prepare_request(request_id, &buffer, cursor, &trigger_id, trigger, cx)
            });
            let (prepared, snapshot) = match preparation {
                Preparation::Send(prepared, snapshot) => (prepared, snapshot),
                Preparation::WaitUntil(retry_at) => {
                    wait_until(retry_at, cx).await;
                    continue;
                }
                Preparation::Skip => {
                    this.update(cx, |this, _| this.finish_pending(request_id))?;
                    return Ok(());
                }
            };
            let PreparedRequest {
                backend,
                api_key,
                input,
                window,
                abs_path,
                details,
            } = prepared;
            let project = this.read_with(cx, |this, _| this.project.clone())?;
            let tab_jump = tab_jump::start(
                &windsurf,
                project,
                &buffer,
                cursor,
                request_id,
                backend.clone(),
                api_key.clone(),
                cx,
            );
            let exchange = cx.background_spawn(backend.get_tab(api_key, input)).await;

            let retry_at = match &exchange.result {
                Ok(stream) if stream.tab_jump && stream.jump_position.is_some() => {
                    let jump =
                        language_server_jump(&this, &buffer, &snapshot, stream, request_id, cx)
                            .await;
                    let outcome = match &jump {
                        Some(_) => RequestOutcome::TabJump,
                        None => RequestOutcome::Discarded("the Tab Jump target isn't available"),
                    };
                    windsurf.update(cx, |windsurf, cx| {
                        windsurf.finish_request(request_id, &details, &exchange, outcome, cx);
                        windsurf.set_pending_jump(jump.as_ref().map(|jump| PendingJump {
                            buffer: jump.target_buffer.downgrade(),
                            target: jump.target,
                            completion_id: stream.completion_id.clone(),
                            prompt_id: stream.prompt_id.clone(),
                            shown_at: cx.background_executor().now(),
                        }));
                    });
                    this.update(cx, |this, cx| {
                        this.finish_pending(request_id);
                        this.current = None;
                        this.language_server_jump = jump;
                        cx.notify();
                    })?;
                    return Ok(());
                }
                Ok(stream) => {
                    // The language server rewrites whole lines of its own choosing.
                    let window = match stream.line_range {
                        Some((start_line, end_line)) => {
                            line_window(&snapshot, start_line, end_line).unwrap_or(window)
                        }
                        None => window,
                    };
                    let prediction =
                        prepare_prediction(&buffer, &snapshot, &window, stream, &abs_path, cx)
                            .await;
                    let outcome = match &prediction {
                        Ok(Some((edits, _))) => RequestOutcome::Predicted(edits.len()),
                        Ok(None) => RequestOutcome::NoPrediction,
                        Err(reason) => RequestOutcome::Discarded(reason),
                    };
                    windsurf.update(cx, |windsurf, cx| {
                        windsurf.finish_request(request_id, &details, &exchange, outcome, cx)
                    });
                    let response = ResponseForBuffer {
                        stream: stream.clone(),
                        window,
                        abs_path,
                        prompt_id: if stream.prompt_id.is_empty() {
                            exchange
                                .request
                                .as_ref()
                                .map(|request| request.prompt_id.clone())
                                .unwrap_or_default()
                        } else {
                            stream.prompt_id.clone()
                        },
                        document: details.document.clone(),
                        from_cache: false,
                    };
                    let jump = match (&prediction, tab_jump) {
                        (Ok(None), Some(tab_jump)) => tab_jump.await,
                        (_, Some(tab_jump)) => {
                            tab_jump.detach();
                            None
                        }
                        (_, None) => None,
                    };
                    this.update(cx, |this, cx| {
                        this.finish_pending(request_id);
                        this.language_server_jump = None;
                        if let Ok(Some((edits, edit_preview))) = prediction {
                            this.install(
                                &buffer,
                                snapshot,
                                edits,
                                edit_preview,
                                response,
                                request_id,
                                &trigger_id,
                                cx,
                            );
                        } else if let Some(jump) = jump {
                            this.current = Some(jump);
                        }
                        cx.notify();
                    })?;
                    return Ok(());
                }
                Err(_) => windsurf.update(cx, |windsurf, cx| {
                    windsurf.finish_request(
                        request_id,
                        &details,
                        &exchange,
                        RequestOutcome::Failed,
                        cx,
                    )
                }),
            };

            let Some(retry_at) = retry_at else {
                this.update(cx, |this, cx| {
                    this.finish_pending(request_id);
                    cx.notify();
                })?;
                return Ok(());
            };
            wait_until(retry_at, cx).await;
        }
    }

    /// Makes a converted response the shown prediction and starts the
    /// predictive request for it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn install(
        &mut self,
        buffer: &Entity<Buffer>,
        snapshot: BufferSnapshot,
        edits: PredictionEdits,
        edit_preview: EditPreview,
        response: ResponseForBuffer,
        request_id: u64,
        trigger_id: &str,
        cx: &mut Context<Self>,
    ) {
        let current_snapshot = buffer.read(cx).snapshot();
        let assumed =
            edit_prediction_types::interpolate_edits(&snapshot, &current_snapshot, &edits)
                .filter(|edits| !edits.is_empty())
                .map(|edits| apply_edits(&current_snapshot, &edits));
        let cursor_position = assumed
            .as_ref()
            .map(|(text, cursor)| row_and_column(text, *cursor))
            .unwrap_or_default();
        let completion_id = response.stream.completion_id.clone();
        let mut record = PredictionRecord::new(
            response.prompt_id,
            &response.document,
            &response.window,
            response.stream.old_text,
            response.stream.new_text,
            cursor_position,
            response.from_cache,
        );
        if !completion_id.is_empty() {
            record.completion_id = completion_id;
        }
        self.current = Some(CurrentPrediction {
            id: SharedString::from(format!("windsurf-{request_id}")),
            buffer: buffer.clone(),
            snapshot,
            edits,
            edit_preview,
            jump: None,
            record: Some(record),
            shown_at: None,
        });
        if let Some((assumed_text, assumed_cursor)) = assumed {
            self.start_prefetch(buffer, assumed_text, assumed_cursor, trigger_id, cx);
        }
    }

    pub(crate) fn finish_pending(&mut self, request_id: u64) {
        if self
            .pending_request
            .as_ref()
            .is_some_and(|(pending_id, _)| *pending_id == request_id)
        {
            self.pending_request = None;
        }
    }

    /// `Some` when the language server's Tab Jump decides what to show: the
    /// jump itself while the origin buffer is unchanged, or nothing once the
    /// cursor arrived at the target, which requests the edit there.
    fn suggest_language_server_jump(
        &mut self,
        buffer: &Entity<Buffer>,
        cursor: Anchor,
        cx: &mut Context<Self>,
    ) -> Option<Option<EditPrediction>> {
        let landed = self.windsurf.update(cx, |windsurf, cx| {
            windsurf.take_landed_jump(buffer, cursor, cx)
        });
        if let Some(landed) = landed {
            self.language_server_jump = None;
            self.current = None;
            let delay = cx
                .background_executor()
                .now()
                .saturating_duration_since(landed.shown_at);
            self.windsurf.update(cx, |windsurf, cx| {
                windsurf.report_language_server_feedback(
                    &landed.completion_id,
                    &landed.prompt_id,
                    true,
                    delay,
                    buffer,
                    cx,
                )
            });
            self.start_request(
                buffer.clone(),
                cursor,
                Duration::ZERO,
                SupercompleteTriggerCondition::TabJumpAccept,
                cx,
            );
            return Some(None);
        }
        let jump = self.language_server_jump.as_ref()?;
        if &jump.origin != buffer {
            return None;
        }
        // Another editor took the jump when it landed there.
        let still_pending = self.windsurf.read(cx).pending_jump.is_some();
        if buffer.read(cx).version() != jump.origin_version || !still_pending {
            self.language_server_jump = None;
            self.windsurf
                .update(cx, |windsurf, _| windsurf.set_pending_jump(None));
            return None;
        }
        Some(Some(EditPrediction::Jump {
            id: Some(jump.id.clone()),
            snapshot: jump.target_buffer.read(cx).snapshot(),
            target: jump.target,
        }))
    }

    fn record_feedback(
        &mut self,
        current: CurrentPrediction,
        accepted: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(record) = current.record else {
            return;
        };
        let now = cx.background_executor().now();
        let delay = current
            .shown_at
            .map(|shown_at| now.saturating_duration_since(shown_at))
            .unwrap_or_default();
        let before_accept = self
            .last_suggested
            .take()
            .filter(|(buffer_id, _)| *buffer_id == current.buffer.entity_id())
            .map(|(_, snapshot)| snapshot);
        self.windsurf.update(cx, |windsurf, cx| {
            windsurf.record_feedback(
                &current.buffer,
                before_accept.as_ref(),
                &record,
                accepted,
                delay,
                cx,
            )
        });
    }
}

/// The window a language server rewrite covers: lines `[start, end)` of the
/// document that was sent.
fn line_window(
    snapshot: &BufferSnapshot,
    start_line: u32,
    end_line: u32,
) -> Option<SelectionWindow> {
    let text = snapshot.text();
    let byte_range = line_range_bytes(&text, start_line, end_line)?;
    Some(SelectionWindow {
        text: text[byte_range.clone()].to_string(),
        start_line,
        end_line,
        byte_range,
    })
}

/// Resolves the language server's Tab Jump to a buffer position. The column
/// is in UTF-16 code units, as the official extension passes it straight to
/// a VS Code `Position`.
async fn language_server_jump(
    this: &WeakEntity<WindsurfDelegate>,
    buffer: &Entity<Buffer>,
    snapshot: &BufferSnapshot,
    stream: &TabStream,
    request_id: u64,
    cx: &mut AsyncApp,
) -> Option<LanguageServerJump> {
    let (row, column) = stream.jump_position?;
    let same_file = stream.path.is_empty()
        || buffer.read_with(cx, |buffer, cx| {
            crate::context::buffer_location(buffer, cx)
                .is_some_and(|location| location.abs_path.to_string_lossy() == stream.path)
        });
    let target_buffer = if same_file {
        buffer.clone()
    } else {
        let project = this.read_with(cx, |this, _| this.project.clone()).ok()??;
        let open = project
            .update(cx, |project, cx| {
                let project_path = project.find_project_path(&stream.path, cx)?;
                Some(project.open_buffer(project_path, cx))
            })
            .ok()??;
        open.await.ok()?
    };
    if &target_buffer == buffer
        && &buffer.read_with(cx, |buffer, _| buffer.version()) != snapshot.version()
    {
        return None;
    }
    let target = target_buffer.read_with(cx, |target_buffer, _| {
        let target_snapshot = target_buffer.snapshot();
        let point =
            target_snapshot.clip_point_utf16(Unclipped(PointUtf16::new(row, column)), Bias::Left);
        target_snapshot.anchor_before(point)
    });
    Some(LanguageServerJump {
        id: SharedString::from(format!("windsurf-ls-jump-{request_id}")),
        origin: buffer.clone(),
        origin_version: snapshot.version().clone(),
        target_buffer,
        target,
    })
}

async fn wait_until(instant: Instant, cx: &mut AsyncApp) {
    let now = cx.background_executor().now();
    cx.background_executor()
        .timer(instant.saturating_duration_since(now))
        .await;
}

/// Converts a response into anchored edits and their preview. Fails when the
/// window it rewrites changed since `snapshot` was taken.
pub(crate) async fn prepare_prediction(
    buffer: &Entity<Buffer>,
    snapshot: &BufferSnapshot,
    window: &SelectionWindow,
    stream: &TabStream,
    abs_path: &str,
    cx: &mut AsyncApp,
) -> Result<Option<(PredictionEdits, EditPreview)>, &'static str> {
    let conversion = cx
        .background_spawn({
            let snapshot = snapshot.clone();
            let stream = stream.clone();
            let window = window.byte_range.clone();
            let abs_path = abs_path.to_string();
            async move { convert(&snapshot, window, &stream, &abs_path) }
        })
        .await?;
    if conversion.is_empty() {
        return Ok(None);
    }
    let still_valid = buffer.read_with(cx, |buffer, _| {
        window_unchanged(snapshot, &buffer.snapshot(), window)
    });
    if !still_valid {
        return Err("the buffer changed while the request was in flight");
    }
    let edits: PredictionEdits = conversion
        .into_iter()
        .map(|(range, text)| {
            (
                snapshot.anchor_after(range.start)..snapshot.anchor_before(range.end),
                Arc::from(text),
            )
        })
        .collect();
    let edit_preview = buffer
        .read_with(cx, |buffer, cx| buffer.preview_edits(edits.clone(), cx))
        .await;
    Ok(Some((edits, edit_preview)))
}

/// Checks that the window the prediction rewrites still has the text it had
/// when the request was sent.
fn window_unchanged(
    request_snapshot: &BufferSnapshot,
    current: &BufferSnapshot,
    window: &SelectionWindow,
) -> bool {
    if current.version() == request_snapshot.version() {
        return true;
    }
    let start = request_snapshot
        .anchor_before(window.byte_range.start)
        .to_offset(current);
    let end = request_snapshot
        .anchor_after(window.byte_range.end)
        .to_offset(current);
    current.text_for_range(start..end).collect::<String>() == window.text
}

fn convert(
    snapshot: &BufferSnapshot,
    window: Range<usize>,
    stream: &TabStream,
    abs_path: &str,
) -> Result<Vec<(Range<usize>, String)>, &'static str> {
    if !stream.path.is_empty() && stream.path != abs_path {
        return Err("the prediction is for a different file");
    }
    if stream.noop || stream.tab_jump || stream.new_text.is_empty() {
        return Ok(Vec::new());
    }
    prediction_edits(&snapshot.text(), window, &stream.old_text, &stream.new_text).map_err(
        |error| match error {
            PredictionError::OldTextMismatch => "the returned old text doesn't match the buffer",
            PredictionError::WindowOutOfBounds => "the request window is outside the buffer",
        },
    )
}

impl EditPredictionDelegate for WindsurfDelegate {
    fn name() -> &'static str {
        "windsurf"
    }

    fn display_name() -> &'static str {
        "Windsurf"
    }

    fn show_predictions_in_menu() -> bool {
        true
    }

    fn supports_jump_to_edit() -> bool {
        tab_jump::is_enabled()
    }

    fn icons(&self, _cx: &App) -> EditPredictionIconSet {
        EditPredictionIconSet::new(IconName::ZedPredict)
    }

    fn is_enabled(&self, buffer: &Entity<Buffer>, _cursor_position: Anchor, cx: &App) -> bool {
        let settings = WindsurfSettings::get_global(cx);
        settings.enabled
            && settings.api_key.is_some()
            && crate::context::buffer_location(buffer.read(cx), cx).is_some()
    }

    fn is_refreshing(&self, _cx: &App) -> bool {
        self.pending_request.is_some()
    }

    fn refresh(
        &mut self,
        buffer: Entity<Buffer>,
        cursor_position: Anchor,
        debounce_duration: Duration,
        trigger: EditPredictionRequestTrigger,
        cx: &mut Context<Self>,
    ) {
        if !self.windsurf.read(cx).can_request(cx) {
            return;
        }
        self.adopt_landing(&buffer, cx);
        if let Some(jump) = &self.language_server_jump {
            if jump.origin == buffer && buffer.read(cx).version() == jump.origin_version {
                return;
            }
            self.language_server_jump = None;
        }
        if let Some(current) = &self.current
            && current.buffer == buffer
            && current.interpolate(&buffer.read(cx).snapshot()).is_some()
        {
            self.windsurf
                .update(cx, |windsurf, _| windsurf.count_local_cache_hit());
            return;
        }
        match self.use_prefetch(&buffer, cursor_position, cx) {
            PrefetchUse::Used | PrefetchUse::Waiting => return,
            PrefetchUse::Unavailable => self.prefetch = None,
        }

        let debounce = if trigger == EditPredictionRequestTrigger::Explicit {
            debounce_duration
        } else {
            debounce_duration.max(DEBOUNCE)
        };
        self.start_request(
            buffer,
            cursor_position,
            debounce,
            trigger_condition(trigger),
            cx,
        );
    }

    fn accept(&mut self, cx: &mut Context<Self>) {
        self.cancel_pending_request(cx);
        self.language_server_jump = None;
        if let Some(current) = self.current.take() {
            self.record_feedback(current, true, cx);
        }
    }

    fn discard(&mut self, reason: EditPredictionDiscardReason, cx: &mut Context<Self>) {
        self.cancel_pending_request(cx);
        if let Some(jump) = self.language_server_jump.take() {
            let pending = self.windsurf.update(cx, |windsurf, _| {
                let pending = windsurf.pending_jump.clone();
                windsurf.set_pending_jump(None);
                pending
            });
            if reason == EditPredictionDiscardReason::Rejected
                && let Some(pending) = pending
            {
                let delay = cx
                    .background_executor()
                    .now()
                    .saturating_duration_since(pending.shown_at);
                self.windsurf.update(cx, |windsurf, cx| {
                    windsurf.report_language_server_feedback(
                        &pending.completion_id,
                        &pending.prompt_id,
                        false,
                        delay,
                        &jump.origin,
                        cx,
                    )
                });
            }
        }
        if let Some(current) = self.current.take() {
            tab_jump::clear_landing(&current.id, cx);
            if reason == EditPredictionDiscardReason::Rejected {
                self.prefetch = None;
                self.record_feedback(current, false, cx);
            }
        }
    }

    fn did_show(&mut self, _display_type: SuggestionDisplayType, cx: &mut Context<Self>) {
        if let Some(current) = &mut self.current
            && current.shown_at.is_none()
        {
            current.shown_at = Some(cx.background_executor().now());
        }
    }

    fn suggest(
        &mut self,
        buffer: &Entity<Buffer>,
        cursor_position: Anchor,
        cx: &mut Context<Self>,
    ) -> Option<EditPrediction> {
        self.adopt_landing(buffer, cx);
        if self.windsurf.read(cx).uses_language_server() {
            if let Some(prediction) = self.suggest_language_server_jump(buffer, cursor_position, cx)
            {
                return prediction;
            }
            self.windsurf
                .read(cx)
                .selection_changed(buffer, cursor_position, cx);
        }
        let current = self.current.as_ref()?;
        if &current.buffer != buffer {
            let jump = current.jump.as_ref()?;
            let stale = jump.buffer != *buffer
                || &buffer.read(cx).version() != jump.snapshot.version()
                || !tab_jump::is_landing_pending(&current.id, cx);
            if stale {
                if jump.buffer == *buffer {
                    let id = current.id.clone();
                    tab_jump::clear_landing(&id, cx);
                    self.current = None;
                }
                return None;
            }
            return Some(EditPrediction::Jump {
                id: Some(current.id.clone()),
                snapshot: current.buffer.read(cx).snapshot(),
                target: jump.target,
            });
        }
        let snapshot = buffer.read(cx).snapshot();
        let Some(edits) = current.interpolate(&snapshot) else {
            self.current = None;
            return None;
        };
        let id = current.id.clone();
        let edit_preview = current.edit_preview.clone();
        self.last_suggested = Some((buffer.entity_id(), snapshot));
        let cursor_position = edits
            .last()
            .map(|(range, text)| PredictedCursorPosition::new(range.start, text.len()));
        Some(EditPrediction::Local {
            id: Some(id),
            edits,
            cursor_position,
            edit_preview: Some(edit_preview),
        })
    }
}
