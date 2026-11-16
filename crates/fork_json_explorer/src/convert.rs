use std::sync::Arc;

use anyhow::{Context as _, Result};
use editor::{Editor, actions::MoveToEnd};
use gpui::{App, AsyncWindowContext, Context, Entity, TaskExt as _, WeakEntity, Window, actions};
use language::{Buffer, BufferId, BufferSnapshot, Language};
use multi_buffer::MultiBufferOffset;
use settings::Settings as _;

use crate::{
    JsonExplorerSettings,
    core::json_utils::{FormatSourceKind, format_json_or_json_string, stringify_json_text},
    state::ConvertState,
};

actions!(
    json_explorer,
    [
        /// Switches a JSON document between formatted JSON and a JSON string, or
        /// converts a plain text document holding Python repr or line-broken JSON to JSON.
        ToggleCurrentDocument
    ]
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DocumentKind {
    Json,
    PlainText,
    Other,
}

pub(crate) fn document_kind(buffer: &Buffer) -> DocumentKind {
    let Some(language) = buffer.language() else {
        return DocumentKind::PlainText;
    };
    match language.name().as_ref() {
        "JSON" | "JSONC" => DocumentKind::Json,
        "Plain Text" => DocumentKind::PlainText,
        _ => DocumentKind::Other,
    }
}

pub(crate) fn is_enabled(cx: &App) -> bool {
    JsonExplorerSettings::get_global(cx).enabled
}

/// The text a toggle would put in place of `text`, or `None` when nothing
/// applies. In JSON documents a JSON string becomes formatted JSON and
/// anything else that is a JSON container becomes a JSON string. In plain text
/// documents only Python repr and line-broken JSON are converted.
pub(crate) fn toggle_replacement(kind: DocumentKind, text: &str) -> Option<String> {
    match kind {
        DocumentKind::Json => match format_json_or_json_string(text) {
            Some(result) if result.source_kind == FormatSourceKind::JsonStr => {
                Some(result.formatted)
            }
            _ => stringify_json_text(text),
        },
        DocumentKind::PlainText => format_json_or_json_string(text)
            .filter(|result| {
                matches!(
                    result.source_kind,
                    FormatSourceKind::PythonStr | FormatSourceKind::JsonRepaired
                )
            })
            .map(|result| result.formatted),
        DocumentKind::Other => None,
    }
}

pub(crate) async fn json_language(
    buffer: &Entity<Buffer>,
    cx: &mut AsyncWindowContext,
) -> Result<Arc<Language>> {
    let registry = buffer
        .read_with(cx, |buffer, _| buffer.language_registry())
        .context("the buffer has no language registry")?;
    registry.language_for_name("JSON").await
}

pub(crate) fn finish_in_flight(
    state: &Entity<ConvertState>,
    buffer_id: BufferId,
    cx: &mut AsyncWindowContext,
) {
    state.update(cx, |state, _| state.finish(buffer_id));
}

/// Replaces the whole document in one undoable transaction that never groups
/// with the edit before it, switching the language first when requested.
pub(crate) fn replace_document(
    editor: &mut Editor,
    buffer: &Entity<Buffer>,
    text: String,
    language: Option<Arc<Language>>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    buffer.update(cx, |buffer, _| {
        buffer.finalize_last_transaction();
    });
    if let Some(language) = language {
        buffer.update(cx, |buffer, cx| buffer.set_language(Some(language), cx));
    }
    let length = buffer.read(cx).text_snapshot().len();
    editor.transact(window, cx, |editor, window, cx| {
        editor.edit(
            [(MultiBufferOffset(0)..MultiBufferOffset(length), text)],
            cx,
        );
        editor.move_to_end(&MoveToEnd, window, cx);
    });
    buffer.update(cx, |buffer, _| {
        buffer.finalize_last_transaction();
    });
}

pub(crate) fn toggle_current_document(
    editor: &WeakEntity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(editor) = editor.upgrade() else {
        return;
    };
    editor.update(cx, |editor, cx| toggle(editor, window, cx));
}

pub(crate) fn toggle(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    if !is_enabled(cx) || editor.read_only(cx) {
        return;
    }
    let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
        return;
    };
    let kind = document_kind(buffer.read(cx));
    if kind == DocumentKind::Other {
        return;
    }
    let state = ConvertState::global(cx);
    let buffer_id = buffer.read(cx).remote_id();
    if !state.update(cx, |state, _| state.try_begin(buffer_id)) {
        return;
    }
    let snapshot = buffer.read(cx).snapshot();
    cx.spawn_in(window, async move |editor, cx| {
        let outcome = run_toggle(&editor, &buffer, kind, snapshot, cx).await;
        finish_in_flight(&state, buffer_id, cx);
        outcome
    })
    .detach_and_log_err(cx);
}

async fn run_toggle(
    editor: &WeakEntity<Editor>,
    buffer: &Entity<Buffer>,
    kind: DocumentKind,
    snapshot: BufferSnapshot,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let version = snapshot.version().clone();
    let replacement = cx
        .background_executor()
        .spawn(async move { toggle_replacement(kind, &snapshot.text()) })
        .await;
    let Some(replacement) = replacement else {
        log::info!("json_explorer: nothing to convert in the current document");
        return Ok(());
    };
    let language = if kind == DocumentKind::PlainText {
        Some(json_language(buffer, cx).await?)
    } else {
        None
    };
    editor.update_in(cx, |editor, window, cx| {
        if buffer.read(cx).version() != version {
            return;
        }
        let buffer_id = buffer.read(cx).remote_id();
        replace_document(editor, buffer, replacement, language, window, cx);
        let state = ConvertState::global(cx);
        state.update(cx, |state, cx| state.clear_hint(buffer_id, cx));
    })
}
