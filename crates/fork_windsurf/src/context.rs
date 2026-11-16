//! Context for the current document: full text, cursor, the window around the
//! cursor that the server rewrites, and nearby diagnostics. Zed's buffer
//! offsets are already UTF-8 byte offsets and rows are 0-based, which are the
//! units `GetTab` expects (`fork/spike/结论.md` 问题 15).

use std::{
    ops::Range,
    path::{Path, PathBuf},
};

use gpui::App;
use language::{Anchor, Buffer, BufferSnapshot, Point, ToOffset as _, ToPointUtf16 as _};

use crate::{
    backend::DocumentContext,
    proto::exa::codeium_common_pb::{CodeDiagnostic, DocumentPosition, Language, Range as Span},
};

/// Lines on each side of the cursor row in the window (`SUPERCOMPLETE_LINE_RADIUS`).
const SELECTION_RADIUS: u32 = 5;
const DIAGNOSTIC_RADIUS: u32 = 10;
const MAX_DIAGNOSTICS: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectionWindow {
    pub text: String,
    /// 0-based, end exclusive.
    pub start_line: u32,
    pub end_line: u32,
    pub byte_range: Range<usize>,
}

/// Rows `[row - 5, row + 6)` clipped to the file, with blank lines dropped from
/// both ends of the window (but never the cursor row itself).
pub fn selection_window(text: &str, cursor_row: u32) -> SelectionWindow {
    let lines: Vec<&str> = text.split('\n').collect();
    let line_count = lines.len() as u32;
    let row = cursor_row.min(line_count.saturating_sub(1));
    let mut start_line = row.saturating_sub(SELECTION_RADIUS);
    let mut end_line = (row + SELECTION_RADIUS + 1).min(line_count);
    while start_line < row && lines[start_line as usize].trim().is_empty() {
        start_line += 1;
    }
    while end_line - 1 > row && lines[end_line as usize - 1].trim().is_empty() {
        end_line -= 1;
    }
    let start_byte: usize = lines[..start_line as usize]
        .iter()
        .map(|line| line.len() + 1)
        .sum();
    let window_text = lines[start_line as usize..end_line as usize].join("\n");
    SelectionWindow {
        byte_range: start_byte..start_byte + window_text.len(),
        text: window_text,
        start_line,
        end_line,
    }
}

/// Where a buffer lives, as the server sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferLocation {
    pub abs_path: PathBuf,
    pub workspace_path: PathBuf,
}

impl BufferLocation {
    pub fn uri(&self) -> String {
        file_uri(&self.abs_path)
    }

    pub fn workspace_uri(&self) -> String {
        file_uri(&self.workspace_path)
    }
}

pub fn file_uri(path: &Path) -> String {
    url::Url::from_file_path(path)
        .map(String::from)
        .unwrap_or_else(|()| format!("file://{}", path.to_string_lossy()))
}

/// The buffer's absolute path and its worktree root. Works for remote
/// projects too, where both are paths on the remote machine.
pub fn buffer_location(buffer: &Buffer, cx: &App) -> Option<BufferLocation> {
    let file = project::File::from_dyn(buffer.file())?;
    let worktree = file.worktree.read(cx);
    let root = worktree.abs_path();
    if file.path.is_empty() {
        let workspace_path = root.parent().map(Path::to_path_buf)?;
        return Some(BufferLocation {
            abs_path: root.to_path_buf(),
            workspace_path,
        });
    }
    Some(BufferLocation {
        abs_path: root.join(file.path.as_std_path()),
        workspace_path: root.to_path_buf(),
    })
}

pub struct DocumentSnapshot {
    pub document: DocumentContext,
    pub window: SelectionWindow,
}

pub fn document_context(
    snapshot: &BufferSnapshot,
    cursor: Anchor,
    location: &BufferLocation,
    is_dirty: bool,
) -> DocumentSnapshot {
    document_context_for_text(
        snapshot.text(),
        cursor.to_offset(snapshot),
        snapshot
            .language()
            .map(|language| language.name().0.to_string()),
        location,
        is_dirty,
        None,
    )
}

/// The document context for `text`, which needn't be the buffer's current
/// text: predictive requests describe the buffer as if a prediction had been
/// accepted.
pub fn document_context_for_text(
    text: String,
    cursor_offset: usize,
    zed_language: Option<String>,
    location: &BufferLocation,
    is_dirty: bool,
    cursor_position: Option<(u32, u32)>,
) -> DocumentSnapshot {
    let cursor_row = text
        .get(..cursor_offset)
        .map(|before| before.matches('\n').count() as u32)
        .unwrap_or_default();
    let window = selection_window(&text, cursor_row);
    let (language, editor_language) = language_for(zed_language);
    DocumentSnapshot {
        document: DocumentContext {
            text,
            absolute_uri: location.uri(),
            workspace_uri: location.workspace_uri(),
            language,
            editor_language,
            cursor_offset,
            selection: window.text.clone(),
            selection_start_line: window.start_line,
            selection_end_line: window.end_line,
            is_dirty,
            cursor_position,
        },
        window,
    }
}

/// Diagnostics within ten rows of the cursor, at most five, of any severity
/// (`SUPERCOMPLETE_CODE_DIAGNOSTICS_TOP_K=5`, threshold `all_diagnostics`;
/// `fork/spike/结论.md` 问题 7). Errors come first, then the closest to the
/// cursor. The fields are the ones the official client fills: a range with
/// UTF-16 positions only, message, severity, source and uri.
pub fn diagnostics_near(
    snapshot: &BufferSnapshot,
    cursor_row: u32,
    uri: &str,
) -> Vec<CodeDiagnostic> {
    let max_point = snapshot.max_point();
    let start = Point::new(cursor_row.saturating_sub(DIAGNOSTIC_RADIUS), 0);
    let end_row = (cursor_row + DIAGNOSTIC_RADIUS).min(max_point.row);
    let end = Point::new(end_row, snapshot.line_len(end_row));
    let mut entries: Vec<_> = snapshot
        .diagnostics_in_range::<_, usize>(start..end, false)
        .map(|entry| {
            let start_row = snapshot.offset_to_point(entry.range.start).row;
            (
                entry.diagnostic.severity,
                start_row.abs_diff(cursor_row),
                entry.range.clone(),
                entry.diagnostic.clone(),
            )
        })
        .collect();
    entries.sort_by_key(|(severity, distance, range, _)| (*severity, *distance, range.start));
    entries
        .into_iter()
        .take(MAX_DIAGNOSTICS)
        .map(|(severity, _, range, diagnostic)| {
            let position = |offset: usize| {
                let point = offset.to_point_utf16(snapshot);
                DocumentPosition {
                    row: u64::from(point.row),
                    col: u64::from(point.column),
                }
            };
            CodeDiagnostic {
                range: Some(Span {
                    start_position: Some(position(range.start)),
                    end_position: Some(position(range.end)),
                    ..Default::default()
                }),
                message: diagnostic.message.as_str().to_string(),
                severity: severity_name(severity).to_string(),
                source: diagnostic.source.clone().unwrap_or_default(),
                uri: uri.to_string(),
                ..Default::default()
            }
        })
        .collect()
}

fn severity_name(severity: lsp::DiagnosticSeverity) -> &'static str {
    match severity {
        lsp::DiagnosticSeverity::ERROR => "error",
        lsp::DiagnosticSeverity::WARNING => "warning",
        lsp::DiagnosticSeverity::INFORMATION => "information",
        _ => "hint",
    }
}

/// Maps a Zed language name to the `Language` enum and the VS Code language
/// id the official client puts in `editor_language`.
pub fn language_for(zed_language: Option<String>) -> (Language, String) {
    let Some(name) = zed_language else {
        return (Language::Plaintext, "plaintext".to_string());
    };
    let (language, editor_language) = match name.as_str() {
        "Rust" => (Language::Rust, "rust"),
        "Python" => (Language::Python, "python"),
        "TypeScript" => (Language::Typescript, "typescript"),
        "TSX" => (Language::Tsx, "typescriptreact"),
        "JavaScript" => (Language::Javascript, "javascript"),
        "JSON" => (Language::Json, "json"),
        "JSONC" => (Language::Json, "jsonc"),
        "YAML" => (Language::Yaml, "yaml"),
        "TOML" => (Language::Toml, "toml"),
        "Markdown" => (Language::Markdown, "markdown"),
        "Go" => (Language::Go, "go"),
        "C" => (Language::C, "c"),
        "C++" => (Language::Cpp, "cpp"),
        "C#" => (Language::Csharp, "csharp"),
        "CSS" => (Language::Css, "css"),
        "SCSS" => (Language::Scss, "scss"),
        "HTML" => (Language::Html, "html"),
        "Shell Script" => (Language::Shell, "shellscript"),
        "Ruby" => (Language::Ruby, "ruby"),
        "Java" => (Language::Java, "java"),
        "Kotlin" => (Language::Kotlin, "kotlin"),
        "Swift" => (Language::Swift, "swift"),
        "PHP" => (Language::Php, "php"),
        "Lua" => (Language::Lua, "lua"),
        "SQL" => (Language::Sql, "sql"),
        "Elixir" => (Language::Elixir, "elixir"),
        "Erlang" => (Language::Erlang, "erlang"),
        "Haskell" => (Language::Haskell, "haskell"),
        "Scala" => (Language::Scala, "scala"),
        "Dart" => (Language::Dart, "dart"),
        "Vue.js" => (Language::Vue, "vue"),
        "Svelte" => (Language::Svelte, "svelte"),
        "Astro" => (Language::Astro, "astro"),
        "Dockerfile" => (Language::Dockerfile, "dockerfile"),
        "Make" => (Language::Makefile, "makefile"),
        "CMake" => (Language::Cmake, "cmake"),
        "Proto" => (Language::Protobuf, "proto"),
        "OCaml" => (Language::Ocaml, "ocaml"),
        "XML" => (Language::Xml, "xml"),
        "LaTeX" => (Language::Latex, "latex"),
        "Perl" => (Language::Perl, "perl"),
        "R" => (Language::R, "r"),
        "Clojure" => (Language::Clojure, "clojure"),
        "Solidity" => (Language::Solidity, "solidity"),
        "GDScript" => (Language::Gdscript, "gdscript"),
        "Nim" => (Language::Nim, "nim"),
        "Plain Text" => (Language::Plaintext, "plaintext"),
        _ => return (Language::Plaintext, name.to_lowercase()),
    };
    (language, editor_language.to_string())
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext as _, TestAppContext};
    use language::{Buffer, DiagnosticEntry, DiagnosticSet, LanguageServerId, PointUtf16};
    use pretty_assertions::assert_eq;

    use super::*;

    fn window(text: &str, row: u32) -> (String, u32, u32) {
        let window = selection_window(text, row);
        assert_eq!(&text[window.byte_range.clone()], window.text);
        (window.text, window.start_line, window.end_line)
    }

    #[test]
    fn test_window_spans_five_rows_each_side() {
        let text: String = (0..20).map(|row| format!("line {row}\n")).collect();
        let (window_text, start, end) = window(&text, 10);
        assert_eq!((start, end), (5, 16));
        assert!(window_text.starts_with("line 5\n") && window_text.ends_with("line 15"));
    }

    #[test]
    fn test_window_is_clipped_to_the_file() {
        let text = "a\nb\nc";
        assert_eq!(window(text, 0), ("a\nb\nc".to_string(), 0, 3));
        assert_eq!(window(text, 2), ("a\nb\nc".to_string(), 0, 3));
        // A trailing newline yields an empty last row, which is kept when the
        // cursor is on it.
        assert_eq!(window("a\nb\n", 2), ("a\nb\n".to_string(), 0, 3));
    }

    #[test]
    fn test_window_drops_blank_rows_at_both_ends() {
        let text = "a\n\n  \nb\nc\nd\n\n\n";
        // Cursor on `c` (row 4): rows 0..8 minus trailing blank rows.
        assert_eq!(window(text, 4), ("a\n\n  \nb\nc\nd".to_string(), 0, 6));
        let text = "x\n\n\n\n\n\ny\n";
        // Cursor on `y` (row 6): window starts at row 1, which is blank.
        assert_eq!(window(text, 6), ("y".to_string(), 6, 7));
        // A blank cursor row itself is kept.
        assert_eq!(window("\n\n", 1), ("".to_string(), 1, 2));
    }

    #[test]
    fn test_window_with_multibyte_and_crlf_source() {
        let text = "// 你好 🌍\nlet 名字 = 1;\r\nlet b = 名字;";
        let (window_text, start, end) = window(text, 1);
        assert_eq!((start, end), (0, 3));
        assert_eq!(window_text, text);
    }

    #[gpui::test]
    fn test_cursor_offset_is_utf8_bytes(cx: &mut TestAppContext) {
        let source = "// 你好 🌍\r\nlet 名字 = \"世界\";\r\nprint(名字)\r\n";
        let buffer = cx.new(|cx| Buffer::local(source, cx));
        buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            // Zed keeps text with `\n` line endings internally.
            let text = snapshot.text();
            assert_eq!(text, source.replace("\r\n", "\n"));
            let cursor_byte = text.find("print(名").expect("print") + "print(名".len();
            let cursor = snapshot.anchor_after(cursor_byte);
            let location = BufferLocation {
                abs_path: PathBuf::from("/work/src/main.py"),
                workspace_path: PathBuf::from("/work"),
            };
            let context = document_context(&snapshot, cursor, &location, true);
            assert_eq!(context.document.cursor_offset, cursor_byte);
            assert_eq!(
                &context.document.text[..cursor_byte],
                "// 你好 🌍\nlet 名字 = \"世界\";\nprint(名"
            );
            assert_eq!(context.window.start_line, 0);
            // The empty row after the last newline is dropped from the window.
            assert_eq!(context.window.end_line, 3);
            assert_eq!(context.document.absolute_uri, "file:///work/src/main.py");
            assert_eq!(context.document.workspace_uri, "file:///work");
            assert_eq!(context.document.language, Language::Plaintext);
        });
    }

    #[test]
    fn test_file_uri_percent_encodes() {
        assert_eq!(
            file_uri(Path::new("/work/my dir/文件.rs")),
            "file:///work/my%20dir/%E6%96%87%E4%BB%B6.rs"
        );
    }

    #[test]
    fn test_language_mapping() {
        assert_eq!(
            language_for(Some("Rust".to_string())),
            (Language::Rust, "rust".to_string())
        );
        assert_eq!(
            language_for(Some("TSX".to_string())),
            (Language::Tsx, "typescriptreact".to_string())
        );
        assert_eq!(
            language_for(Some("Gleam".to_string())),
            (Language::Plaintext, "gleam".to_string())
        );
        assert_eq!(language_for(None).0, Language::Plaintext);
    }

    #[gpui::test]
    fn test_diagnostics_near_cursor(cx: &mut TestAppContext) {
        let source: String = (0..40).map(|row| format!("line {row}\n")).collect();
        let buffer = cx.new(|cx| Buffer::local(source, cx));
        buffer.update(cx, |buffer, cx| {
            let entries = [0u32, 18, 19, 20, 21, 22, 23, 24, 31]
                .into_iter()
                .map(|row| {
                    DiagnosticEntry::new(
                        PointUtf16::new(row, 0)..PointUtf16::new(row, 4),
                        language::Diagnostic {
                            severity: if row == 24 {
                                lsp::DiagnosticSeverity::ERROR
                            } else {
                                lsp::DiagnosticSeverity::WARNING
                            },
                            message: language::DiagnosticMessage::plain(format!("problem {row}")),
                            group_id: row as usize,
                            is_primary: true,
                            ..Default::default()
                        },
                    )
                })
                .collect::<Vec<_>>();
            let snapshot = buffer.snapshot();
            let set = DiagnosticSet::new(entries, &snapshot);
            buffer.update_diagnostics(LanguageServerId(0), set, cx);
        });
        buffer.read_with(cx, |buffer, _| {
            let diagnostics = diagnostics_near(&buffer.snapshot(), 20, "file:///a.txt");
            let messages: Vec<_> = diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect();
            assert_eq!(
                messages,
                vec![
                    "problem 24",
                    "problem 20",
                    "problem 19",
                    "problem 21",
                    "problem 18"
                ]
            );
            assert_eq!(diagnostics[0].severity, "error");
            let range = diagnostics[0].range.as_ref().expect("range");
            assert_eq!(
                range.start_position,
                Some(DocumentPosition { row: 24, col: 0 })
            );
            assert_eq!((range.start_offset, range.end_offset), (0, 0));
            assert_eq!(diagnostics[0].language, 0);
        });
    }
}
