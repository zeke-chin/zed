use std::{cell::Cell, rc::Rc};

use anyhow::Result;
use editor::Editor;
use gpui::{AsyncWindowContext, Context, Entity, TaskExt as _, WeakEntity, Window};
use language::{Buffer, BufferEditSource, BufferEvent, BufferSnapshot};
use settings::Settings as _;

use crate::{
    JsonExplorerSettings,
    convert::{
        DocumentKind, document_kind, finish_in_flight, is_enabled, json_language, replace_document,
    },
    core::json_utils::{FormatSourceKind, JsonFormatResult, format_json_or_json_string},
    state::{ConvertHintKind, ConvertState},
};

/// Pastes larger than this are left alone so detection never holds on to
/// unbounded memory.
const MAX_DETECT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PasteAction {
    ConvertNow,
    Hint(ConvertHintKind),
}

pub(crate) fn paste_action(
    result: &JsonFormatResult,
    settings: &JsonExplorerSettings,
) -> PasteAction {
    match result.source_kind {
        FormatSourceKind::Json | FormatSourceKind::JsonStr => PasteAction::ConvertNow,
        FormatSourceKind::PythonStr if settings.python_repr_auto_convert => PasteAction::ConvertNow,
        FormatSourceKind::PythonStr => PasteAction::Hint(ConvertHintKind::PythonRepr),
        FormatSourceKind::JsonRepaired if settings.line_break_repair_auto_convert => {
            PasteAction::ConvertNow
        }
        FormatSourceKind::JsonRepaired => PasteAction::Hint(ConvertHintKind::LineBreakRepair),
    }
}

fn starts_like_json(snapshot: &BufferSnapshot) -> bool {
    snapshot
        .chars_at(0)
        .find(|character| !matches!(character, ' ' | '\t' | '\n' | '\r'))
        .is_some_and(|character| matches!(character, '{' | '[' | '"'))
}

fn is_whole_replacement(
    edits: &[language::Edit<usize>],
    previous_length: usize,
    new_length: usize,
) -> bool {
    new_length > 0
        && edits.len() == 1
        && edits[0].old == (0..previous_length)
        && edits[0].new == (0..new_length)
}

pub(crate) fn watch_buffer(buffer: &Entity<Buffer>, window: &mut Window, cx: &mut Context<Editor>) {
    let subscription = buffer.update(cx, |buffer, _| buffer.subscribe());
    let mut previous_length = buffer.read(cx).text_snapshot().len();
    let reload_generation = Rc::new(Cell::new(0usize));
    let buffer_id = buffer.read(cx).remote_id();

    cx.observe_release(buffer, move |_, _, cx| {
        let state = ConvertState::global(cx);
        state.update(cx, |state, cx| state.clear_hint(buffer_id, cx));
    })
    .detach();

    cx.subscribe_in(
        buffer,
        window,
        move |_, buffer, event: &BufferEvent, window, cx| match event {
            BufferEvent::Reloaded => reload_generation.set(reload_generation.get() + 1),
            BufferEvent::Edited { source } => {
                let patch = subscription.consume();
                let new_length = buffer.read(cx).text_snapshot().len();
                let old_length = std::mem::replace(&mut previous_length, new_length);
                if *source == BufferEditSource::User
                    && is_whole_replacement(patch.edits(), old_length, new_length)
                {
                    on_whole_replacement(buffer, &reload_generation, window, cx);
                }
            }
            _ => {}
        },
    )
    .detach();
}

fn on_whole_replacement(
    buffer: &Entity<Buffer>,
    reload_generation: &Rc<Cell<usize>>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    if !is_enabled(cx) || document_kind(buffer.read(cx)) != DocumentKind::PlainText {
        return;
    }
    let snapshot = buffer.read(cx).snapshot();
    if snapshot.len() > MAX_DETECT_BYTES || !starts_like_json(&snapshot) {
        return;
    }
    let state = ConvertState::global(cx);
    let buffer_id = snapshot.remote_id();
    if !state.update(cx, |state, _| state.try_begin(buffer_id)) {
        return;
    }
    let started_generation = reload_generation.get();
    let reload_generation = reload_generation.clone();
    let buffer = buffer.clone();
    cx.spawn_in(window, async move |editor, cx| {
        let outcome = detect_and_apply(
            &editor,
            &buffer,
            snapshot,
            &reload_generation,
            started_generation,
            cx,
        )
        .await;
        finish_in_flight(&state, buffer_id, cx);
        outcome
    })
    .detach_and_log_err(cx);
}

async fn detect_and_apply(
    editor: &WeakEntity<Editor>,
    buffer: &Entity<Buffer>,
    snapshot: BufferSnapshot,
    reload_generation: &Rc<Cell<usize>>,
    started_generation: usize,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let version = snapshot.version().clone();
    let result = cx
        .background_executor()
        .spawn(async move { format_json_or_json_string(&snapshot.text()) })
        .await;

    let still_current = |buffer: &Buffer| {
        buffer.version() == version
            && buffer.is_dirty()
            && reload_generation.get() == started_generation
    };
    let settings = cx.update(|_, cx| JsonExplorerSettings::get_global(cx).clone())?;
    let current = buffer.read_with(cx, |buffer, cx| {
        is_enabled(cx) && document_kind(buffer) == DocumentKind::PlainText && still_current(buffer)
    });
    if !current {
        return Ok(());
    }

    let buffer_id = buffer.read_with(cx, |buffer, _| buffer.remote_id());
    let state = cx.update(|_, cx| ConvertState::global(cx))?;
    let action = result
        .as_ref()
        .map(|result| paste_action(result, &settings));
    match (result, action) {
        (Some(result), Some(PasteAction::ConvertNow)) => {
            let language = json_language(buffer, cx).await?;
            editor.update_in(cx, |editor, window, cx| {
                if !still_current(buffer.read(cx)) {
                    return;
                }
                replace_document(editor, buffer, result.formatted, Some(language), window, cx);
                state.update(cx, |state, cx| state.clear_hint(buffer_id, cx));
            })
        }
        (_, Some(PasteAction::Hint(kind))) => {
            buffer.update(cx, |buffer, cx| {
                let version = buffer.version();
                state.update(cx, |state, cx| state.set_hint(buffer_id, version, kind, cx));
            });
            Ok(())
        }
        _ => {
            state.update(cx, |state, cx| state.clear_hint(buffer_id, cx));
            Ok(())
        }
    }
}
