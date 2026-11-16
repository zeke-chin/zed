//! Tab Jump: the prediction of where the next edit is when it isn't near the
//! cursor. The official language server doesn't get it from `GetTab`; on every
//! trigger it also sends a `GetChatMessage` whose prompt it assembles locally
//! (`fork/spike/结论.md` 问题 18). This module does the same: it builds the
//! prompt from the trajectory (`prompt.rs`), sends it next to `GetTab`, reads
//! the continuation (`response.rs`) and turns `TargetContent` /
//! `ReplacementContent` into edits at the place the text is found.
//!
//! The delegate only uses the result when `GetTab` has nothing near the
//! cursor. A jump within the file is shown as ordinary edits away from the
//! cursor, which the editor offers as "jump to edit"; a jump to another open
//! file becomes `EditPrediction::Jump`, and the edits are handed to that file's
//! editor once it opens (the "landing"). After jumping, `ReplacementContent` is
//! the prediction there, so no second request is needed.

pub mod prompt;
pub mod response;
#[cfg(test)]
mod tests;

use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use gpui::{App, AppContext as _, AsyncApp, Entity, Global, SharedString, Task, WeakEntity};
use language::{Anchor, Buffer, BufferSnapshot, Point, ToOffset as _};
use project::Project;
use settings::{Settings as _, SettingsStore};

use crate::{
    Windsurf, WindsurfSettings,
    backend::TabBackend,
    context::{buffer_location, language_for},
    delegate::CurrentPrediction,
    diff::prediction_edits,
    native::get_chat_message::ChatExchange,
    proto::exa::api_server_pb::GetChatMessageRequest,
};
use prompt::{ActiveDocument, HistoryEvent, OpenDocument, PromptInput};
use response::{LocateError, ParseError};

/// Mirrors `fork.windsurf.tab_jump` for `supports_jump_to_edit`, which gets no
/// context to read settings from.
static ENABLED: AtomicBool = AtomicBool::new(true);

const MAX_SUMMARY_MESSAGE_LEN: usize = 2000;

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

#[derive(Default)]
struct TabJumpState {
    /// What `windsurf: show last request` shows about Tab Jump.
    last: Option<serde_json::Value>,
    /// Edits for another file that wait for its editor to open.
    landing: Option<CurrentPrediction>,
}

impl Global for TabJumpState {}

/// Where a jump to another file starts.
#[derive(Clone)]
pub(crate) struct JumpOrigin {
    pub buffer: Entity<Buffer>,
    /// The origin buffer when the jump was predicted; edits after that make
    /// the jump stale.
    pub snapshot: BufferSnapshot,
    /// Where the cursor goes in the target buffer.
    pub target: Anchor,
}

pub(crate) fn init(cx: &mut App) {
    cx.set_global(TabJumpState::default());
    sync_setting(cx);
    cx.observe_global::<SettingsStore>(sync_setting).detach();
}

fn sync_setting(cx: &mut App) {
    ENABLED.store(WindsurfSettings::get_global(cx).tab_jump, Ordering::Relaxed);
}

pub fn last_summary(cx: &App) -> serde_json::Value {
    cx.try_global::<TabJumpState>()
        .and_then(|state| state.last.clone())
        .unwrap_or_default()
}

/// Hands the waiting edits for `buffer` to its editor's delegate.
pub(crate) fn take_landing(buffer: &Entity<Buffer>, cx: &mut App) -> Option<CurrentPrediction> {
    let state = cx.try_global::<TabJumpState>()?;
    if state.landing.as_ref()?.buffer != *buffer {
        return None;
    }
    cx.global_mut::<TabJumpState>().landing.take()
}

pub(crate) fn is_landing_pending(id: &SharedString, cx: &App) -> bool {
    cx.try_global::<TabJumpState>()
        .and_then(|state| state.landing.as_ref())
        .is_some_and(|landing| landing.id == *id)
}

pub(crate) fn clear_landing(id: &SharedString, cx: &mut App) {
    if is_landing_pending(id, cx) {
        cx.global_mut::<TabJumpState>().landing = None;
    }
}

struct Collected {
    request: GetChatMessageRequest,
    active_buffer: Entity<Buffer>,
    /// Files the jump may land in, the active one first. Buffers that aren't
    /// loaded are opened while the request is in flight.
    candidates: Vec<(String, Option<Entity<Buffer>>)>,
}

/// Starts a Tab Jump request next to `GetTab` and returns its prediction, if
/// `fork.windsurf.tab_jump` is on. The caller has already checked everything
/// else `GetTab` needs (key, backoff, a non-empty trajectory).
#[allow(clippy::too_many_arguments)]
pub(crate) fn start(
    windsurf: &Entity<Windsurf>,
    project: Option<WeakEntity<Project>>,
    buffer: &Entity<Buffer>,
    cursor: Anchor,
    request_id: u64,
    backend: Arc<dyn TabBackend>,
    api_key: String,
    cx: &mut AsyncApp,
) -> Option<Task<Option<CurrentPrediction>>> {
    let collected = cx.update(|cx| {
        if !WindsurfSettings::get_global(cx).tab_jump {
            return None;
        }
        collect(windsurf.read(cx), buffer, cursor, cx)
    })?;
    Some(cx.spawn(async move |cx| run(collected, project, request_id, backend, api_key, cx).await))
}

fn collect(
    windsurf: &Windsurf,
    buffer: &Entity<Buffer>,
    cursor: Anchor,
    cx: &App,
) -> Option<Collected> {
    let snapshot = buffer.read(cx).snapshot();
    let location = buffer_location(buffer.read(cx), cx)?;
    let active_path = location.abs_path.to_string_lossy().into_owned();
    let (language, editor_language) = language_for(
        snapshot
            .language()
            .map(|language| language.name().0.to_string()),
    );
    let cursor_offset = cursor.to_offset(&snapshot);

    let history = prompt::history_from_steps(&windsurf.trajectory().steps());
    let edited_paths: Vec<&str> = history
        .iter()
        .filter_map(|event| match event {
            HistoryEvent::Edit { path, .. } => Some(path.as_str()),
            HistoryEvent::Clipboard(_) => None,
        })
        .collect();
    let mut other_documents = Vec::new();
    let mut other_buffers = Vec::new();
    for tracked in windsurf.trajectory().tracked_buffers() {
        if tracked == *buffer {
            continue;
        }
        let tracked_buffer = tracked.read(cx);
        let Some(location) = buffer_location(tracked_buffer, cx) else {
            continue;
        };
        let path = location.abs_path.to_string_lossy().into_owned();
        let (document_language, _) = language_for(
            tracked_buffer
                .language()
                .map(|language| language.name().0.to_string()),
        );
        let text = edited_paths
            .contains(&path.as_str())
            .then(|| tracked_buffer.text());
        other_documents.push(OpenDocument {
            path: path.clone(),
            language: document_language,
            text,
        });
        other_buffers.push((path, tracked));
    }
    other_documents.sort_by(|left, right| left.path.cmp(&right.path));

    let input = PromptInput {
        active: ActiveDocument {
            path: active_path.clone(),
            language,
            editor_language,
            text: snapshot.text(),
            cursor_offset,
            code_item: code_item(&snapshot, cursor_offset),
        },
        other_documents,
        history,
        presented_at: chrono::Local::now()
            .format("%b %-d, %Y at %-I:%M%P, UTC%:z")
            .to_string(),
    };
    let prompt = prompt::build_prompt(&input);
    let mut candidates = vec![(active_path, Some(buffer.clone()))];
    for path in &prompt.other_file_paths {
        let loaded = other_buffers
            .iter()
            .find(|(buffer_path, _)| buffer_path == path)
            .map(|(_, buffer)| buffer.clone());
        candidates.push((path.clone(), loaded));
    }
    Some(Collected {
        request: prompt::build_request(prompt),
        active_buffer: buffer.clone(),
        candidates,
    })
}

/// The name of the innermost symbol around the cursor ("Active Code Item").
fn code_item(snapshot: &BufferSnapshot, offset: usize) -> Option<String> {
    let item = snapshot.symbols_containing(offset, None).pop()?;
    let name = item
        .name_ranges
        .iter()
        .filter_map(|range| item.text.get(range.clone()))
        .collect::<Vec<_>>()
        .join(" ");
    Some(if name.is_empty() {
        item.text.to_string()
    } else {
        name
    })
}

async fn run(
    collected: Collected,
    project: Option<WeakEntity<Project>>,
    request_id: u64,
    backend: Arc<dyn TabBackend>,
    api_key: String,
    cx: &mut AsyncApp,
) -> Option<CurrentPrediction> {
    let Collected {
        request,
        active_buffer,
        candidates,
    } = collected;
    let opening: Vec<(usize, Task<anyhow::Result<Entity<Buffer>>>)> = cx.update(|cx| {
        let Some(project) = project.and_then(|project| project.upgrade()) else {
            return Vec::new();
        };
        candidates
            .iter()
            .enumerate()
            .filter(|(_, (_, buffer))| buffer.is_none())
            .filter_map(|(index, (path, _))| {
                let task = project.update(cx, |project, cx| {
                    let project_path = project.find_project_path(path, cx)?;
                    Some(project.open_buffer(project_path, cx))
                })?;
                Some((index, task))
            })
            .collect()
    });

    let sent_at = SystemTime::now();
    let exchange = cx
        .background_spawn(backend.get_chat_message(api_key, request))
        .await;

    let mut buffers: Vec<Option<Entity<Buffer>>> = candidates
        .iter()
        .map(|(_, buffer)| buffer.clone())
        .collect();
    for (index, task) in opening {
        match task.await {
            Ok(buffer) => buffers[index] = Some(buffer),
            Err(error) => log::debug!(
                "windsurf: couldn't open {} for Tab Jump: {error:#}",
                candidates[index].0
            ),
        }
    }
    let paths: Vec<&str> = candidates.iter().map(|(path, _)| path.as_str()).collect();
    let result = interpret(&exchange, &active_buffer, &paths, &buffers, request_id, cx).await;
    let (prediction, outcome, target) = match result {
        Ok((prediction, target)) => {
            let outcome = if prediction.jump.is_some() {
                "jump to another file"
            } else {
                "jump within the file"
            };
            (Some(prediction), outcome.to_string(), target)
        }
        Err(reason) => (None, reason, serde_json::Value::Null),
    };
    log::info!(
        "windsurf: Tab Jump finished in {} ms: {outcome}",
        exchange.elapsed.as_millis()
    );
    let summary = summary(&exchange, sent_at, &outcome, target);
    cx.update(|cx| {
        let state = cx.default_global::<TabJumpState>();
        state.last = Some(summary);
        if let Some(prediction) = prediction
            .as_ref()
            .filter(|prediction| prediction.jump.is_some())
        {
            state.landing = Some(CurrentPrediction {
                jump: None,
                ..prediction.clone()
            });
        }
    });
    prediction
}

async fn interpret(
    exchange: &ChatExchange,
    active_buffer: &Entity<Buffer>,
    paths: &[&str],
    buffers: &[Option<Entity<Buffer>>],
    request_id: u64,
    cx: &mut AsyncApp,
) -> Result<(CurrentPrediction, serde_json::Value), String> {
    let stream = exchange
        .result
        .as_ref()
        .map_err(|error| format!("failed: {error}"))?;
    let completion = match response::parse_completion(&stream.text) {
        Ok(Some(completion)) => completion,
        Ok(None) => return Err("no jump".to_string()),
        Err(ParseError::Incomplete) => return Err("the completion ended early".to_string()),
        Err(ParseError::Malformed) => {
            return Err("the completion isn't in the expected format".to_string());
        }
    };
    let available: Vec<(&str, Entity<Buffer>, BufferSnapshot, String)> = cx.update(|cx| {
        paths
            .iter()
            .zip(buffers)
            .filter_map(|(path, buffer)| {
                let buffer = buffer.clone()?;
                let snapshot = buffer.read(cx).snapshot();
                let text = snapshot.text();
                Some((*path, buffer, snapshot, text))
            })
            .collect()
    });
    let texts: Vec<&str> = available
        .iter()
        .map(|(_, _, _, text)| text.as_str())
        .collect();
    let (index, target_match) = response::locate_in_files(&texts, &completion.target_content)
        .map_err(|error| {
            match error {
                LocateError::Empty => "the jump target is empty",
                LocateError::NotFound => "the jump target isn't in the file",
                LocateError::Ambiguous => "the jump target occurs more than once",
            }
            .to_string()
        })?;
    let (path, target_buffer, snapshot, text) = &available[index];
    let (target_text, _) =
        response::split_cursor_marker(&completion.target_content.replace("\r\n", "\n"));
    let edits = prediction_edits(
        text,
        target_match.range.clone(),
        &target_text,
        &completion.replacement_content,
    )
    .map_err(|error| format!("couldn't diff the jump target: {error:?}"))?;
    let Some((first_edit, _)) = edits.first() else {
        return Err("the completion doesn't change the jump target".to_string());
    };
    let cursor = target_match.cursor.unwrap_or(first_edit.start);
    let edits: Arc<[(Range<Anchor>, Arc<str>)]> = edits
        .into_iter()
        .map(|(range, text)| {
            (
                snapshot.anchor_after(range.start)..snapshot.anchor_before(range.end),
                Arc::from(text),
            )
        })
        .collect();
    let edit_preview = target_buffer
        .read_with(cx, |buffer, cx| buffer.preview_edits(edits.clone(), cx))
        .await;
    let jump = (target_buffer != active_buffer).then(|| JumpOrigin {
        buffer: active_buffer.clone(),
        snapshot: available[0].2.clone(),
        target: snapshot.anchor_before(cursor),
    });
    let cursor_point: Point = snapshot.offset_to_point(cursor);
    let target = serde_json::json!({
        "path": path,
        "row": cursor_point.row,
        "column": cursor_point.column,
        "target_content": completion.target_content,
        "replacement_content": completion.replacement_content,
    });
    Ok((
        CurrentPrediction {
            id: SharedString::from(format!("windsurf-jump-{request_id}")),
            buffer: target_buffer.clone(),
            snapshot: snapshot.clone(),
            edits,
            edit_preview,
            jump,
            record: None,
            shown_at: None,
        },
        target,
    ))
}

fn summary(
    exchange: &ChatExchange,
    sent_at: SystemTime,
    outcome: &str,
    target: serde_json::Value,
) -> serde_json::Value {
    let mut summary = serde_json::json!({
        "sent_at_unix_ms": sent_at.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64,
        "outcome": outcome,
        "elapsed_ms": exchange.elapsed.as_millis() as u64,
        "first_delta_ms": exchange.first_delta_after.map(|duration| duration.as_millis() as u64),
        "target": target,
    });
    match &exchange.result {
        Ok(stream) => {
            summary["completion"] = stream.text.clone().into();
            summary["stop_reason"] = stream.stop_reason.clone().into();
            summary["deltas"] = stream.deltas.into();
        }
        Err(error) => summary["error"] = error.to_string().into(),
    }
    summary["request"] = exchange
        .request
        .as_ref()
        .map(request_summary)
        .unwrap_or_default();
    summary
}

/// The request with the key and token replaced by their lengths and long
/// messages shortened.
pub fn request_summary(request: &GetChatMessageRequest) -> serde_json::Value {
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
    if let Some(prompts) = value
        .get_mut("chat_message_prompts")
        .and_then(|prompts| prompts.as_array_mut())
    {
        for message in prompts {
            if let Some(prompt) = message.get_mut("prompt") {
                let text = prompt.as_str().unwrap_or_default();
                if text.len() > MAX_SUMMARY_MESSAGE_LEN {
                    let mut end = MAX_SUMMARY_MESSAGE_LEN;
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    *prompt = format!("{}… ({} more bytes)", &text[..end], text.len() - end).into();
                }
            }
            if let Some(ranges) = message
                .get_mut("prompt_annotation_ranges")
                .and_then(|ranges| ranges.as_array_mut())
            {
                for range in ranges {
                    if let Some(suffix) = range.get_mut("suffix") {
                        let length = suffix.as_str().map(str::len).unwrap_or_default();
                        *suffix = format!("<{length} bytes>").into();
                    }
                }
            }
        }
    }
    value
}
