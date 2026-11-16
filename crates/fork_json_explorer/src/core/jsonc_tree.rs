//! Error tolerant JSONC tree, modelled on `jsonc-parser`'s `parseTree`:
//! comments are skipped, trailing commas and other malformed input still yield
//! a tree. Offsets and lengths are byte offsets into the source text.

const MAX_DEPTH: usize = 512;

#[derive(Debug, Clone, PartialEq)]
pub enum JsoncNode {
    Object(Vec<JsoncProperty>),
    Array(Vec<JsoncNode>),
    String {
        offset: usize,
        length: usize,
        value: String,
    },
    Literal,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JsoncProperty {
    pub key: String,
    pub value: Option<JsoncNode>,
}

pub fn parse_tree(text: &str) -> Option<JsoncNode> {
    let mut parser = Parser {
        scanner: Scanner {
            text,
            position: 0,
            token_offset: 0,
            token_value: String::new(),
        },
        token: Token::Eof,
    };
    parser.scan_next();
    if parser.token == Token::Eof {
        return None;
    }
    parser.parse_value(0).ok().flatten()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Token {
    OpenBrace,
    CloseBrace,
    OpenBracket,
    CloseBracket,
    Comma,
    Colon,
    Null,
    True,
    False,
    Str,
    Number,
    LineComment,
    BlockComment,
    LineBreak,
    Trivia,
    Unknown,
    Eof,
}

struct TooDeep;

fn is_trivia_character(character: char) -> bool {
    matches!(
        character,
        ' ' | '\t' | '\u{b}' | '\u{c}' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

fn is_line_break(character: char) -> bool {
    matches!(character, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn is_unknown_content(character: char) -> bool {
    if is_trivia_character(character) || is_line_break(character) {
        return false;
    }
    !matches!(character, '}' | ']' | '{' | '[' | '"' | ':' | ',' | '/')
}

struct Scanner<'a> {
    text: &'a str,
    position: usize,
    token_offset: usize,
    token_value: String,
}

impl Scanner<'_> {
    fn char_at(&self, position: usize) -> Option<char> {
        self.text.get(position..)?.chars().next()
    }

    fn byte_at(&self, position: usize) -> Option<u8> {
        self.text.as_bytes().get(position).copied()
    }

    fn scan(&mut self) -> Token {
        self.token_offset = self.position;
        self.token_value.clear();
        let Some(character) = self.char_at(self.position) else {
            return Token::Eof;
        };

        if is_trivia_character(character) {
            while let Some(next) = self.char_at(self.position) {
                if !is_trivia_character(next) {
                    break;
                }
                self.position += next.len_utf8();
            }
            return Token::Trivia;
        }

        if is_line_break(character) {
            self.position += character.len_utf8();
            if character == '\r' && self.byte_at(self.position) == Some(b'\n') {
                self.position += 1;
            }
            return Token::LineBreak;
        }

        match character {
            '{' => self.single(Token::OpenBrace),
            '}' => self.single(Token::CloseBrace),
            '[' => self.single(Token::OpenBracket),
            ']' => self.single(Token::CloseBracket),
            ':' => self.single(Token::Colon),
            ',' => self.single(Token::Comma),
            '"' => self.scan_string(),
            '/' => self.scan_slash(),
            '-' => {
                self.position += 1;
                if matches!(self.byte_at(self.position), Some(b'0'..=b'9')) {
                    self.scan_number();
                    Token::Number
                } else {
                    Token::Unknown
                }
            }
            '0'..='9' => {
                self.scan_number();
                Token::Number
            }
            _ => self.scan_literal(),
        }
    }

    fn single(&mut self, token: Token) -> Token {
        self.position += 1;
        token
    }

    fn scan_slash(&mut self) -> Token {
        match self.byte_at(self.position + 1) {
            Some(b'/') => {
                self.position += 2;
                while let Some(next) = self.char_at(self.position) {
                    if is_line_break(next) {
                        break;
                    }
                    self.position += next.len_utf8();
                }
                Token::LineComment
            }
            Some(b'*') => {
                self.position += 2;
                match self
                    .text
                    .get(self.position..)
                    .and_then(|rest| rest.find("*/"))
                {
                    Some(relative) => self.position += relative + 2,
                    None => self.position = self.text.len(),
                }
                Token::BlockComment
            }
            _ => {
                self.position += 1;
                Token::Unknown
            }
        }
    }

    fn scan_digits(&mut self) {
        while matches!(self.byte_at(self.position), Some(b'0'..=b'9')) {
            self.position += 1;
        }
    }

    fn scan_number(&mut self) {
        if self.byte_at(self.position) == Some(b'0') {
            self.position += 1;
        } else {
            self.scan_digits();
        }
        if self.byte_at(self.position) == Some(b'.') {
            self.position += 1;
            if matches!(self.byte_at(self.position), Some(b'0'..=b'9')) {
                self.scan_digits();
            } else {
                return;
            }
        }
        if matches!(self.byte_at(self.position), Some(b'e' | b'E')) {
            self.position += 1;
            if matches!(self.byte_at(self.position), Some(b'+' | b'-')) {
                self.position += 1;
            }
            if matches!(self.byte_at(self.position), Some(b'0'..=b'9')) {
                self.scan_digits();
            }
        }
    }

    fn scan_literal(&mut self) -> Token {
        let start = self.position;
        while let Some(next) = self.char_at(self.position) {
            if !is_unknown_content(next) {
                break;
            }
            self.position += next.len_utf8();
        }
        if start == self.position {
            if let Some(next) = self.char_at(self.position) {
                self.position += next.len_utf8();
            }
            return Token::Unknown;
        }
        match self.text.get(start..self.position) {
            Some("true") => Token::True,
            Some("false") => Token::False,
            Some("null") => Token::Null,
            _ => Token::Unknown,
        }
    }

    fn scan_hex_digits(&mut self, count: usize) -> Option<u16> {
        let mut value: u16 = 0;
        let mut digits = 0;
        while digits < count {
            let Some(nibble) = self
                .byte_at(self.position)
                .and_then(|byte| char::from(byte).to_digit(16))
            else {
                break;
            };
            value = value.wrapping_mul(16).wrapping_add(nibble as u16);
            self.position += 1;
            digits += 1;
        }
        (digits == count).then_some(value)
    }

    fn scan_string(&mut self) -> Token {
        self.position += 1;
        let mut units: Vec<u16> = Vec::new();
        while let Some(character) = self.char_at(self.position) {
            if character == '"' {
                self.position += 1;
                break;
            }
            if character == '\\' {
                self.position += 1;
                let Some(escaped) = self.char_at(self.position) else {
                    break;
                };
                self.position += escaped.len_utf8();
                match escaped {
                    '"' => units.push(0x22),
                    '\\' => units.push(0x5C),
                    '/' => units.push(0x2F),
                    'b' => units.push(0x08),
                    'f' => units.push(0x0C),
                    'n' => units.push(0x0A),
                    'r' => units.push(0x0D),
                    't' => units.push(0x09),
                    'u' => {
                        if let Some(unit) = self.scan_hex_digits(4) {
                            units.push(unit);
                        }
                    }
                    _ => {}
                }
                continue;
            }
            if matches!(character, '\n' | '\r') {
                break;
            }
            self.position += character.len_utf8();
            let mut buffer = [0u16; 2];
            units.extend_from_slice(character.encode_utf16(&mut buffer));
        }
        self.token_value = String::from_utf16_lossy(&units);
        Token::Str
    }
}

struct Parser<'a> {
    scanner: Scanner<'a>,
    token: Token,
}

impl Parser<'_> {
    fn scan_next(&mut self) {
        loop {
            let token = self.scanner.scan();
            match token {
                Token::LineComment | Token::BlockComment | Token::Unknown => {}
                Token::Trivia | Token::LineBreak => {}
                other => {
                    self.token = other;
                    return;
                }
            }
        }
    }

    fn skip_until(&mut self, stop_tokens: &[Token]) {
        while self.token != Token::Eof && !stop_tokens.contains(&self.token) {
            self.scan_next();
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<Option<JsoncNode>, TooDeep> {
        if depth > MAX_DEPTH {
            return Err(TooDeep);
        }
        match self.token {
            Token::OpenBracket => self.parse_array(depth).map(Some),
            Token::OpenBrace => self.parse_object(depth).map(Some),
            Token::Str => {
                let node = JsoncNode::String {
                    offset: self.scanner.token_offset,
                    length: self.scanner.position - self.scanner.token_offset,
                    value: std::mem::take(&mut self.scanner.token_value),
                };
                self.scan_next();
                Ok(Some(node))
            }
            Token::Number | Token::True | Token::False | Token::Null => {
                self.scan_next();
                Ok(Some(JsoncNode::Literal))
            }
            _ => Ok(None),
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<JsoncNode, TooDeep> {
        self.scan_next();
        let mut items = Vec::new();
        while self.token != Token::CloseBracket && self.token != Token::Eof {
            if self.token == Token::Comma {
                self.scan_next();
            }
            match self.parse_value(depth + 1)? {
                Some(node) => items.push(node),
                None => self.skip_until(&[Token::CloseBracket, Token::Comma]),
            }
        }
        if self.token == Token::CloseBracket {
            self.scan_next();
        }
        Ok(JsoncNode::Array(items))
    }

    fn parse_object(&mut self, depth: usize) -> Result<JsoncNode, TooDeep> {
        self.scan_next();
        let mut properties = Vec::new();
        while self.token != Token::CloseBrace && self.token != Token::Eof {
            if self.token == Token::Comma {
                self.scan_next();
            }
            if let Some(property) = self.parse_property(depth)? {
                properties.push(property);
            }
        }
        if self.token == Token::CloseBrace {
            self.scan_next();
        }
        Ok(JsoncNode::Object(properties))
    }

    fn parse_property(&mut self, depth: usize) -> Result<Option<JsoncProperty>, TooDeep> {
        let stop_tokens = [Token::CloseBrace, Token::Comma];
        if self.token != Token::Str {
            self.skip_until(&stop_tokens);
            return Ok(None);
        }
        let key = std::mem::take(&mut self.scanner.token_value);
        self.scan_next();
        let mut value = None;
        if self.token == Token::Colon {
            self.scan_next();
            value = self.parse_value(depth + 1)?;
            if value.is_none() {
                self.skip_until(&stop_tokens);
            }
        } else {
            self.skip_until(&stop_tokens);
        }
        Ok(Some(JsoncProperty { key, value }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_blank_input_has_no_tree() {
        assert_eq!(parse_tree(""), None);
        assert_eq!(parse_tree("  // only a comment\n"), None);
    }

    #[test]
    fn comments_and_trailing_commas_are_tolerated() {
        let text = "{\n// c\n\"a\": [\"x\", /* b */ \"y\",],\n}";
        let Some(JsoncNode::Object(properties)) = parse_tree(text) else {
            panic!("expected object");
        };
        assert_eq!(properties.len(), 1);
        let Some(JsoncNode::Array(items)) = &properties[0].value else {
            panic!("expected array");
        };
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn string_offsets_are_byte_offsets() {
        let text = "[\"你好\", \"x\"]";
        let Some(JsoncNode::Array(items)) = parse_tree(text) else {
            panic!("expected array");
        };
        assert_eq!(
            items[0],
            JsoncNode::String {
                offset: 1,
                length: 8,
                value: "你好".to_string()
            }
        );
        let JsoncNode::String { offset, length, .. } = &items[1] else {
            panic!("expected string");
        };
        assert_eq!(&text[*offset..offset + length], "\"x\"");
    }

    #[test]
    fn malformed_input_still_yields_partial_tree() {
        let Some(JsoncNode::Object(properties)) = parse_tree("{\"a\": \"x\" \"b\": \"y\", \"c\"")
        else {
            panic!("expected object");
        };
        assert_eq!(properties[0].key, "a");
        assert!(
            properties
                .iter()
                .any(|property| property.key == "c" && property.value.is_none())
        );
    }

    #[test]
    fn deep_nesting_is_rejected_without_overflow() {
        assert_eq!(parse_tree(&"[".repeat(100_000)), None);
    }
}
