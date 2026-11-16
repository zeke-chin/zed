//! Wraps the editor's `SemanticsProvider` so that hovering a string value in a
//! JSON file appends a preview block to the original (LSP) hover. Every other
//! method, and hovers outside JSON files, go straight to the wrapped provider.

use std::{ops::Range, rc::Rc, sync::Arc};

use anyhow::Result;
use collections::{HashMap, HashSet};
use editor::{Editor, GotoDefinitionKind, RenameTarget, SemanticsProvider};
use futures::future::Shared;
use gpui::{App, AppContext as _, Context, Entity, Task, WeakEntity};
use language::{Buffer, BufferRow, BufferSnapshot, ToOffset as _};
use lsp::LanguageServerId;
use project::{
    DocumentHighlight, HoverBlock, HoverBlockKind, InlayHint, InvalidationStrategy, LocationLink,
    ProjectTransaction,
    lsp_store::{BufferSemanticTokens, CacheInlayHints},
};
use settings::Settings as _;
use text::BufferId;

use crate::{
    JsonExplorerSettings,
    core::{
        json_utils::{RawStringValueHit, find_string_value_at_offset},
        markdown_preview::close_dangling_code_fence,
    },
    nested::{OpenKind, is_json_buffer, open_link, value_content},
    pending_opens::{self, PendingOpen},
};

const MAX_PREVIEW_CHARS: usize = 2000;

pub(crate) fn attach(editor: &mut Editor, cx: &mut Context<Editor>) {
    let Some(inner) = editor.semantics_provider() else {
        return;
    };
    editor.set_semantics_provider(Some(Rc::new(JsonHoverProvider {
        inner,
        editor: cx.entity().downgrade(),
    })));
}

pub(crate) struct JsonHoverProvider {
    inner: Rc<dyn SemanticsProvider>,
    editor: WeakEntity<Editor>,
}

struct HoverPreview {
    hover: project::Hover,
    version: clock::Global,
    offset: usize,
}

fn compute_preview(snapshot: BufferSnapshot, offset: usize, token: u64) -> Option<HoverPreview> {
    let hit = find_string_value_at_offset(&snapshot.text(), offset)?;
    let range = snapshot.anchor_before(hit.offset)..snapshot.anchor_after(hit.offset + hit.length);
    Some(HoverPreview {
        hover: project::Hover {
            contents: vec![HoverBlock {
                text: hover_markdown(&hit, &open_link(token)),
                kind: HoverBlockKind::Markdown,
            }],
            range: Some(range),
            language: None,
        },
        version: snapshot.version().clone(),
        offset: hit.offset,
    })
}

impl SemanticsProvider for JsonHoverProvider {
    fn hover(
        &self,
        buffer: &Entity<Buffer>,
        position: text::Anchor,
        cx: &mut App,
    ) -> Option<Task<Option<Vec<project::Hover>>>> {
        let inner_request = self.inner.hover(buffer, position, cx);
        if !JsonExplorerSettings::get_global(cx).enabled || !is_json_buffer(buffer.read(cx)) {
            return inner_request;
        }

        let snapshot = buffer.read(cx).snapshot();
        let offset = position.to_offset(&snapshot);
        let token = pending_opens::reserve_token(cx);
        let preview_task =
            cx.background_spawn(async move { compute_preview(snapshot, offset, token) });
        let editor = self.editor.clone();
        let buffer = buffer.downgrade();
        Some(cx.spawn(async move |cx| {
            let mut hovers = match inner_request {
                Some(request) => request.await,
                None => None,
            };
            let Some(preview) = preview_task.await else {
                return hovers;
            };
            cx.update(|cx| {
                pending_opens::insert(
                    token,
                    PendingOpen {
                        editor,
                        buffer,
                        version: preview.version,
                        offset: preview.offset,
                    },
                    cx,
                )
            });
            hovers.get_or_insert_default().push(preview.hover);
            hovers
        }))
    }

    fn debugger_hover(
        &self,
        buffer: &Entity<Buffer>,
        position: text::Anchor,
        cx: &mut App,
    ) -> Option<Task<Option<project::DebuggerHover>>> {
        self.inner.debugger_hover(buffer, position, cx)
    }

    fn inline_values(
        &self,
        buffer_handle: Entity<Buffer>,
        range: Range<text::Anchor>,
        cx: &mut App,
    ) -> Option<Task<Result<Vec<InlayHint>>>> {
        self.inner.inline_values(buffer_handle, range, cx)
    }

    fn applicable_inlay_chunks(
        &self,
        buffer: &Entity<Buffer>,
        ranges: &[Range<text::Anchor>],
        cx: &mut App,
    ) -> Vec<Range<BufferRow>> {
        self.inner.applicable_inlay_chunks(buffer, ranges, cx)
    }

    fn invalidate_inlay_hints(&self, for_buffers: &HashSet<BufferId>, cx: &mut App) {
        self.inner.invalidate_inlay_hints(for_buffers, cx)
    }

    fn inlay_hints(
        &self,
        invalidate: InvalidationStrategy,
        buffer: Entity<Buffer>,
        ranges: Vec<Range<text::Anchor>>,
        known_chunks: Option<(clock::Global, HashSet<Range<BufferRow>>)>,
        cx: &mut App,
    ) -> Option<HashMap<Range<BufferRow>, Task<Result<CacheInlayHints>>>> {
        self.inner
            .inlay_hints(invalidate, buffer, ranges, known_chunks, cx)
    }

    fn semantic_tokens(
        &self,
        buffer: Entity<Buffer>,
        cx: &mut App,
    ) -> Option<Shared<Task<std::result::Result<BufferSemanticTokens, Arc<anyhow::Error>>>>> {
        self.inner.semantic_tokens(buffer, cx)
    }

    fn supports_inlay_hints(&self, buffer: &Entity<Buffer>, cx: &mut App) -> bool {
        self.inner.supports_inlay_hints(buffer, cx)
    }

    fn supports_semantic_tokens(&self, buffer: &Entity<Buffer>, cx: &mut App) -> bool {
        self.inner.supports_semantic_tokens(buffer, cx)
    }

    fn document_highlights(
        &self,
        buffer: &Entity<Buffer>,
        position: text::Anchor,
        cx: &mut App,
    ) -> Option<Task<Result<Vec<DocumentHighlight>>>> {
        self.inner.document_highlights(buffer, position, cx)
    }

    fn definitions(
        &self,
        buffer: &Entity<Buffer>,
        position: text::Anchor,
        kind: GotoDefinitionKind,
        cx: &mut App,
    ) -> Option<Task<Result<Option<Vec<LocationLink>>>>> {
        self.inner.definitions(buffer, position, kind, cx)
    }

    fn range_for_rename(
        &self,
        buffer: &Entity<Buffer>,
        position: text::Anchor,
        cx: &mut App,
    ) -> Task<Result<Option<RenameTarget>>> {
        self.inner.range_for_rename(buffer, position, cx)
    }

    fn perform_rename(
        &self,
        buffer: &Entity<Buffer>,
        position: text::Anchor,
        new_name: String,
        language_server_id: Option<LanguageServerId>,
        cx: &mut App,
    ) -> Option<Task<Result<ProjectTransaction>>> {
        self.inner
            .perform_rename(buffer, position, new_name, language_server_id, cx)
    }
}

/// Layout matches the VS Code version: title and key path, a link, the
/// preview, an optional truncation note, and the link again.
pub(crate) fn hover_markdown(hit: &RawStringValueHit, link_target: &str) -> String {
    let kind = OpenKind::of(hit);
    let title = match kind {
        OpenKind::Json => "Parsed JSON",
        OpenKind::Python => "Parsed Python dict",
        OpenKind::Markdown => "Markdown",
        OpenKind::Text => "String value",
    };
    let link = format!("[{}]({link_target})", kind.link_label());
    let (preview, truncated_note) = truncate_for_preview(value_content(hit));

    let mut parts = vec![
        format!("**{title}** {}", inline_code(&hit.key_path)),
        link.clone(),
    ];
    match kind {
        OpenKind::Json | OpenKind::Python => parts.push(code_block(preview, "json")),
        OpenKind::Text => parts.push(code_block(preview, "")),
        OpenKind::Markdown => {
            parts.push("---".to_string());
            parts.push(close_dangling_code_fence(preview));
        }
    }
    parts.extend(truncated_note);
    if kind == OpenKind::Markdown {
        parts.push("---".to_string());
    }
    parts.push(link);
    parts.join("\n\n")
}

fn truncate_for_preview(text: &str) -> (&str, Option<String>) {
    let Some((cut, _)) = text.char_indices().nth(MAX_PREVIEW_CHARS) else {
        return (text, None);
    };
    let omitted = text[cut..].chars().count();
    (
        &text[..cut],
        Some(format!(
            "_… 预览已截断，省略 {omitted} 字符。点击链接在侧边栏查看完整内容。_"
        )),
    )
}

fn longest_backtick_run(text: &str) -> usize {
    text.split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0)
}

fn code_block(text: &str, language: &str) -> String {
    let fence = "`".repeat((longest_backtick_run(text) + 1).max(3));
    format!("{fence}{language}\n{text}\n{fence}")
}

fn inline_code(text: &str) -> String {
    let delimiter = "`".repeat(longest_backtick_run(text) + 1);
    let padding = if text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{delimiter}{padding}{text}{padding}{delimiter}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{json_utils::find_string_values_raw, json_value::quote_json_string};

    const LINK: &str = "zed-fork://json-explorer/open?token=7";

    fn markdown_for(text: &str, key_path: &str) -> String {
        let hit = find_string_values_raw(text)
            .into_iter()
            .find(|hit| hit.key_path == key_path)
            .expect("key path should exist");
        hover_markdown(&hit, LINK)
    }

    /// The `README` example of the VS Code version, with the expected title of each value.
    fn readme_example() -> String {
        let markdown = "# 标题一\n这是一段 **Markdown** 测试文本。\n\n- 列表项 A\n- 列表项 B\n\n> 这是一个引用块。\n\n`inline code` 和代码块：\n\n```python\nprint('hello world')\n```";
        format!(
            r#"{{
  "test_id": 1024,
  "status": "success",
  "data": {{
    "plain_str": "这是一个普通的字符串，没有任何特殊格式。",
    "markdown_str": {},
    "json_str": "{{\"user_id\": 123, \"roles\": [\"admin\", \"editor\"], \"settings\": {{\"theme\": \"dark\", \"notifications\": true}}}}",
    "mixed_list": ["简单文本", "**加粗文本**", "{{\"key\": \"value\"}}"]
  }},
  "metadata": {{
    "created_at": "2023-10-27T10:00:00Z",
    "version": "1.0.0"
  }}
}}"#,
            quote_json_string(markdown)
        )
    }

    #[test]
    fn readme_example_titles_match_the_expected_table() {
        let text = readme_example();
        let expected = [
            ("status", "**String value**"),
            ("metadata.version", "**String value**"),
            ("data.plain_str", "**String value**"),
            ("data.mixed_list[0]", "**String value**"),
            ("data.mixed_list[1]", "**String value**"),
            ("data.markdown_str", "**Markdown**"),
            ("data.json_str", "**Parsed JSON**"),
            ("data.mixed_list[2]", "**Parsed JSON**"),
        ];
        for (key_path, title) in expected {
            let markdown = markdown_for(&text, key_path);
            assert!(
                markdown.starts_with(&format!("{title} `{key_path}`")),
                "{key_path}: {markdown}"
            );
        }
    }

    #[test]
    fn parsed_json_layout() {
        let markdown = markdown_for(r#"{"a":"{\"b\":1}"}"#, "a");
        let link = format!("[▸ Open parsed JSON in side panel]({LINK})");
        assert_eq!(
            markdown,
            format!("**Parsed JSON** `a`\n\n{link}\n\n```json\n{{\n\t\"b\": 1\n}}\n```\n\n{link}")
        );
    }

    #[test]
    fn python_dict_layout() {
        let markdown = markdown_for(r#"{"a":"{'b': None}"}"#, "a");
        assert!(markdown.starts_with("**Parsed Python dict** `a`"));
        assert!(markdown.contains("```json\n{\n\t\"b\": null\n}\n```"));
        assert_eq!(
            markdown
                .matches("[▸ Open parsed Python dict in side panel]")
                .count(),
            2
        );
    }

    #[test]
    fn plain_text_shows_real_line_breaks_in_a_code_block() {
        let markdown = markdown_for(r#"{"a":"line1\nline2\tend"}"#, "a");
        let link = format!("[▸ Open value in side panel]({LINK})");
        assert_eq!(
            markdown,
            format!("**String value** `a`\n\n{link}\n\n```\nline1\nline2\tend\n```\n\n{link}")
        );
    }

    #[test]
    fn code_fence_outgrows_backticks_in_the_value() {
        let markdown = markdown_for(r#"{"a":"x ``` y"}"#, "a");
        assert!(markdown.contains("````\nx ``` y\n````"), "{markdown}");
    }

    #[test]
    fn markdown_is_rendered_between_rules() {
        let text = readme_example();
        let markdown = markdown_for(&text, "data.markdown_str");
        let link = format!("[▸ Open markdown in side panel]({LINK})");
        assert!(markdown.starts_with(&format!(
            "**Markdown** `data.markdown_str`\n\n{link}\n\n---\n\n# 标题一\n"
        )));
        assert!(markdown.ends_with(&format!("```\n\n---\n\n{link}")));
    }

    #[test]
    fn long_values_are_truncated_with_a_note() {
        let value = "字".repeat(MAX_PREVIEW_CHARS + 5);
        let markdown = markdown_for(&format!(r#"{{"a":"{value}"}}"#), "a");
        assert!(markdown.contains(&format!(
            "```\n{}\n```\n\n_… 预览已截断，省略 5 字符。",
            "字".repeat(MAX_PREVIEW_CHARS)
        )));
        assert!(
            markdown
                .ends_with("[▸ Open value in side panel](zed-fork://json-explorer/open?token=7)")
        );
    }

    #[test]
    fn truncated_markdown_closes_its_code_fence() {
        let value = format!(
            "# Title\n\n- item a\n- item b\n\n```text\n{}",
            "x".repeat(MAX_PREVIEW_CHARS)
        );
        let markdown = markdown_for(&format!(r#"{{"a":{}}}"#, quote_json_string(&value)), "a");
        let note_index = markdown.find("_… 预览已截断").expect("note should exist");
        assert!(markdown[..note_index].ends_with("\n```\n\n"), "{markdown}");
    }

    #[test]
    fn inline_code_escapes_backticks() {
        assert_eq!(inline_code("a.b"), "`a.b`");
        assert_eq!(inline_code("a`b"), "``a`b``");
        assert_eq!(inline_code("`a"), "`` `a ``");
    }

    #[test]
    fn short_values_are_not_truncated() {
        let (preview, note) = truncate_for_preview("short");
        assert_eq!(preview, "short");
        assert_eq!(note, None);
        let exact = "x".repeat(MAX_PREVIEW_CHARS);
        assert_eq!(truncate_for_preview(&exact).1, None);
    }
}
