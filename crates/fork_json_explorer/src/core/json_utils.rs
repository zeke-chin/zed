use super::json_value::{JsonValue, quote_json_string, script_trim};
use super::jsonc_tree::{JsoncNode, parse_tree};
use super::python_repr::python_repr_to_json;
use super::quoted_line_breaks::escape_quoted_line_breaks;

const UNWRAP_DEPTH: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Json,
    Python,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatSourceKind {
    Json,
    JsonStr,
    JsonRepaired,
    PythonStr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonFormatResult {
    pub formatted: String,
    pub source_kind: FormatSourceKind,
}

/// Offsets and lengths are byte offsets into the searched text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawNestedJsonHit {
    pub offset: usize,
    pub length: usize,
    pub parsed_text: String,
    pub key_path: String,
    pub source_kind: SourceKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawStringValueHit {
    pub offset: usize,
    pub length: usize,
    pub key_path: String,
    pub raw_value: String,
    pub parsed_text: Option<String>,
    pub source_kind: SourceKind,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StructuredUnwrap {
    pub value: JsonValue,
    pub source_kind: SourceKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyPathSegment {
    Key(String),
    Index(usize),
}

pub fn try_unwrap_json_string(raw: &str) -> Option<JsonValue> {
    let mut candidate = script_trim(raw).to_string();
    if candidate.is_empty() {
        return None;
    }

    for _ in 0..UNWRAP_DEPTH {
        match JsonValue::parse(&candidate)? {
            container if container.is_container() => return Some(container),
            JsonValue::String(text) => {
                let next = script_trim(&text);
                if next.is_empty() || next == candidate {
                    return None;
                }
                candidate = next.to_string();
            }
            _ => return None,
        }
    }

    None
}

/// Unwrap a string as a JSON container (up to 4 levels of string wrapping) or
/// as a Python repr literal.
pub fn try_unwrap_structured(raw: &str) -> Option<StructuredUnwrap> {
    if let Some(value) = try_unwrap_json_string(raw) {
        return Some(StructuredUnwrap {
            value,
            source_kind: SourceKind::Json,
        });
    }

    let python_result = python_repr_to_json(script_trim(raw))?;
    let value = JsonValue::parse(&python_result.json)?;
    value.is_container().then_some(StructuredUnwrap {
        value,
        source_kind: SourceKind::Python,
    })
}

pub fn format_json_or_json_string(text: &str) -> Option<JsonFormatResult> {
    let trimmed = script_trim(text);
    if trimmed.is_empty() {
        return None;
    }

    let repaired = escape_quoted_line_breaks(trimmed);
    let was_repaired = repaired != trimmed;

    let mut candidate = repaired.clone();
    let mut parsed_from_string = false;

    for _ in 0..UNWRAP_DEPTH {
        let Some(parsed) = JsonValue::parse(&candidate) else {
            break;
        };

        if parsed.is_container() {
            let base_kind = if parsed_from_string {
                FormatSourceKind::JsonStr
            } else {
                FormatSourceKind::Json
            };
            let source_kind = if base_kind == FormatSourceKind::Json && was_repaired {
                FormatSourceKind::JsonRepaired
            } else {
                base_kind
            };
            return Some(JsonFormatResult {
                formatted: parsed.to_pretty_string(),
                source_kind,
            });
        }

        let JsonValue::String(inner) = parsed else {
            return None;
        };
        let next = script_trim(&inner);
        if next.is_empty() || next == candidate {
            return None;
        }
        candidate = next.to_string();
        parsed_from_string = true;
    }

    let python_result = python_repr_to_json(&repaired)?;
    let parsed = JsonValue::parse(&python_result.json)?;
    parsed.is_container().then(|| JsonFormatResult {
        formatted: parsed.to_pretty_string(),
        source_kind: FormatSourceKind::PythonStr,
    })
}

/// Compact JSON container turned into a JSON string literal.
pub fn stringify_json_text(text: &str) -> Option<String> {
    let trimmed = script_trim(text);
    if trimmed.is_empty() {
        return None;
    }
    let parsed = JsonValue::parse(trimmed)?;
    if !parsed.is_container() {
        return None;
    }
    Some(quote_json_string(&parsed.to_compact_string()))
}

pub fn find_nested_json_strings_raw(text: &str) -> Vec<RawNestedJsonHit> {
    find_string_values_raw(text)
        .into_iter()
        .filter_map(|hit| {
            Some(RawNestedJsonHit {
                offset: hit.offset,
                length: hit.length,
                parsed_text: hit.parsed_text?,
                key_path: hit.key_path,
                source_kind: hit.source_kind,
            })
        })
        .collect()
}

pub fn find_string_values_raw(text: &str) -> Vec<RawStringValueHit> {
    let mut hits = Vec::new();
    if let Some(root) = parse_tree(text) {
        let mut path = Vec::new();
        walk_all_strings(&root, &mut path, &mut hits);
    }
    hits
}

/// The string value whose literal contains `byte_offset`, boundaries included.
pub fn find_string_value_at_offset(text: &str, byte_offset: usize) -> Option<RawStringValueHit> {
    find_string_values_raw(text)
        .into_iter()
        .find(|hit| hit.offset <= byte_offset && byte_offset <= hit.offset + hit.length)
}

fn walk_all_strings(
    node: &JsoncNode,
    path: &mut Vec<KeyPathSegment>,
    hits: &mut Vec<RawStringValueHit>,
) {
    match node {
        JsoncNode::String {
            offset,
            length,
            value,
        } => {
            let unwrapped = try_unwrap_structured(value);
            hits.push(RawStringValueHit {
                offset: *offset,
                length: *length,
                key_path: format_key_path(path),
                raw_value: value.clone(),
                parsed_text: unwrapped
                    .as_ref()
                    .map(|unwrapped| unwrapped.value.to_pretty_string()),
                source_kind: unwrapped.map_or(SourceKind::Json, |unwrapped| unwrapped.source_kind),
            });
        }
        JsoncNode::Object(properties) => {
            for property in properties {
                let Some(value) = &property.value else {
                    continue;
                };
                path.push(KeyPathSegment::Key(property.key.clone()));
                walk_all_strings(value, path, hits);
                path.pop();
            }
        }
        JsoncNode::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                path.push(KeyPathSegment::Index(index));
                walk_all_strings(item, path, hits);
                path.pop();
            }
        }
        JsoncNode::Literal => {}
    }
}

pub fn format_key_path(path: &[KeyPathSegment]) -> String {
    if path.is_empty() {
        return "root".to_string();
    }

    let mut result = String::new();
    for segment in path {
        match segment {
            KeyPathSegment::Index(index) => result.push_str(&format!("[{index}]")),
            KeyPathSegment::Key(key) if result.is_empty() => result.push_str(key),
            KeyPathSegment::Key(key) => {
                result.push('.');
                result.push_str(key);
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested(value: &str) -> String {
        quote_json_string(value)
    }

    fn compact(value: &Option<JsonValue>) -> String {
        value
            .as_ref()
            .map(JsonValue::to_compact_string)
            .unwrap_or_default()
    }

    #[test]
    fn unwrap_json_string() {
        assert_eq!(compact(&try_unwrap_json_string(r#"{"a":1}"#)), r#"{"a":1}"#);
        assert_eq!(compact(&try_unwrap_json_string("[1,2,3]")), "[1,2,3]");
        assert_eq!(
            compact(&try_unwrap_json_string(&nested(r#"{"a":1}"#))),
            r#"{"a":1}"#
        );
        for text in [
            "42",
            "true",
            "null",
            "\"hello\"",
            "not json",
            "{a:1}",
            "",
            "   ",
            "{'a': 1}",
        ] {
            assert_eq!(try_unwrap_json_string(text), None, "{text:?}");
        }
    }

    #[test]
    fn unwrap_depth_is_limited() {
        let mut value = r#"{"a":1}"#.to_string();
        for _ in 0..10 {
            value = nested(&value);
        }
        assert_eq!(try_unwrap_json_string(&value), None);
    }

    #[test]
    fn unwrap_structured() {
        let json = try_unwrap_structured(r#"{"a":1}"#).unwrap();
        assert_eq!(json.source_kind, SourceKind::Json);
        let double = try_unwrap_structured(&nested(r#"{"a":1}"#)).unwrap();
        assert_eq!(double.source_kind, SourceKind::Json);
        assert_eq!(double.value.to_compact_string(), r#"{"a":1}"#);
        let python = try_unwrap_structured("{'a': 1, 'b': True, 'c': None}").unwrap();
        assert_eq!(python.source_kind, SourceKind::Python);
        assert_eq!(
            python.value.to_compact_string(),
            r#"{"a":1,"b":true,"c":null}"#
        );
        let list = try_unwrap_structured("[1, 'a', True]").unwrap();
        assert_eq!(list.source_kind, SourceKind::Python);
        for text in ["True", "'hello'", "not anything", ""] {
            assert_eq!(try_unwrap_structured(text), None, "{text:?}");
        }
    }

    #[test]
    fn format_plain_json() {
        let result = format_json_or_json_string(r#"{"a":1}"#).unwrap();
        assert_eq!(result.source_kind, FormatSourceKind::Json);
        assert_eq!(result.formatted, "{\n\t\"a\": 1\n}");
    }

    #[test]
    fn format_json_string() {
        let result = format_json_or_json_string(&nested(r#"{"a":1}"#)).unwrap();
        assert_eq!(result.source_kind, FormatSourceKind::JsonStr);
    }

    #[test]
    fn format_rejects_non_containers_and_invalid_input() {
        for text in ["42", "\"plain string\"", "not json", ""] {
            assert_eq!(format_json_or_json_string(text), None, "{text:?}");
        }
    }

    #[test]
    fn format_python_repr() {
        let result = format_json_or_json_string("{'a': 1, 'b': True}").unwrap();
        assert_eq!(result.source_kind, FormatSourceKind::PythonStr);
        assert_eq!(result.formatted, "{\n\t\"a\": 1,\n\t\"b\": true\n}");
    }

    #[test]
    fn format_repairs_terminal_wrapped_json() {
        let input = "{\"request_id\":\"req-001\",\"session\":\"{'token': 'abc123', 'expires_in': 3600,\n  'roles':\n    ['admin', 'user']}\"}";
        let result = format_json_or_json_string(input).unwrap();
        assert_eq!(result.source_kind, FormatSourceKind::JsonRepaired);
        assert!(result.formatted.contains("\"request_id\": \"req-001\""));
        assert!(
            result
                .formatted
                .contains("\"session\": \"{'token': 'abc123'")
        );
    }

    #[test]
    fn clean_and_pretty_json_stay_json_kind() {
        let clean = format_json_or_json_string(r#"{"a": 1, "b": [true, null]}"#).unwrap();
        assert_eq!(clean.source_kind, FormatSourceKind::Json);
        let pretty = format_json_or_json_string("{\n  \"a\": 1,\n  \"b\": 2\n}").unwrap();
        assert_eq!(pretty.source_kind, FormatSourceKind::Json);
    }

    #[test]
    fn format_repairs_python_repr_with_wrapped_strings() {
        let result = format_json_or_json_string("{'a': 'line1\nline2', 'b': 2}").unwrap();
        assert_eq!(result.source_kind, FormatSourceKind::PythonStr);
        assert_eq!(
            result.formatted,
            "{\n\t\"a\": \"line1\\nline2\",\n\t\"b\": 2\n}"
        );
    }

    #[test]
    fn unsupported_python_literals_are_left_alone() {
        for text in [
            "{'t': datetime(2024, 1, 1)}",
            "{'a': b'raw'}",
            "{'a', 'b'}",
            "{1: 'a'}",
            "{'obj': <Foo object at 0x100>}",
            "def foo():\n    return {'a': 1}",
        ] {
            assert_eq!(format_json_or_json_string(text), None, "{text:?}");
        }
    }

    #[test]
    fn stringify() {
        assert_eq!(
            stringify_json_text(r#"{ "a" : 1 }"#),
            Some(r#""{\"a\":1}""#.to_string())
        );
        for text in ["42", "bad", ""] {
            assert_eq!(stringify_json_text(text), None, "{text:?}");
        }
    }

    #[test]
    fn big_numbers_keep_their_digits() {
        let input = r#"{"big":12345678901234567890,"precise":0.1000000000000000055,"sci":1.5E+10}"#;
        let formatted = format_json_or_json_string(input).unwrap().formatted;
        assert_eq!(
            formatted,
            "{\n\t\"big\": 12345678901234567890,\n\t\"precise\": 0.1000000000000000055,\n\t\"sci\": 1.5E+10\n}"
        );
        let stringified = stringify_json_text(&formatted).unwrap();
        assert_eq!(
            stringified,
            r#""{\"big\":12345678901234567890,\"precise\":0.1000000000000000055,\"sci\":1.5E+10}""#
        );
    }

    #[test]
    fn python_big_numbers_keep_their_digits() {
        let result =
            format_json_or_json_string("{'a': 12345678901234567890, 'b': 0.1000000000000000055}")
                .unwrap();
        assert_eq!(
            result.formatted,
            "{\n\t\"a\": 12345678901234567890,\n\t\"b\": 0.1000000000000000055\n}"
        );
    }

    #[test]
    fn toggling_twice_restores_the_original() {
        let formatted = "{\n\t\"id\": 12345678901234567890,\n\t\"ratio\": 0.1000000000000000055,\n\t\"name\": \"你好😀\",\n\t\"list\": [\n\t\t1,\n\t\t{\n\t\t\t\"k\": null\n\t\t}\n\t],\n\t\"empty\": {},\n\t\"none\": []\n}";
        let as_string = stringify_json_text(formatted).unwrap();
        let restored = format_json_or_json_string(&as_string).unwrap();
        assert_eq!(restored.source_kind, FormatSourceKind::JsonStr);
        assert_eq!(restored.formatted, formatted);

        let string_form = as_string.as_str();
        let to_json = format_json_or_json_string(string_form).unwrap().formatted;
        assert_eq!(stringify_json_text(&to_json).unwrap(), string_form);
    }

    #[test]
    fn key_paths() {
        use KeyPathSegment::{Index, Key};
        let key = |name: &str| Key(name.to_string());
        assert_eq!(format_key_path(&[]), "root");
        assert_eq!(format_key_path(&[key("payload")]), "payload");
        assert_eq!(
            format_key_path(&[key("data"), key("payload")]),
            "data.payload"
        );
        assert_eq!(format_key_path(&[key("items"), Index(0)]), "items[0]");
        assert_eq!(
            format_key_path(&[key("items"), Index(2), key("config")]),
            "items[2].config"
        );
        assert_eq!(format_key_path(&[Index(0)]), "[0]");
    }

    #[test]
    fn nested_strings_in_flat_object() {
        let text = format!(r#"{{"id":1,"payload":{}}}"#, nested(r#"{"user":"zeke"}"#));
        let hits = find_nested_json_strings_raw(&text);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key_path, "payload");
        assert_eq!(hits[0].parsed_text, "{\n\t\"user\": \"zeke\"\n}");
    }

    #[test]
    fn multiple_nested_strings() {
        let text = format!(
            r#"{{"first":{},"second":{}}}"#,
            nested(r#"{"a":1}"#),
            nested("[1,2,3]")
        );
        let mut paths: Vec<String> = find_nested_json_strings_raw(&text)
            .into_iter()
            .map(|hit| hit.key_path)
            .collect();
        paths.sort();
        assert_eq!(paths, ["first", "second"]);
    }

    #[test]
    fn primitive_strings_are_not_nested() {
        assert!(
            find_nested_json_strings_raw(r#"{"num":"42","bool":"true","str":"hello"}"#).is_empty()
        );
        assert!(find_nested_json_strings_raw(r#"{"a":1,"b":[1,2],"c":"plain"}"#).is_empty());
        assert!(find_nested_json_strings_raw("").is_empty());
    }

    #[test]
    fn nested_paths_through_objects_and_arrays() {
        let inner = nested(r#"{"deep":true}"#);
        let hits = find_nested_json_strings_raw(&format!(r#"{{"outer":{{"inner":{inner}}}}}"#));
        assert_eq!(hits[0].key_path, "outer.inner");
        let hits = find_nested_json_strings_raw(&format!(r#"{{"items":[null,{inner}]}}"#));
        assert_eq!(hits[0].key_path, "items[1]");
    }

    #[test]
    fn jsonc_comments_and_trailing_commas() {
        let inner = nested(r#"{"a":1}"#);
        let text = format!("{{\n// a comment\n\"payload\":{inner},\n}}");
        let hits = find_nested_json_strings_raw(&text);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key_path, "payload");
    }

    #[test]
    fn hit_covers_the_whole_literal() {
        let payload = nested(r#"{"a":1}"#);
        let text = format!(r#"{{"payload":{payload}}}"#);
        let hits = find_nested_json_strings_raw(&text);
        assert_eq!(
            &text[hits[0].offset..hits[0].offset + hits[0].length],
            payload
        );
    }

    #[test]
    fn all_string_values() {
        let hits = find_string_values_raw(r#"{"name":"zeke","age":"30","empty":""}"#);
        assert_eq!(hits.len(), 3);
        assert!(hits.iter().all(|hit| hit.parsed_text.is_none()));
        let hits = find_string_values_raw(r#"{"tags":["a","b","c"]}"#);
        let paths: Vec<&str> = hits.iter().map(|hit| hit.key_path.as_str()).collect();
        assert_eq!(paths, ["tags[0]", "tags[1]", "tags[2]"]);
    }

    #[test]
    fn string_values_expose_unescaped_text_and_source_kind() {
        let text = format!(r#"{{"plain":"hello","data":{}}}"#, nested(r#"{"a":1}"#));
        let hits = find_string_values_raw(&text);
        assert_eq!(hits[0].raw_value, "hello");
        assert_eq!(hits[0].parsed_text, None);
        assert_eq!(hits[0].source_kind, SourceKind::Json);
        assert_eq!(hits[1].parsed_text.as_deref(), Some("{\n\t\"a\": 1\n}"));
        assert_eq!(hits[1].source_kind, SourceKind::Json);

        let hits = find_string_values_raw(r#"{"msg":"line1\nline2\tindented"}"#);
        assert_eq!(hits[0].raw_value, "line1\nline2\tindented");

        let hits = find_string_values_raw(r#"{"log":"{'user':'alice','ok':True}"}"#);
        assert_eq!(hits[0].source_kind, SourceKind::Python);
        assert_eq!(
            hits[0].parsed_text.as_deref(),
            Some("{\n\t\"user\": \"alice\",\n\t\"ok\": true\n}")
        );
    }

    #[test]
    fn find_by_byte_offset_with_cjk_and_emoji() {
        let text = "{\"名字\":\"你好😀\",\"n\":{\"k\":\"值\"}}";
        let hit_start = text.find("\"你好").unwrap();
        let literal_length = "\"你好😀\"".len();
        for offset in [
            hit_start,
            hit_start + 4,
            hit_start + 7,
            hit_start + literal_length,
        ] {
            let hit = find_string_value_at_offset(text, offset).unwrap();
            assert_eq!(hit.key_path, "名字");
            assert_eq!(hit.raw_value, "你好😀");
            assert_eq!(&text[hit.offset..hit.offset + hit.length], "\"你好😀\"");
        }
        let nested_offset = text.find('值').unwrap();
        let hit = find_string_value_at_offset(text, nested_offset).unwrap();
        assert_eq!(hit.key_path, "n.k");
        assert_eq!(find_string_value_at_offset(text, 1), None);
    }
}
