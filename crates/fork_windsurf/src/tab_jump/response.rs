//! Reads the model's continuation of the half-written `<replace_file_content>`
//! and finds the jump target in a file. The continuation starts right after
//! `"TargetContent":`, e.g.
//! ` "    w = <|cursor|>add(v)\n",\n\t\t"ReplacementContent": "    w = v.add()\n"\n\t}]\n}`.
//! `TargetContent` is text from the file with `<|cursor|>` where the cursor
//! should go; `ReplacementContent` is what that text becomes.

use std::ops::Range;

use super::prompt::CURSOR_MARKER;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JumpCompletion {
    /// Still contains `<|cursor|>`.
    pub target_content: String,
    pub replacement_content: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The stream stopped inside the JSON.
    Incomplete,
    /// Not the expected `"…", "ReplacementContent": "…"` shape.
    Malformed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocateError {
    /// Nothing but the cursor marker.
    Empty,
    NotFound,
    /// The text occurs more than once, so the place is unclear.
    Ambiguous,
}

/// Returns `None` when the model left `TargetContent` empty, which is how it
/// says there is no next edit.
pub fn parse_completion(text: &str) -> Result<Option<JumpCompletion>, ParseError> {
    let (target_content, rest) = json_string(text)?;
    if target_content.is_empty() {
        return Ok(None);
    }
    let rest = expect_punctuation(rest, ',')?;
    let (key, rest) = json_string(rest)?;
    if key != "ReplacementContent" {
        return Err(ParseError::Malformed);
    }
    let rest = expect_punctuation(rest, ':')?;
    let (replacement_content, _) = json_string(rest)?;
    Ok(Some(JumpCompletion {
        target_content: target_content.replace("\r\n", "\n"),
        replacement_content: replacement_content.replace("\r\n", "\n"),
    }))
}

fn expect_punctuation(text: &str, punctuation: char) -> Result<&str, ParseError> {
    let text = text.trim_start();
    if text.is_empty() {
        return Err(ParseError::Incomplete);
    }
    text.strip_prefix(punctuation).ok_or(ParseError::Malformed)
}

/// Reads one JSON string literal after optional whitespace and returns it
/// with the rest of the text.
fn json_string(text: &str) -> Result<(String, &str), ParseError> {
    let text = text.trim_start();
    if text.is_empty() {
        return Err(ParseError::Incomplete);
    }
    if !text.starts_with('"') {
        return Err(ParseError::Malformed);
    }
    let mut values = serde_json::Deserializer::from_str(text).into_iter::<String>();
    match values.next() {
        Some(Ok(value)) => {
            let consumed = values.byte_offset();
            Ok((value, &text[consumed..]))
        }
        Some(Err(error)) if error.is_eof() => Err(ParseError::Incomplete),
        Some(Err(_)) | None => Err(ParseError::Malformed),
    }
}

/// Removes every `<|cursor|>` and returns where the first one was.
pub fn split_cursor_marker(target_content: &str) -> (String, Option<usize>) {
    let position = target_content.find(CURSOR_MARKER);
    (target_content.replace(CURSOR_MARKER, ""), position)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetMatch {
    /// Byte range of the target text in the searched text.
    pub range: Range<usize>,
    /// Byte offset of `<|cursor|>`, when the model put one in.
    pub cursor: Option<usize>,
}

/// Finds the only occurrence of the target text (without the marker) in
/// `text`. Line endings in the target are normalized to `\n`, like buffer text.
pub fn locate(text: &str, target_content: &str) -> Result<TargetMatch, LocateError> {
    let (target, cursor) = split_cursor_marker(&target_content.replace("\r\n", "\n"));
    if target.trim().is_empty() {
        return Err(LocateError::Empty);
    }
    let start = find_unique(text, &target)?;
    Ok(TargetMatch {
        range: start..start + target.len(),
        cursor: cursor.map(|cursor| start + cursor),
    })
}

fn find_unique(text: &str, needle: &str) -> Result<usize, LocateError> {
    let first = text.find(needle).ok_or(LocateError::NotFound)?;
    let next_start = first
        + text[first..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
    if text[next_start..].contains(needle) {
        return Err(LocateError::Ambiguous);
    }
    Ok(first)
}

/// Picks the file to jump to: the target file (index 0) when the text is
/// there, otherwise the one other file that has it exactly once.
pub fn locate_in_files(
    texts: &[&str],
    target_content: &str,
) -> Result<(usize, TargetMatch), LocateError> {
    let Some((first, others)) = texts.split_first() else {
        return Err(LocateError::NotFound);
    };
    match locate(first, target_content) {
        Ok(found) => return Ok((0, found)),
        Err(LocateError::NotFound) => {}
        Err(error) => return Err(error),
    }
    let mut found = None;
    for (index, text) in others.iter().enumerate() {
        match locate(text, target_content) {
            Ok(target_match) if found.is_none() => found = Some((index + 1, target_match)),
            Ok(_) | Err(LocateError::Ambiguous) => return Err(LocateError::Ambiguous),
            Err(_) => {}
        }
    }
    found.ok_or(LocateError::NotFound)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    const COMPLETE: &str = " \"    print(v.size())\\n    w = <|cursor|>add(v, 1)\\n\",\n\t\t\"ReplacementContent\": \"    print(v.size())\\n    w = v.add(1)\\n\"\n\t}]\n}\n</replace_file_content>\n";

    #[test]
    fn test_parse_complete_completion() {
        assert_eq!(
            parse_completion(COMPLETE),
            Ok(Some(JumpCompletion {
                target_content: "    print(v.size())\n    w = <|cursor|>add(v, 1)\n".to_string(),
                replacement_content: "    print(v.size())\n    w = v.add(1)\n".to_string(),
            }))
        );
    }

    #[test]
    fn test_parse_empty_target_means_no_jump() {
        assert_eq!(
            parse_completion(" \"\",\n\t\t\"ReplacementContent\": \"\"\n\t}]\n}"),
            Ok(None)
        );
        assert_eq!(parse_completion(" \"\""), Ok(None));
    }

    #[test]
    fn test_parse_incomplete_json() {
        for cut in [
            "",
            " ",
            " \"    print(v.si",
            " \"a\\",
            " \"a\"",
            " \"a\",\n\t\t\"Replacement",
            " \"a\",\n\t\t\"ReplacementContent\"",
            " \"a\",\n\t\t\"ReplacementContent\": ",
            " \"a\",\n\t\t\"ReplacementContent\": \"b",
        ] {
            assert_eq!(
                parse_completion(cut),
                Err(ParseError::Incomplete),
                "{cut:?}"
            );
        }
        // Once both strings are there the trailing JSON doesn't matter.
        let cut_tail = &COMPLETE[..COMPLETE.find("\n\t}]").expect("tail")];
        assert!(matches!(parse_completion(cut_tail), Ok(Some(_))));
    }

    #[test]
    fn test_parse_malformed() {
        assert_eq!(parse_completion(" null"), Err(ParseError::Malformed));
        assert_eq!(
            parse_completion(" \"a\" \"ReplacementContent\": \"b\""),
            Err(ParseError::Malformed)
        );
        assert_eq!(
            parse_completion(" \"a\", \"Other\": \"b\""),
            Err(ParseError::Malformed)
        );
    }

    #[test]
    fn test_locate_with_and_without_cursor() {
        let text = "a = 1\nb = add(a)\nc = 3\n";
        assert_eq!(
            locate(text, "b = <|cursor|>add(a)"),
            Ok(TargetMatch {
                range: 6..16,
                cursor: Some(10),
            })
        );
        assert_eq!(
            locate(text, "b = add(a)\nc"),
            Ok(TargetMatch {
                range: 6..18,
                cursor: None,
            })
        );
        assert_eq!(locate(text, "<|cursor|>"), Err(LocateError::Empty));
        assert_eq!(locate(text, "d = 4"), Err(LocateError::NotFound));
    }

    #[test]
    fn test_locate_rejects_ambiguous_text() {
        let text = "x = 1\nprint(x)\nprint(x)\n";
        assert_eq!(
            locate(text, "<|cursor|>print(x)"),
            Err(LocateError::Ambiguous)
        );
        // Overlapping occurrences count too.
        assert_eq!(locate("aaa", "a<|cursor|>a"), Err(LocateError::Ambiguous));
        assert_eq!(
            locate(text, "x = 1\n<|cursor|>print(x)").map(|found| found.cursor),
            Ok(Some(6))
        );
    }

    #[test]
    fn test_locate_multibyte_and_crlf() {
        let text = "名字 = \"世界🙂\"\nprint(名字)\n";
        let found = locate(text, "print(<|cursor|>名字)").expect("found");
        let expected_start = text.find("print(").expect("print");
        assert_eq!(found.range.start, expected_start);
        assert_eq!(found.cursor, Some(expected_start + "print(".len()));
        assert_eq!(&text[found.range], "print(名字)");

        let found = locate(text, "\"世界🙂<|cursor|>\"\r\nprint").expect("found");
        let quote = text.find("\"世界").expect("quote");
        assert_eq!(found.cursor, Some(quote + "\"世界🙂".len()));
        assert_eq!(&text[found.range], "\"世界🙂\"\nprint");
    }

    #[test]
    fn test_locate_in_files_prefers_the_target_file() {
        let target = "fn a() {}\nfn b() {}\n";
        let other = "fn b() {}\nfn c() {}\n";
        assert_eq!(
            locate_in_files(&[target, other], "fn <|cursor|>b() {}").map(|(index, _)| index),
            Ok(0)
        );
        assert_eq!(
            locate_in_files(&[target, other], "fn <|cursor|>c() {}").map(|(index, _)| index),
            Ok(1)
        );
        assert_eq!(
            locate_in_files(&[target, other, other], "fn <|cursor|>c() {}"),
            Err(LocateError::Ambiguous)
        );
        assert_eq!(
            locate_in_files(&[target, other], "fn d() {}"),
            Err(LocateError::NotFound)
        );
    }
}
