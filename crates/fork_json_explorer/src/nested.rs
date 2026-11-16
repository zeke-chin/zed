use std::borrow::Cow;

use editor::Editor;
use gpui::{App, AppContext as _, Context, Entity, TaskExt as _, Window, actions};
use language::{Buffer, ToOffset as _};
use multi_buffer::MultiBuffer;
use settings::Settings as _;
use workspace::{Toast, notifications::NotificationId};

use crate::{
    JsonExplorerSettings,
    core::{
        json_utils::{RawStringValueHit, SourceKind, find_string_value_at_offset},
        markdown_detect::{PlainStringFormat, detect_string_format},
    },
    pending_opens,
};

actions!(
    json_explorer,
    [
        /// Opens the JSON string value under the cursor, parsed, in a new document to the right.
        ParseNestedValue
    ]
);

pub const LINK_PREFIX: &str = "zed-fork://json-explorer/";
const OPEN_LINK_PATH: &str = "open?token=";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenKind {
    Json,
    Python,
    Markdown,
    Text,
}

impl OpenKind {
    pub(crate) fn of(hit: &RawStringValueHit) -> Self {
        match (&hit.parsed_text, hit.source_kind) {
            (Some(_), SourceKind::Python) => Self::Python,
            (Some(_), SourceKind::Json) => Self::Json,
            (None, _) => match detect_string_format(&hit.raw_value) {
                PlainStringFormat::Markdown => Self::Markdown,
                PlainStringFormat::Text => Self::Text,
            },
        }
    }

    pub(crate) fn link_label(self) -> &'static str {
        match self {
            Self::Json => "▸ Open parsed JSON in side panel",
            Self::Python => "▸ Open parsed Python dict in side panel",
            Self::Markdown => "▸ Open markdown in side panel",
            Self::Text => "▸ Open value in side panel",
        }
    }

    fn title_prefix(self) -> &'static str {
        match self {
            Self::Json => "Parsed",
            Self::Python => "Parsed-py",
            Self::Markdown | Self::Text => "Value",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Json | Self::Python => "json",
            Self::Markdown => "md",
            Self::Text => "txt",
        }
    }

    /// `None` means Plain Text.
    pub(crate) fn language_name(self) -> Option<&'static str> {
        match self {
            Self::Json | Self::Python => Some("JSON"),
            Self::Markdown => Some("Markdown"),
            Self::Text => None,
        }
    }
}

/// The parsed (or raw) text of a hovered string value: what the side panel shows.
pub(crate) fn value_content(hit: &RawStringValueHit) -> &str {
    hit.parsed_text.as_deref().unwrap_or(&hit.raw_value)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NestedDocument {
    pub content: String,
    pub kind: OpenKind,
    pub title: String,
}

impl NestedDocument {
    pub(crate) fn from_hit(hit: &RawStringValueHit) -> Self {
        let kind = OpenKind::of(hit);
        Self {
            content: value_content(hit).to_string(),
            kind,
            title: format!(
                "{}-{}.{}",
                kind.title_prefix(),
                sanitize_key_path(&hit.key_path),
                kind.extension()
            ),
        }
    }

    pub(crate) fn at_offset(text: &str, offset: usize) -> Option<Self> {
        find_string_value_at_offset(text, offset).map(|hit| Self::from_hit(&hit))
    }
}

pub(crate) fn sanitize_key_path(key_path: &str) -> Cow<'_, str> {
    if key_path.is_empty() {
        return Cow::Borrowed("root");
    }
    let is_safe = |character: char| character.is_ascii_alphanumeric() || "._-".contains(character);
    if key_path.chars().all(is_safe) {
        return Cow::Borrowed(key_path);
    }
    Cow::Owned(
        key_path
            .chars()
            .map(|character| if is_safe(character) { character } else { '_' })
            .collect(),
    )
}

pub(crate) fn open_link(token: u64) -> String {
    format!("{LINK_PREFIX}{OPEN_LINK_PATH}{token}")
}

pub(crate) fn is_json_buffer(buffer: &Buffer) -> bool {
    buffer
        .language()
        .is_some_and(|language| matches!(language.name().as_ref(), "JSON" | "JSONC"))
}

pub(crate) fn attach(editor: &mut Editor, cx: &mut Context<Editor>) {
    let weak_editor = cx.entity().downgrade();
    editor
        .register_action(move |_: &ParseNestedValue, window, cx| {
            if let Some(editor) = weak_editor.upgrade() {
                parse_nested_value(editor, window, cx);
            }
        })
        .detach();
}

fn parse_nested_value(editor: Entity<Editor>, window: &mut Window, cx: &mut App) {
    if !JsonExplorerSettings::get_global(cx).enabled {
        return;
    }
    let cursor = {
        let editor = editor.read(cx);
        let head = editor.selections.newest_anchor().head();
        editor.buffer().read(cx).text_anchor_for_position(head, cx)
    };
    let Some((buffer, position)) = cursor else {
        return;
    };
    let offset = position.to_offset(&buffer.read(cx).snapshot());
    open_nested_value(editor, buffer, offset, window, cx);
}

/// Handles the part of a `zed-fork://json-explorer/` link after that prefix.
/// Returns `false` for paths this feature doesn't know.
pub fn handle_hover_link(path: &str, window: &mut Window, cx: &mut App) -> bool {
    let Some(token) = path
        .strip_prefix(OPEN_LINK_PATH)
        .and_then(|token| token.parse::<u64>().ok())
    else {
        return false;
    };

    let Some(pending) = pending_opens::get(token, cx) else {
        log::info!("json_explorer: hover link token {token} is no longer cached");
        return true;
    };
    let (Some(editor), Some(buffer)) = (pending.editor.upgrade(), pending.buffer.upgrade()) else {
        return true;
    };
    if buffer.read(cx).version().changed_since(&pending.version) {
        show_stale_link_toast(&editor, cx);
        return true;
    }
    open_nested_value(editor, buffer, pending.offset, window, cx);
    true
}

struct StaleHoverLink;

fn show_stale_link_toast(editor: &Entity<Editor>, cx: &mut App) {
    let Some(workspace) = editor.read(cx).workspace() else {
        return;
    };
    workspace.update(cx, |workspace, cx| {
        workspace.show_toast(
            Toast::new(
                NotificationId::unique::<StaleHoverLink>(),
                "The document changed after this hover was shown. Hover the value again to open it.",
            )
            .autohide(),
            cx,
        );
    });
}

fn open_nested_value(
    editor: Entity<Editor>,
    buffer: Entity<Buffer>,
    offset: usize,
    window: &mut Window,
    cx: &mut App,
) {
    if !is_json_buffer(buffer.read(cx)) {
        return;
    }
    let Some(workspace) = editor.read(cx).workspace() else {
        return;
    };
    let project = workspace.read(cx).project().clone();
    let languages = project.read(cx).languages().clone();
    let snapshot = buffer.read(cx).snapshot();
    let document_task =
        cx.background_spawn(async move { NestedDocument::at_offset(&snapshot.text(), offset) });

    window
        .spawn(cx, async move |cx| {
            let Some(document) = document_task.await else {
                return anyhow::Ok(());
            };
            let language = match document.kind.language_name() {
                Some(name) => match languages.language_for_name(name).await {
                    Ok(language) => Some(language),
                    Err(error) => {
                        log::warn!("json_explorer: language {name} is unavailable: {error:#}");
                        None
                    }
                },
                None => None,
            };
            let new_buffer = project
                .update(cx, |project, cx| project.create_buffer(language, false, cx))
                .await?;
            new_buffer.update(cx, |new_buffer, cx| {
                if let Some(transaction_id) = new_buffer.set_text(document.content, cx) {
                    new_buffer.forget_transaction(transaction_id);
                }
            });
            workspace.update_in(cx, |workspace, window, cx| {
                let origin_pane = workspace
                    .pane_for(&editor)
                    .unwrap_or_else(|| workspace.active_pane().clone());
                let target_pane = workspace.adjacent_pane_of(&origin_pane, window, cx);
                let multi_buffer =
                    cx.new(|cx| MultiBuffer::singleton(new_buffer, cx).with_title(document.title));
                let new_editor = cx.new(|cx| {
                    Editor::for_multibuffer(multi_buffer, Some(project.clone()), window, cx)
                });
                target_pane.update(cx, |pane, cx| {
                    pane.add_item(Box::new(new_editor), true, true, None, window, cx);
                });
            })
        })
        .detach_and_log_err(cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::json_utils::find_string_values_raw;

    fn document_for(text: &str, key_path: &str) -> NestedDocument {
        let hit = find_string_values_raw(text)
            .into_iter()
            .find(|hit| hit.key_path == key_path)
            .expect("key path should exist");
        NestedDocument::from_hit(&hit)
    }

    #[test]
    fn sanitizes_key_paths_like_the_typescript_version() {
        assert_eq!(
            sanitize_key_path("data.commands[6].desc"),
            "data.commands_6_.desc"
        );
        assert_eq!(sanitize_key_path("a.b_c-d.0"), "a.b_c-d.0");
        assert_eq!(sanitize_key_path(""), "root");
        assert_eq!(sanitize_key_path("数据.x"), "__.x");
    }

    #[test]
    fn nested_json_opens_as_parsed_json() {
        let document = document_for(r#"{"data":{"json_str":"{\"a\":1}"}}"#, "data.json_str");
        assert_eq!(document.kind, OpenKind::Json);
        assert_eq!(document.title, "Parsed-data.json_str.json");
        assert_eq!(document.content, "{\n\t\"a\": 1\n}");
        assert_eq!(document.kind.language_name(), Some("JSON"));
    }

    #[test]
    fn python_dict_opens_as_json() {
        let document = document_for(r#"{"p":"{'a': True}"}"#, "p");
        assert_eq!(document.kind, OpenKind::Python);
        assert_eq!(document.title, "Parsed-py-p.json");
        assert_eq!(document.content, "{\n\t\"a\": true\n}");
    }

    #[test]
    fn markdown_and_text_open_with_raw_value() {
        let markdown = "# 标题一\n这是一段 **Markdown** 测试文本。\n\n- 列表项 A\n- 列表项 B";
        let text = format!(
            r#"{{"m":{},"list":["plain\ttext"]}}"#,
            crate::core::json_value::quote_json_string(markdown)
        );
        let document = document_for(&text, "m");
        assert_eq!(document.kind, OpenKind::Markdown);
        assert_eq!(document.title, "Value-m.md");
        assert_eq!(document.content, markdown);

        let document = document_for(&text, "list[0]");
        assert_eq!(document.kind, OpenKind::Text);
        assert_eq!(document.title, "Value-list_0_.txt");
        assert_eq!(document.content, "plain\ttext");
        assert_eq!(document.kind.language_name(), None);
    }

    #[test]
    fn document_at_offset_requires_a_string_value() {
        let text = r#"{"key": "value", "number": 1}"#;
        let value_offset = text.find("value").expect("value should exist");
        assert_eq!(
            NestedDocument::at_offset(text, value_offset).map(|document| document.content),
            Some("value".to_string())
        );
        let number_offset = text.find('1').expect("number should exist");
        assert_eq!(NestedDocument::at_offset(text, number_offset), None);
        assert_eq!(NestedDocument::at_offset(text, 2), None);
    }

    #[test]
    fn open_link_round_trips_through_the_prefix() {
        let link = open_link(42);
        assert_eq!(link, "zed-fork://json-explorer/open?token=42");
        assert_eq!(
            link.strip_prefix(LINK_PREFIX)
                .and_then(|path| path.strip_prefix(OPEN_LINK_PATH)),
            Some("42")
        );
    }
}
