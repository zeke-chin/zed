#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedText {
    pub text: String,
    /// Zero-based document line containing the first selected character.
    pub start_line: usize,
}

impl SelectedText {
    pub fn new(text: impl Into<String>, start_line: usize) -> Self {
        Self {
            text: text.into(),
            start_line,
        }
    }
}

fn is_blank(line: &str) -> bool {
    line.chars()
        .all(|character| character.is_whitespace() || character == '\u{feff}')
}

fn split_lines(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut line_start = 0;
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'\n' => {
                lines.push(text.get(line_start..index).unwrap_or_default());
                index += 1;
                line_start = index;
            }
            b'\r' => {
                lines.push(text.get(line_start..index).unwrap_or_default());
                index += if bytes.get(index + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
                line_start = index;
            }
            _ => index += 1,
        }
    }
    lines.push(text.get(line_start..).unwrap_or_default());
    lines
}

fn longest_run(text: &str, marker: char) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for character in text.chars() {
        if character == marker {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

/// Keeps indentation and internal blank lines; removes only empty boundary lines.
pub fn format_selection(path: &str, selection: &SelectedText) -> String {
    let lines = split_lines(&selection.text);
    let line_count = lines.len();
    let mut first = 0;
    while first < line_count && lines.get(first).is_some_and(|line| is_blank(line)) {
        first += 1;
    }
    let mut end = line_count;
    while end > first && lines.get(end - 1).is_some_and(|line| is_blank(line)) {
        end -= 1;
    }
    if first >= end {
        // A whitespace-only selection still needs meaningful source coordinates.
        first = 0;
        end = line_count;
        if end > 1 && lines.get(end - 1).is_some_and(|line| line.is_empty()) {
            end -= 1;
        }
    }
    let content = lines.get(first..end).unwrap_or_default().join("\n");
    let start = selection.start_line + first + 1;
    let last = selection.start_line + end;
    let location = if start == last {
        format!("{path}:{start}")
    } else {
        format!("{path}:{start}-{last}")
    };
    // Tilde fences also allow backticks in filenames and Markdown snippets.
    let marker = if path.contains('`') { '~' } else { '`' };
    let fence_length = (longest_run(path, marker).max(longest_run(&content, marker)) + 1).max(3);
    let fence = marker.to_string().repeat(fence_length);
    format!("{fence}{location}\n{content}\n{fence}")
}

pub fn format_context(path: &str, selections: &[SelectedText]) -> String {
    selections
        .iter()
        .map(|selection| format_selection(path, selection))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn short_path(path: &str) -> String {
    let file_name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    format!("../{file_name}")
}

/// Matches a fenced block starting at the beginning of `rest`. Returns the
/// number of bytes consumed, the location line and the body.
fn match_fenced_block(rest: &str) -> Option<(usize, &str, &str)> {
    let marker = rest
        .chars()
        .next()
        .filter(|character| *character == '`' || *character == '~')?;
    let fence_length = rest
        .chars()
        .take_while(|character| *character == marker)
        .count();
    if fence_length < 3 {
        return None;
    }
    let (fence, after_fence) = rest.split_at_checked(fence_length)?;
    let (location, body_region) = after_fence.split_once('\n')?;
    if location.is_empty() {
        return None;
    }
    let closing = format!("\n{fence}");
    let mut offset = 0;
    loop {
        let found = body_region.get(offset..)?.find(&closing)? + offset;
        let tail = body_region.get(found + closing.len()..)?;
        if tail.is_empty() || tail.starts_with('\n') {
            let body = body_region.get(..found)?;
            return Some((rest.len() - tail.len(), location, body));
        }
        offset = found + 1;
    }
}

fn strip_fences(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut index = 0;
    while let Some(rest) = text.get(index..) {
        let Some(character) = rest.chars().next() else {
            break;
        };
        let at_line_start = index == 0 || text.as_bytes().get(index - 1) == Some(&b'\n');
        if at_line_start && let Some((consumed, location, body)) = match_fenced_block(rest) {
            output.push_str(&short_path(location));
            output.push('\n');
            output.push_str(body);
            index += consumed;
        } else {
            output.push(character);
            index += character.len_utf8();
        }
    }
    output
}

/// Splits a trailing `:line` or `:start-end` suffix off the first colon that makes the remainder valid.
fn split_line_suffix(text: &str) -> (&str, &str) {
    for (index, character) in text.char_indices() {
        if character != ':' {
            continue;
        }
        let Some(suffix) = text.get(index..) else {
            continue;
        };
        if is_line_suffix(suffix) {
            return (text.get(..index).unwrap_or_default(), suffix);
        }
    }
    (text, "")
}

fn is_line_suffix(suffix: &str) -> bool {
    let Some(numbers) = suffix.strip_prefix(':') else {
        return false;
    };
    let is_number = |part: &str| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit());
    match numbers.split_once('-') {
        Some((start, end)) => is_number(start) && is_number(end),
        None => is_number(numbers),
    }
}

fn file_extension(filename: &str) -> &str {
    match filename.rfind('.') {
        Some(index) if index + 1 < filename.len() => filename.get(index..).unwrap_or_default(),
        _ => "",
    }
}

fn last_characters(text: &str, count: usize) -> String {
    let total = text.chars().count();
    text.chars().skip(total.saturating_sub(count)).collect()
}

/// Formats only the preview; never alters the clipboard itself.
pub fn format_copy_preview(text: &str, is_context: bool, max_length: usize) -> String {
    let mut preview = text.to_string();
    if is_context {
        preview = strip_fences(text);
        if preview == text {
            preview = short_path(text);
        }
        let location = preview.split('\n').next().unwrap_or_default();
        let location_length = location.chars().count();
        if location_length >= max_length {
            if location_length == max_length {
                return location.to_string();
            }
            // Spend the limited space on the filename's end, keeping the extension.
            let (filename, mut lines) = split_line_suffix(location.get(3..).unwrap_or_default());
            let extension = file_extension(filename);
            let prefix = "../..";
            let prefix_length = prefix.len();
            if prefix_length + extension.chars().count() + lines.chars().count() > max_length {
                lines = "";
            }
            let tail = format!("{filename}{lines}");
            if max_length <= prefix_length {
                return last_characters(&tail, max_length);
            }
            return format!(
                "{prefix}{}",
                last_characters(&tail, max_length - prefix_length)
            );
        }
    }
    let preview = split_lines(&preview).join("\\n").replace('\t', "\\t");
    if preview.chars().count() > max_length {
        let kept: String = preview.chars().take(max_length.saturating_sub(1)).collect();
        format!("{kept}…")
    } else {
        preview
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(text: &str, start_line: usize) -> SelectedText {
        SelectedText::new(text, start_line)
    }

    #[test]
    fn blank_boundary_lines_are_removed_and_source_line_numbers_track_retained_text() {
        assert_eq!(
            format_selection(
                "/repo/README.md",
                &selection(
                    "\n## 功能演示\n\n### 1. 智能识别与格式化\n![Auto Paste Format](images/auto-paste-format.gif)\n\n",
                    6
                )
            ),
            "```/repo/README.md:8-11\n## 功能演示\n\n### 1. 智能识别与格式化\n![Auto Paste Format](images/auto-paste-format.gif)\n```"
        );
    }

    #[test]
    fn single_line_selections_preserve_indentation_and_trailing_spaces() {
        assert_eq!(
            format_selection("/a.ts", &selection("  value  ", 4)),
            "```/a.ts:5\n  value  \n```"
        );
    }

    #[test]
    fn selection_ending_at_the_start_of_the_next_line_excludes_that_line() {
        assert_eq!(
            format_selection("/a", &selection("a\nb\n", 0)),
            "```/a:1-2\na\nb\n```"
        );
    }

    #[test]
    fn crlf_source_and_blank_lines_preserve_correct_coordinates() {
        assert_eq!(
            format_selection("C:\\项目\\a.ts", &selection("\r\n  a\r\n\r\n  b\r\n", 1)),
            "```C:\\项目\\a.ts:3-5\n  a\n\n  b\n```"
        );
    }

    #[test]
    fn markdown_snippets_cannot_prematurely_close_the_outer_fence() {
        assert_eq!(
            format_selection("/a.md", &selection("```ts\na\n```", 0)),
            "````/a.md:1-3\n```ts\na\n```\n````"
        );
    }

    #[test]
    fn filenames_containing_backticks_use_tilde_fences() {
        assert_eq!(
            format_selection("/a`b", &selection("~~~", 0)),
            "~~~~/a`b:1\n~~~\n~~~~"
        );
    }

    #[test]
    fn whitespace_only_selection_remains_copyable() {
        assert_eq!(
            format_selection("/a", &selection("  \n", 3)),
            "```/a:4\n  \n```"
        );
    }

    #[test]
    fn multiple_selections_get_separate_source_headers() {
        assert_eq!(
            format_context("/a", &[selection("a", 0), selection("b", 8)]),
            "```/a:1\na\n```\n\n```/a:9\nb\n```"
        );
    }

    #[test]
    fn lone_cr_line_endings_are_normalized_and_empty_selection_is_stable() {
        assert_eq!(
            format_selection("/a", &selection("a\rb", 0)),
            "```/a:1-2\na\nb\n```"
        );
        assert_eq!(format_selection("/a", &selection("", 0)), "```/a:1\n\n```");
        assert_eq!(format_context("/a", &[]), "");
    }

    #[test]
    fn context_preview_keeps_filename_lines_and_content_without_the_outer_fence() {
        let content = format_selection(
            "/home/zeke/Downloads/# Copy Anchor 📌.txt",
            &selection("## 使用\n\n在已保存文件中", 4),
        );
        assert_eq!(
            format_copy_preview(&content, true, 100),
            "../# Copy Anchor 📌.txt:5-7\\n## 使用\\n\\n在已保存文件中"
        );
    }

    #[test]
    fn plain_preview_preserves_markdown_and_escapes_line_endings() {
        assert_eq!(
            format_copy_preview("## 使用\r\n\r\n在已保存文件中", false, 100),
            "## 使用\\n\\n在已保存文件中"
        );
        assert_eq!(
            format_copy_preview("```/a:1\ntext\n```", false, 100),
            "```/a:1\\ntext\\n```"
        );
    }

    #[test]
    fn preview_handles_path_only_copies_and_code_containing_fences() {
        assert_eq!(
            format_copy_preview("C:\\repo\\file.txt", true, 100),
            "../file.txt"
        );
        let content = format_selection("/a`b.md", &selection("~~~\ncode\n~~~", 0));
        assert_eq!(
            format_copy_preview(&content, true, 100),
            "../a`b.md:1-3\\n~~~\\ncode\\n~~~"
        );
    }

    #[test]
    fn preview_limit_includes_ellipsis_and_does_not_split_multibyte_characters() {
        assert_eq!(format_copy_preview("📌中文abcdef", false, 4), "📌中文…");
        assert_eq!(format_copy_preview("long", false, 1), "…");
        assert_eq!(format_copy_preview("📌中文", false, 3), "📌中文");
    }

    #[test]
    fn short_context_previews_keep_the_filename_end_and_extension() {
        assert_eq!(
            format_copy_preview("/repo/README.txt", true, 11),
            "../..ME.txt"
        );
        let content = format_selection("/repo/README.txt", &selection("body", 4));
        assert_eq!(format_copy_preview(&content, true, 13), "../..ME.txt:5");
        assert_eq!(format_copy_preview(&content, true, 10), "../..E.txt");
        assert_eq!(format_copy_preview("/repo/README.txt", true, 4), ".txt");
        assert_eq!(
            format_copy_preview("/repo/📌中文.txt", true, 12),
            "../📌中文.txt"
        );
    }

    #[test]
    fn preview_escapes_tabs_and_handles_exact_length_location() {
        assert_eq!(format_copy_preview("a\tb", false, 100), "a\\tb");
        assert_eq!(format_copy_preview("/repo/a.txt", true, 9), "../a.txt");
        assert_eq!(format_copy_preview("/repo/a.txt", true, 8), "../a.txt");
    }
}
