//! Assembles the Tab Jump prompt the way the official language server does
//! (`fork/spike/结论.md` 问题 18): a fixed system prompt, then user messages
//! that show the files involved, the recent edits as `[diff_block_start]`
//! blocks, the current file with `<|cursor|>` inside `<additional_metadata>`,
//! and finally a half-written `<replace_file_content>` system message that the
//! model continues. The wording is copied from the captured requests; only the
//! structure is reproduced, never captured file contents.
//!
//! The official client truncates by a token budget
//! (`TAB_JUMP_CUMULATIVE_PROMPT_CONFIG`); the limits below follow its
//! multipliers on the 10240 token context (`MODEL_CHAT_20706_TOKENS`) and are
//! documented in `fork/docs/5.Windsurf完整上下文.md`.

use collections::HashMap;

use crate::{
    native::get_tab::INSTRUCTION,
    proto::exa::{
        api_server_pb::{ChatMessageRequestType, GetChatMessageRequest},
        chat_pb::{CacheControlType, ChatMessagePrompt, PromptCacheOptions},
        codeium_common_pb::{
            ChatMessageSource, CompletionConfiguration, Language, PromptAnnotationKind,
            PromptAnnotationRange, ProviderSource,
        },
        cortex_pb::{action_result, cortex_trajectory_step},
        diff_action_pb::{UnifiedDiffLineType, unified_diff::UnifiedDiffLine},
    },
};

pub const CURSOR_MARKER: &str = "<|cursor|>";

pub const SYSTEM_PROMPT: &str = "You are Cascade, a powerful agentic AI coding assistant designed by the Codeium engineering team: a world-class AI company based in Silicon Valley, California.\nExclusively available in Devin Desktop, the agentic IDE, you operate on the revolutionary AI Flow paradigm, enabling you to work both independently and collaboratively with a USER.\nYou are pair programming with a USER to solve their coding task. The task may require creating a new codebase, modifying or debugging an existing codebase, or simply answering a question.\nThe USER will send you requests, which you must always prioritize addressing. Along with each USER request, we will attach additional metadata about their current state, such as what files they have open and where their cursor is.\nThis information may or may not be relevant to the coding task, it is up for you to decide.";

pub const STOP_PATTERNS: [&str; 5] = [
    "<|user|>",
    "<|bot|>",
    "<|context_request|>",
    "<|endoftext|>",
    "<|end_of_turn|>",
];
pub const MAX_OUTPUT_TOKENS: u64 = 2048;

/// `MODEL_CHAT_20706_TOKENS`.
pub const CONTEXT_TOKENS: usize = 10240;
/// `persistent_active_document_multiplier = 0.45`.
pub const ACTIVE_DOCUMENT_TOKENS: usize = CONTEXT_TOKENS * 45 / 100;
/// `trajectory_context_multiplier = 0.5` times `trajectory_truncation_multiplier
/// = 0.5`: what is left after the official client truncates the edit history.
pub const HISTORY_TOKENS: usize = CONTEXT_TOKENS / 4;
/// `persistent_open_docs_multiplier = 0.25`, shared by all file views.
pub const FILE_VIEW_TOKENS: usize = CONTEXT_TOKENS / 4;
/// `persistent_max_tokens_per_open_doc`.
pub const MAX_TOKENS_PER_FILE_VIEW: usize = 2048;
pub const MAX_HISTORY_EVENTS: usize = 30;
pub const MAX_OTHER_DOCUMENTS: usize = 10;
/// Views smaller than this aren't worth sending.
const MIN_FILE_VIEW_TOKENS: usize = 64;
const HUNK_CONTEXT_LINES: usize = 3;

const DIFF_NOTE: &str = "Please note that the above snippet only shows the MODIFIED lines from the last change. It shows up to 3 lines of unchanged lines before and after the modified lines. The actual file contents may have many more lines not shown.";
const IDE_ACTION: &str = "The USER performed the following action in the IDE:";

/// A rough count: the captured `num_tokens` values are about one token per
/// four bytes of source code.
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActiveDocument {
    pub path: String,
    pub language: Language,
    pub editor_language: String,
    pub text: String,
    /// UTF-8 byte offset into `text`.
    pub cursor_offset: usize,
    /// The innermost symbol around the cursor, e.g. a function name.
    pub code_item: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OpenDocument {
    pub path: String,
    pub language: Language,
    /// Only needed for files whose edits are in the history.
    pub text: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HistoryEvent {
    Edit {
        path: String,
        /// The hunks between `[diff_block_start]` and `[diff_block_end]`.
        diff: String,
        /// 0-based row of the first change.
        first_changed_row: usize,
        /// The file before this edit, when this is the first edit to it that
        /// the trajectory recorded.
        original_content: Option<String>,
    },
    Clipboard(String),
}

#[derive(Clone, Debug)]
pub struct PromptInput {
    pub active: ActiveDocument,
    pub other_documents: Vec<OpenDocument>,
    /// Oldest first.
    pub history: Vec<HistoryEvent>,
    /// e.g. "Nov 16, 2026 at 7:44am, UTC-05:00".
    pub presented_at: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TabJumpPrompt {
    pub messages: Vec<ChatMessagePrompt>,
    pub language: Language,
    pub target_path: String,
    /// Other files whose contents are in the prompt, in prompt order. Only
    /// these can be jumped to besides the target file.
    pub other_file_paths: Vec<String>,
}

/// The `CODE_ACTION` and `CLIPBOARD` steps of a trajectory as prompt events.
pub fn history_from_steps(
    steps: &[crate::proto::exa::cortex_pb::CortexTrajectoryStep],
) -> Vec<HistoryEvent> {
    let mut events = Vec::new();
    for step in steps {
        match &step.step {
            Some(cortex_trajectory_step::Step::CodeAction(action)) => {
                let Some(action_result::Result::Edit(edit)) = action
                    .action_result
                    .as_ref()
                    .and_then(|result| result.result.as_ref())
                else {
                    continue;
                };
                let Some(diff) = edit.diff.as_ref() else {
                    continue;
                };
                let lines = diff
                    .unified_diff
                    .as_ref()
                    .map(|unified_diff| unified_diff.lines.as_slice())
                    .unwrap_or_default();
                let Some(rendered) = render_hunks(diff.start_line.max(0) as usize, lines) else {
                    continue;
                };
                events.push(HistoryEvent::Edit {
                    path: uri_to_path(&edit.absolute_uri),
                    diff: rendered.text,
                    first_changed_row: rendered.first_changed_row,
                    original_content: (!edit.original_content.is_empty())
                        .then(|| edit.original_content.clone()),
                });
            }
            Some(cortex_trajectory_step::Step::Clipboard(clipboard))
                if !clipboard.content.is_empty() =>
            {
                events.push(HistoryEvent::Clipboard(clipboard.content.clone()));
            }
            _ => {}
        }
    }
    events
}

pub fn uri_to_path(uri: &str) -> String {
    url::Url::parse(uri)
        .ok()
        .filter(|url| url.scheme() == "file")
        .and_then(|url| url.to_file_path().ok())
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| uri.strip_prefix("file://").unwrap_or(uri).to_string())
}

pub struct RenderedHunks {
    pub text: String,
    pub first_changed_row: usize,
}

/// Re-renders a step's diff (ten lines of context) as unified diff hunks with
/// three lines of context, splitting where more than six unchanged lines
/// separate two changes. `start_line` is the 0-based row of the first line;
/// rows before the first change are the same in the old and new text, so
/// both sides start there.
pub fn render_hunks(start_line: usize, lines: &[UnifiedDiffLine]) -> Option<RenderedHunks> {
    struct Numbered<'a> {
        marker: char,
        text: &'a str,
        old_line: usize,
        new_line: usize,
    }
    let mut old_line = start_line + 1;
    let mut new_line = start_line + 1;
    let mut numbered = Vec::with_capacity(lines.len());
    for line in lines {
        let marker = match UnifiedDiffLineType::try_from(line.r#type) {
            Ok(UnifiedDiffLineType::Insert) => '+',
            Ok(UnifiedDiffLineType::Delete) => '-',
            _ => ' ',
        };
        numbered.push(Numbered {
            marker,
            text: &line.text,
            old_line,
            new_line,
        });
        match marker {
            '+' => new_line += 1,
            '-' => old_line += 1,
            _ => {
                old_line += 1;
                new_line += 1;
            }
        }
    }
    let changed: Vec<usize> = numbered
        .iter()
        .enumerate()
        .filter(|(_, line)| line.marker != ' ')
        .map(|(index, _)| index)
        .collect();
    let first_changed = *changed.first()?;

    let mut hunks: Vec<(usize, usize)> = Vec::new();
    for index in changed {
        let start = index.saturating_sub(HUNK_CONTEXT_LINES);
        let end = (index + HUNK_CONTEXT_LINES + 1).min(numbered.len());
        match hunks.last_mut() {
            Some((_, last_end)) if start <= *last_end => *last_end = end,
            _ => hunks.push((start, end)),
        }
    }
    let mut text = String::new();
    for (start, end) in hunks {
        let hunk = &numbered[start..end];
        let old_count = hunk.iter().filter(|line| line.marker != '+').count();
        let new_count = hunk.iter().filter(|line| line.marker != '-').count();
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!(
            "@@ -{},{old_count} +{},{new_count} @@",
            hunk[0].old_line, hunk[0].new_line
        ));
        for line in hunk {
            text.push('\n');
            text.push(line.marker);
            text.push_str(line.text);
        }
    }
    Some(RenderedHunks {
        text,
        first_changed_row: numbered[first_changed].new_line - 1,
    })
}

/// A run of whole lines from a text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineWindow {
    /// 0-based, end exclusive.
    pub start_row: usize,
    pub end_row: usize,
    pub start_offset: usize,
    pub text: String,
}

/// The whole text when it fits in `max_tokens`, otherwise the lines around
/// `focus_row`, growing below and above in turn. One trailing newline is
/// dropped so that "lines 1 to N" counts the lines that are shown.
pub fn line_window(text: &str, focus_row: usize, max_tokens: usize) -> LineWindow {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let lines: Vec<&str> = text.split('\n').collect();
    if estimate_tokens(text) <= max_tokens {
        return LineWindow {
            start_row: 0,
            end_row: lines.len(),
            start_offset: 0,
            text: text.to_string(),
        };
    }
    let focus_row = focus_row.min(lines.len() - 1);
    let mut start_row = focus_row;
    let mut end_row = focus_row + 1;
    let mut tokens = estimate_tokens(lines[focus_row]);
    loop {
        let mut grew = false;
        if end_row < lines.len() {
            let cost = estimate_tokens(lines[end_row]) + 1;
            if tokens + cost <= max_tokens {
                tokens += cost;
                end_row += 1;
                grew = true;
            }
        }
        if start_row > 0 {
            let cost = estimate_tokens(lines[start_row - 1]) + 1;
            if tokens + cost <= max_tokens {
                tokens += cost;
                start_row -= 1;
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    let start_offset: usize = lines[..start_row].iter().map(|line| line.len() + 1).sum();
    LineWindow {
        start_row,
        end_row,
        start_offset,
        text: lines[start_row..end_row].join("\n"),
    }
}

fn user_message(text: String, safe_for_code_telemetry: bool) -> ChatMessagePrompt {
    ChatMessagePrompt {
        source: ChatMessageSource::User as i32,
        num_tokens: estimate_tokens(&text) as u32,
        prompt: text,
        safe_for_code_telemetry,
        ..Default::default()
    }
}

fn mark_cache_breakpoint(message: Option<&mut ChatMessagePrompt>) {
    if let Some(message) = message {
        message.prompt_cache_options = Some(PromptCacheOptions {
            r#type: CacheControlType::Ephemeral as i32,
        });
    }
}

fn history_message(event: &HistoryEvent) -> String {
    match event {
        HistoryEvent::Edit { path, diff, .. } => format!(
            "The following changes were made by the USER to: {path}.\n[diff_block_start]\n{diff}\n[diff_block_end]\n\n{DIFF_NOTE}"
        ),
        HistoryEvent::Clipboard(content) => {
            format!("{IDE_ACTION}\nI copied the following text to the clipboard: {content}")
        }
    }
}

struct FileView {
    path: String,
    window: LineWindow,
}

fn json_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| format!("\"{text}\""))
}

pub fn build_prompt(input: &PromptInput) -> TabJumpPrompt {
    let active = &input.active;

    let mut history = Vec::new();
    let mut history_tokens = 0;
    for event in input.history.iter().rev().take(MAX_HISTORY_EVENTS) {
        let message = history_message(event);
        let tokens = estimate_tokens(&message);
        if history_tokens + tokens > HISTORY_TOKENS {
            break;
        }
        history_tokens += tokens;
        history.push((event, message));
    }
    history.reverse();

    // What each edited file looked like before the edits that are shown, or
    // its current text when that isn't known; focused on its first change.
    let mut files: Vec<(String, Option<String>, usize)> = Vec::new();
    for (event, _) in &history {
        let HistoryEvent::Edit {
            path,
            first_changed_row,
            original_content,
            ..
        } = event
        else {
            continue;
        };
        match files.iter_mut().find(|(file_path, _, _)| file_path == path) {
            Some((_, content, _)) => {
                if content.is_none() {
                    *content = original_content.clone();
                }
            }
            None => files.push((path.clone(), original_content.clone(), *first_changed_row)),
        }
    }
    let current_texts: HashMap<&str, &str> = input
        .other_documents
        .iter()
        .filter_map(|document| Some((document.path.as_str(), document.text.as_deref()?)))
        .collect();
    let cursor_offset = active.cursor_offset.min(active.text.len());
    let cursor_row = active.text[..cursor_offset].matches('\n').count();

    let mut view_tokens = 0;
    let mut view_for = |path: &str, content: &str, focus_row: usize| {
        let budget = MAX_TOKENS_PER_FILE_VIEW.min(FILE_VIEW_TOKENS.saturating_sub(view_tokens));
        if budget < MIN_FILE_VIEW_TOKENS {
            return None;
        }
        let window = line_window(content, focus_row, budget);
        view_tokens += estimate_tokens(&window.text);
        Some(FileView {
            path: path.to_string(),
            window,
        })
    };
    let active_view = {
        let (content, focus_row) = match files.iter().find(|(path, _, _)| *path == active.path) {
            Some((_, Some(original), first_changed_row)) => (original.as_str(), *first_changed_row),
            _ => (active.text.as_str(), cursor_row),
        };
        view_for(&active.path, content, focus_row)
    };
    let mut other_views = Vec::new();
    for (path, original, focus_row) in files.iter().rev() {
        if *path == active.path {
            continue;
        }
        let Some(content) = original
            .as_deref()
            .or_else(|| current_texts.get(path.as_str()).copied())
        else {
            continue;
        };
        if let Some(view) = view_for(path, content, *focus_row) {
            other_views.push(view);
        }
    }
    other_views.reverse();

    let mut messages = Vec::new();
    for view in other_views.iter().chain(active_view.iter()) {
        messages.push(user_message(
            format!(
                "{IDE_ACTION}\nShow the contents of file {} from lines {} to {}",
                view.path,
                view.window.start_row + 1,
                view.window.end_row
            ),
            true,
        ));
        messages.push(user_message(view.window.text.clone(), false));
    }
    mark_cache_breakpoint(messages.last_mut());
    let history_start = messages.len();
    for (event, message) in history {
        let is_ide_action = matches!(event, HistoryEvent::Clipboard(_));
        messages.push(user_message(message, is_ide_action));
    }
    if messages.len() > history_start {
        mark_cache_breakpoint(messages.last_mut());
    }

    let (window, cursor_in_window) = active_window(&active.text, cursor_offset);
    let mut block = window.text.clone();
    block.insert_str(cursor_in_window, CURSOR_MARKER);
    let mut metadata = format!(
        "<additional_metadata>\nNOTE: Open files and cursor position may not be related to the user's current request. Always verify relevance before assuming connection.\n\nThe USER presented this request to you on {}.\n\nThe current state of the user's IDE is as follows:\nActive Document: {} ({})\nCursor is on line: {}\n",
        input.presented_at,
        active.path,
        active.language.as_str_name(),
        cursor_row + 1,
    );
    let other_documents: Vec<_> = input
        .other_documents
        .iter()
        .filter(|document| document.path != active.path)
        .take(MAX_OTHER_DOCUMENTS)
        .collect();
    if !other_documents.is_empty() {
        metadata.push_str("Other open documents:\n");
        for document in other_documents {
            metadata.push_str(&format!(
                "- {} ({})\n",
                document.path,
                document.language.as_str_name()
            ));
        }
    }
    if let Some(code_item) = &active.code_item {
        metadata.push_str(&format!("Active Code Item: {code_item}\n"));
    }
    metadata.push_str(&format!(
        "Only use this information if it is directly relevant to the user's request.\n\nThe user has mentioned some items in the form @[ITEM]. Here is extra information about the items that were mentioned by the user, in the order that they appear:\n\n@[{}:L{}-L{}] is a [Text Block]:\n{block}\n</additional_metadata>\n<user_request>\n{INSTRUCTION}\n</user_request>",
        active.path,
        window.start_row + 1,
        window.end_row,
    ));
    messages.push(user_message(metadata, true));

    let partial = format!(
        "<replace_file_content>{{\n\t\"TargetFile\": {},\n\t\"CodeMarkdownLanguage\": {},\n\t\"Instruction\": {},\n\t\"Blocking\": true,\n\t\"ReplacementChunks\": [{{\n\t\t\"AllowMultiple\": false,\n\t\t\"TargetContent\":",
        json_string(&active.path),
        json_string(&active.editor_language),
        json_string(INSTRUCTION),
    );
    // Lets the server speculate that the model copies the current text.
    let copy_suffix = format!(
        " {},\n\t\t\"ReplacementContent\": \"\"\n\t}}]\n}}</replace_file_content>\n",
        json_string(&window.text)
    );
    messages.push(ChatMessagePrompt {
        source: ChatMessageSource::System as i32,
        prompt_annotation_ranges: vec![PromptAnnotationRange {
            kind: PromptAnnotationKind::Copy as i32,
            byte_offset_start: partial.len() as u64,
            byte_offset_end: partial.len() as u64,
            suffix: copy_suffix,
        }],
        prompt: partial,
        ..Default::default()
    });

    TabJumpPrompt {
        messages,
        language: active.language,
        target_path: active.path.clone(),
        other_file_paths: other_views.into_iter().map(|view| view.path).collect(),
    }
}

/// The part of the active document that is shown, and the cursor's offset in it.
fn active_window(text: &str, cursor_offset: usize) -> (LineWindow, usize) {
    let cursor_row = text[..cursor_offset].matches('\n').count();
    // Keep a trailing newline when the cursor sits after it.
    let shown = if cursor_offset == text.len() && text.ends_with('\n') {
        format!("{text}\n")
    } else {
        text.to_string()
    };
    let window = line_window(&shown, cursor_row, ACTIVE_DOCUMENT_TOKENS);
    let cursor_in_window = (cursor_offset - window.start_offset).min(window.text.len());
    (window, cursor_in_window)
}

/// The request without `metadata`, model and experiments, which the client
/// fills in (`native/get_chat_message.rs`).
pub fn build_request(prompt: TabJumpPrompt) -> GetChatMessageRequest {
    GetChatMessageRequest {
        prompt: SYSTEM_PROMPT.to_string(),
        chat_message_prompts: prompt.messages,
        request_type: ChatMessageRequestType::Command as i32,
        configuration: Some(CompletionConfiguration {
            num_completions: 1,
            max_tokens: MAX_OUTPUT_TOKENS,
            temperature: 0.1,
            top_k: 50,
            top_p: 1.0,
            stop_patterns: STOP_PATTERNS
                .iter()
                .map(|pattern| pattern.to_string())
                .collect(),
            fim_eot_prob_threshold: 1.0,
            last_message_is_partial: true,
            ..Default::default()
        }),
        prompt_id: uuid::Uuid::new_v4().to_string(),
        provider_source: ProviderSource::TabJump as i32,
        language: prompt.language as i32,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn line(marker: char, text: &str) -> UnifiedDiffLine {
        let line_type = match marker {
            '+' => UnifiedDiffLineType::Insert,
            '-' => UnifiedDiffLineType::Delete,
            _ => UnifiedDiffLineType::Unchanged,
        };
        UnifiedDiffLine {
            text: text.to_string(),
            r#type: line_type as i32,
        }
    }

    fn sources(prompt: &TabJumpPrompt) -> Vec<(i32, bool, bool)> {
        prompt
            .messages
            .iter()
            .map(|message| {
                (
                    message.source,
                    message.safe_for_code_telemetry,
                    message.prompt_cache_options.is_some(),
                )
            })
            .collect()
    }

    #[test]
    fn test_hunks_use_three_lines_of_context_and_split() {
        let mut lines: Vec<UnifiedDiffLine> =
            (0..10).map(|row| line(' ', &format!("a{row}"))).collect();
        lines.push(line('-', "old"));
        lines.push(line('+', "new"));
        lines.push(line('+', "added"));
        lines.extend((0..8).map(|row| line(' ', &format!("b{row}"))));
        lines.push(line('-', "gone"));
        lines.extend((0..2).map(|row| line(' ', &format!("c{row}"))));
        let rendered = render_hunks(4, &lines).expect("there are changes");
        assert_eq!(rendered.first_changed_row, 14);
        assert_eq!(
            rendered.text,
            "@@ -12,7 +12,8 @@\n a7\n a8\n a9\n-old\n+new\n+added\n b0\n b1\n b2\n@@ -21,6 +22,5 @@\n b5\n b6\n b7\n-gone\n c0\n c1"
        );
        assert!(render_hunks(0, &[line(' ', "same")]).is_none());
    }

    #[test]
    fn test_hunks_merge_when_changes_are_close() {
        let lines = vec![
            line(' ', "x"),
            line('-', "a"),
            line('+', "b"),
            line(' ', "1"),
            line(' ', "2"),
            line(' ', "3"),
            line(' ', "4"),
            line(' ', "5"),
            line(' ', "6"),
            line('+', "c"),
        ];
        let rendered = render_hunks(0, &lines).expect("changes");
        assert_eq!(
            rendered.text,
            "@@ -1,8 +1,9 @@\n x\n-a\n+b\n 1\n 2\n 3\n 4\n 5\n 6\n+c"
        );
    }

    #[test]
    fn test_line_window_keeps_small_texts_and_focuses_large_ones() {
        let small = line_window("a\nb\n", 0, 100);
        assert_eq!(
            (small.start_row, small.end_row, small.text.as_str()),
            (0, 2, "a\nb")
        );

        let text: String = (0..100).map(|row| format!("line {row:03}\n")).collect();
        let window = line_window(&text, 50, 20);
        assert!(window.start_row <= 50 && 50 < window.end_row);
        assert!(window.end_row - window.start_row < 10);
        assert!(estimate_tokens(&window.text) <= 20);
        assert_eq!(
            &text[window.start_offset..][..window.text.len()],
            window.text
        );
    }

    fn sample_input() -> PromptInput {
        let active_text = "fn area(w: u32, h: u32) -> u32 {\n    w * h\n}\n\nfn main() {\n    let 面积 = area(2, 3); // 🙂\n}\n";
        let cursor_offset = active_text.find("area(2").expect("call");
        PromptInput {
            active: ActiveDocument {
                path: "/work/src/main.rs".to_string(),
                language: Language::Rust,
                editor_language: "rust".to_string(),
                text: active_text.to_string(),
                cursor_offset,
                code_item: Some("main".to_string()),
            },
            other_documents: vec![
                OpenDocument {
                    path: "/work/src/lib.rs".to_string(),
                    language: Language::Rust,
                    text: Some("pub fn helper() {}\n".to_string()),
                },
                OpenDocument {
                    path: "/work/README.md".to_string(),
                    language: Language::Markdown,
                    text: None,
                },
            ],
            history: vec![
                HistoryEvent::Edit {
                    path: "/work/src/lib.rs".to_string(),
                    diff: "@@ -1,1 +1,1 @@\n-fn helper() {}\n+pub fn helper() {}".to_string(),
                    first_changed_row: 0,
                    original_content: Some("fn helper() {}\n".to_string()),
                },
                HistoryEvent::Clipboard("area".to_string()),
                HistoryEvent::Edit {
                    path: "/work/src/main.rs".to_string(),
                    diff: "@@ -5,2 +5,2 @@\n fn main() {\n-    let x = 1;\n+    let 面积 = area(2, 3); // 🙂".to_string(),
                    first_changed_row: 5,
                    original_content: None,
                },
            ],
            presented_at: "Nov 16, 2026 at 7:44am, UTC-05:00".to_string(),
        }
    }

    #[test]
    fn test_prompt_layout() {
        let prompt = build_prompt(&sample_input());
        let user = ChatMessageSource::User as i32;
        let system = ChatMessageSource::System as i32;
        assert_eq!(
            sources(&prompt),
            vec![
                (user, true, false),
                (user, false, false),
                (user, true, false),
                (user, false, true),
                (user, false, false),
                (user, true, false),
                (user, false, true),
                (user, true, false),
                (system, false, false),
            ]
        );
        let texts: Vec<&str> = prompt
            .messages
            .iter()
            .map(|message| message.prompt.as_str())
            .collect();
        assert_eq!(
            texts[0],
            "The USER performed the following action in the IDE:\nShow the contents of file /work/src/lib.rs from lines 1 to 1"
        );
        assert_eq!(texts[1], "fn helper() {}");
        assert_eq!(
            texts[2],
            "The USER performed the following action in the IDE:\nShow the contents of file /work/src/main.rs from lines 1 to 7"
        );
        assert_eq!(texts[3], sample_input().active.text.trim_end());
        assert_eq!(
            texts[4],
            "The following changes were made by the USER to: /work/src/lib.rs.\n[diff_block_start]\n@@ -1,1 +1,1 @@\n-fn helper() {}\n+pub fn helper() {}\n[diff_block_end]\n\nPlease note that the above snippet only shows the MODIFIED lines from the last change. It shows up to 3 lines of unchanged lines before and after the modified lines. The actual file contents may have many more lines not shown."
        );
        assert_eq!(
            texts[5],
            "The USER performed the following action in the IDE:\nI copied the following text to the clipboard: area"
        );
        assert_eq!(
            texts[7],
            "<additional_metadata>\nNOTE: Open files and cursor position may not be related to the user's current request. Always verify relevance before assuming connection.\n\nThe USER presented this request to you on Nov 16, 2026 at 7:44am, UTC-05:00.\n\nThe current state of the user's IDE is as follows:\nActive Document: /work/src/main.rs (LANGUAGE_RUST)\nCursor is on line: 6\nOther open documents:\n- /work/src/lib.rs (LANGUAGE_RUST)\n- /work/README.md (LANGUAGE_MARKDOWN)\nActive Code Item: main\nOnly use this information if it is directly relevant to the user's request.\n\nThe user has mentioned some items in the form @[ITEM]. Here is extra information about the items that were mentioned by the user, in the order that they appear:\n\n@[/work/src/main.rs:L1-L7] is a [Text Block]:\nfn area(w: u32, h: u32) -> u32 {\n    w * h\n}\n\nfn main() {\n    let 面积 = <|cursor|>area(2, 3); // 🙂\n}\n</additional_metadata>\n<user_request>\nPlease modify the following mentioned code block with the logical next edit. The <|cursor|> in the code block represents where my cursor is.\n</user_request>"
        );
        assert_eq!(
            texts[8],
            "<replace_file_content>{\n\t\"TargetFile\": \"/work/src/main.rs\",\n\t\"CodeMarkdownLanguage\": \"rust\",\n\t\"Instruction\": \"Please modify the following mentioned code block with the logical next edit. The <|cursor|> in the code block represents where my cursor is.\",\n\t\"Blocking\": true,\n\t\"ReplacementChunks\": [{\n\t\t\"AllowMultiple\": false,\n\t\t\"TargetContent\":"
        );
        let annotation = &prompt.messages[8].prompt_annotation_ranges[0];
        assert_eq!(annotation.byte_offset_start, texts[8].len() as u64);
        assert!(annotation.suffix.starts_with(" \"fn area("));
        assert!(
            annotation.suffix.ends_with(
                "\",\n\t\t\"ReplacementContent\": \"\"\n\t}]\n}</replace_file_content>\n"
            )
        );
        assert_eq!(
            prompt.other_file_paths,
            vec!["/work/src/lib.rs".to_string()]
        );
        assert_eq!(prompt.target_path, "/work/src/main.rs");
        assert!(prompt.messages[0].num_tokens > 0);

        let request = build_request(prompt);
        assert_eq!(request.provider_source, ProviderSource::TabJump as i32);
        assert_eq!(request.request_type, ChatMessageRequestType::Command as i32);
        let configuration = request.configuration.expect("configuration");
        assert_eq!(
            (
                configuration.max_tokens,
                configuration.top_k,
                configuration.last_message_is_partial
            ),
            (2048, 50, true)
        );
        assert_eq!(configuration.stop_patterns.len(), 5);
        assert_eq!(request.prompt, SYSTEM_PROMPT);
        assert!(request.metadata.is_none());
    }

    #[test]
    fn test_prompt_without_history_or_other_files() {
        let mut input = sample_input();
        input.history.clear();
        input.other_documents.clear();
        input.active.code_item = None;
        let prompt = build_prompt(&input);
        assert_eq!(prompt.messages.len(), 4);
        let metadata = &prompt.messages[2].prompt;
        assert!(!metadata.contains("Other open documents"));
        assert!(!metadata.contains("Active Code Item"));
        assert!(prompt.messages[1].prompt_cache_options.is_some());
    }

    #[test]
    fn test_history_is_truncated_to_the_most_recent_events() {
        let mut input = sample_input();
        let long_line = "x".repeat(400);
        input.history = (0..40)
            .map(|index| HistoryEvent::Edit {
                path: "/work/src/main.rs".to_string(),
                diff: format!("@@ -1,1 +1,1 @@\n-{index}\n+{long_line}{index}"),
                first_changed_row: 0,
                original_content: None,
            })
            .collect();
        let prompt = build_prompt(&input);
        let edits: Vec<&ChatMessagePrompt> = prompt
            .messages
            .iter()
            .filter(|message| message.prompt.contains("[diff_block_start]"))
            .collect();
        assert!(!edits.is_empty() && edits.len() < 40);
        let total: usize = edits
            .iter()
            .map(|message| estimate_tokens(&message.prompt))
            .sum();
        assert!(total <= HISTORY_TOKENS);
        assert!(
            edits
                .last()
                .expect("an edit")
                .prompt
                .contains(&format!("{long_line}39"))
        );
    }

    #[test]
    fn test_large_active_document_is_windowed_around_the_cursor() {
        let mut input = sample_input();
        let text: String = (0..5000)
            .map(|row| format!("let value_{row} = {row};\n"))
            .collect();
        input.active.cursor_offset = text.find("let value_4000").expect("row");
        input.active.text = text;
        input.history.clear();
        let prompt = build_prompt(&input);
        let metadata = &prompt.messages[prompt.messages.len() - 2].prompt;
        assert!(metadata.contains("Cursor is on line: 4001"));
        assert!(metadata.contains("<|cursor|>let value_4000 = 4000;"));
        assert!(!metadata.contains("let value_0 = 0;"));
        assert!(estimate_tokens(metadata) <= ACTIVE_DOCUMENT_TOKENS + 400);
    }

    #[test]
    fn test_cursor_after_trailing_newline() {
        let mut input = sample_input();
        input.active.text = "a\nb\n".to_string();
        input.active.cursor_offset = 4;
        input.history.clear();
        let prompt = build_prompt(&input);
        let metadata = &prompt.messages[prompt.messages.len() - 2].prompt;
        assert!(metadata.contains("Cursor is on line: 3"));
        assert!(
            metadata
                .contains(":L1-L3] is a [Text Block]:\na\nb\n<|cursor|>\n</additional_metadata>")
        );
    }
}
