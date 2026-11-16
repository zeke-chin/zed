//! Heuristic for whether a plain string value is best shown as Markdown. One
//! strong marker (fence, image, table) or enough weak markers reach the
//! threshold. Each matcher mirrors one regular expression of the TypeScript
//! implementation, including its multiline and whitespace semantics.

use super::json_value::is_script_whitespace;

const MARKDOWN_MIN_LENGTH: usize = 20;
const MARKDOWN_MIN_SCORE: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlainStringFormat {
    Markdown,
    Text,
}

pub fn detect_string_format(value: &str) -> PlainStringFormat {
    if value.encode_utf16().count() < MARKDOWN_MIN_LENGTH {
        return PlainStringFormat::Text;
    }

    let chars: Vec<char> = value.chars().collect();
    let strong_matches = [
        has_fenced_code(&chars),
        has_image(&chars),
        has_table(&chars),
    ];
    let weak_matches = [
        has_heading(&chars),
        has_link(&chars),
        has_blockquote(&chars),
        has_horizontal_rule(&chars),
        has_bold(&chars),
        has_underscore_bold(&chars),
        has_list(&chars, ListKind::Bullet),
        has_list(&chars, ListKind::Ordered),
    ];

    let score = strong_matches.iter().filter(|matched| **matched).count() * 2
        + weak_matches.iter().filter(|matched| **matched).count();

    if score >= MARKDOWN_MIN_SCORE {
        PlainStringFormat::Markdown
    } else {
        PlainStringFormat::Text
    }
}

fn is_terminator(character: char) -> bool {
    matches!(character, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn is_word(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

fn line_starts(chars: &[char]) -> impl Iterator<Item = usize> + '_ {
    std::iter::once(0).chain(
        chars
            .iter()
            .enumerate()
            .filter(|(_, character)| is_terminator(**character))
            .map(|(index, _)| index + 1),
    )
}

fn whitespace_end(chars: &[char], start: usize) -> usize {
    let mut index = start;
    while chars.get(index).is_some_and(|c| is_script_whitespace(*c)) {
        index += 1;
    }
    index
}

fn run_end(chars: &[char], start: usize, accepts: impl Fn(char) -> bool) -> usize {
    let mut index = start;
    while chars.get(index).is_some_and(|c| accepts(*c)) {
        index += 1;
    }
    index
}

fn starts_with_at(chars: &[char], index: usize, pattern: &str) -> bool {
    pattern
        .chars()
        .enumerate()
        .all(|(offset, expected)| chars.get(index + offset) == Some(&expected))
}

fn has_fenced_code(chars: &[char]) -> bool {
    let Some(first) = (0..chars.len()).find(|index| starts_with_at(chars, *index, "```")) else {
        return false;
    };
    (first + 3..chars.len()).any(|index| starts_with_at(chars, index, "```"))
}

fn has_image(chars: &[char]) -> bool {
    (0..chars.len()).any(|start| {
        if !starts_with_at(chars, start, "![") {
            return false;
        }
        let close = run_end(chars, start + 2, |c| c != ']');
        link_target_follows(chars, close)
    })
}

fn link_target_follows(chars: &[char], close_bracket: usize) -> bool {
    if chars.get(close_bracket) != Some(&']') || chars.get(close_bracket + 1) != Some(&'(') {
        return false;
    }
    let target_start = close_bracket + 2;
    let target_end = run_end(chars, target_start, |c| {
        c != ')' && !is_script_whitespace(c)
    });
    target_end > target_start && chars.get(target_end) == Some(&')')
}

fn has_table(chars: &[char]) -> bool {
    line_starts(chars).any(|start| {
        if chars.get(start) != Some(&'|') {
            return false;
        }
        let line_end = run_end(chars, start + 1, |c| !is_terminator(c));
        (start + 2..line_end)
            .filter(|index| chars.get(*index) == Some(&'|'))
            .any(|pipe| separator_row_follows(chars, pipe))
    })
}

fn separator_row_follows(chars: &[char], pipe: usize) -> bool {
    let gap_end = whitespace_end(chars, pipe + 1);
    if gap_end == pipe + 1
        || chars.get(gap_end - 1) != Some(&'\n')
        || chars.get(gap_end) != Some(&'|')
    {
        return false;
    }
    let row_end = run_end(chars, gap_end + 1, |c| {
        is_script_whitespace(c) || matches!(c, ':' | '|' | '-')
    });
    (gap_end + 2..row_end).any(|index| chars.get(index) == Some(&'|'))
}

fn has_heading(chars: &[char]) -> bool {
    line_starts(chars).any(|start| {
        let hashes = run_end(chars, start, |c| c == '#') - start;
        (1..=6).contains(&hashes) && whitespace_then_text(chars, start + hashes)
    })
}

fn has_blockquote(chars: &[char]) -> bool {
    line_starts(chars)
        .any(|start| chars.get(start) == Some(&'>') && whitespace_then_text(chars, start + 1))
}

fn whitespace_then_text(chars: &[char], start: usize) -> bool {
    let end = whitespace_end(chars, start);
    end > start && end < chars.len()
}

fn has_link(chars: &[char]) -> bool {
    (0..chars.len()).any(|start| {
        if chars.get(start) != Some(&'[') {
            return false;
        }
        let label_end = run_end(chars, start + 1, |c| c != ']' && c != '\n');
        label_end > start + 1 && link_target_follows(chars, label_end)
    })
}

fn has_horizontal_rule(chars: &[char]) -> bool {
    line_starts(chars).any(|start| {
        let end = run_end(chars, start, |c| c == '-');
        end - start >= 3 && chars.get(end).is_none_or(|c| is_terminator(*c))
    })
}

fn has_bold(chars: &[char]) -> bool {
    (0..chars.len()).any(|start| {
        if !starts_with_at(chars, start, "**") {
            return false;
        }
        let content_end = run_end(chars, start + 2, |c| c != '*' && c != '\n');
        content_end > start + 2 && starts_with_at(chars, content_end, "**")
    })
}

fn has_underscore_bold(chars: &[char]) -> bool {
    (0..chars.len()).any(|start| {
        if !starts_with_at(chars, start, "__") {
            return false;
        }
        if start > 0 && chars.get(start - 1).is_some_and(|c| is_word(*c)) {
            return false;
        }
        let content_end = run_end(chars, start + 2, |c| c != '_' && c != '\n');
        content_end > start + 2
            && starts_with_at(chars, content_end, "__")
            && !chars.get(content_end + 2).is_some_and(|c| is_word(*c))
    })
}

#[derive(Clone, Copy)]
enum ListKind {
    Bullet,
    Ordered,
}

fn list_item_end(chars: &[char], start: usize, kind: ListKind) -> Option<usize> {
    let marker_end = match kind {
        ListKind::Bullet => {
            if matches!(chars.get(start), Some('-' | '*' | '+')) {
                start + 1
            } else {
                return None;
            }
        }
        ListKind::Ordered => {
            let digits_end = run_end(chars, start, |c| c.is_ascii_digit());
            if digits_end == start || chars.get(digits_end) != Some(&'.') {
                return None;
            }
            digits_end + 1
        }
    };
    let text_start = whitespace_end(chars, marker_end);
    if text_start == marker_end || text_start >= chars.len() {
        return None;
    }
    Some(run_end(chars, text_start, |c| !is_terminator(c)))
}

fn has_list(chars: &[char], kind: ListKind) -> bool {
    line_starts(chars).any(|start| {
        let Some(line_end) = list_item_end(chars, start, kind) else {
            return false;
        };
        let next_line = match (chars.get(line_end), chars.get(line_end + 1)) {
            (Some('\n'), _) => line_end + 1,
            (Some('\r'), Some('\n')) => line_end + 2,
            _ => return false,
        };
        list_item_end(chars, next_line, kind).is_some()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format_of(value: &str) -> PlainStringFormat {
        detect_string_format(value)
    }

    #[test]
    fn short_string_is_text() {
        assert_eq!(format_of("hello world"), PlainStringFormat::Text);
    }

    #[test]
    fn plain_prose_is_text() {
        assert_eq!(
            format_of("This is a longer paragraph of plain prose with multiple words."),
            PlainStringFormat::Text
        );
    }

    #[test]
    fn code_like_content_is_text() {
        assert_eq!(
            format_of("function foo() {\n  return 1 + 2;\n}\nfoo();"),
            PlainStringFormat::Text
        );
    }

    #[test]
    fn markdown_signals() {
        for markdown in [
            "## API Reference\n\n- foo\n- bar\n- baz",
            "Example:\n\n```ts\nconst x = 1;\n```",
            "# Title\n\nSee [docs](https://example.com).",
            "![alt text](https://example.com/img.png) caption",
            "| a | b |\n|---|---|\n| 1 | 2 |",
            "| a | b |\r\n|:--|--:|\r\n| 1 | 2 |",
            "some **bold** text and __more bold__ here",
            "1. first item here\n2. second item here\n> a quote line",
        ] {
            assert_eq!(
                format_of(markdown),
                PlainStringFormat::Markdown,
                "{markdown:?}"
            );
        }
    }

    #[test]
    fn single_weak_signal_is_text() {
        assert_eq!(
            format_of("# Configuration\nport=8080\nhost=localhost"),
            PlainStringFormat::Text
        );
        assert_eq!(
            format_of("- just one bullet line here for sure"),
            PlainStringFormat::Text
        );
        assert_eq!(
            format_of("snake__case__name is not bold at all"),
            PlainStringFormat::Text
        );
    }

    #[test]
    fn length_threshold_counts_utf16_units() {
        assert_eq!(format_of("```a```"), PlainStringFormat::Text);
        assert_eq!(
            format_of("我我我我我我我我我我我我我我我我我我我我"),
            PlainStringFormat::Text
        );
    }
}
