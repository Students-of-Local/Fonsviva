//! Edits one member of a JSON document in place and leaves every other byte
//! as it was, so a user's own formatting, key order and comments-free layout
//! survive an apply and a revert.

use serde::Serialize;
use serde_json::ser::PrettyFormatter;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    #[error("the file is not plain JSON")]
    NotJson,
    #[error("the file is not a JSON object")]
    NotObject,
    #[error("\"{0}\" is not a JSON object")]
    MemberNotObject(String),
}

#[derive(Debug)]
enum Node {
    Object {
        open: usize,
        close: usize,
        members: Vec<Member>,
    },
    Other {
        start: usize,
        end: usize,
    },
}

#[derive(Debug)]
struct Member {
    key: String,
    key_start: usize,
    value: Node,
}

impl Node {
    fn start(&self) -> usize {
        match self {
            Node::Object { open, .. } => *open,
            Node::Other { start, .. } => *start,
        }
    }

    fn end(&self) -> usize {
        match self {
            Node::Object { close, .. } => close + 1,
            Node::Other { end, .. } => *end,
        }
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    text: &'a str,
    pos: usize,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len()
            && matches!(self.bytes[self.pos], b' ' | b'\t' | b'\n' | b'\r')
        {
            self.pos += 1;
        }
    }

    fn string_end(&mut self) -> usize {
        self.pos += 1;
        while self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b'\\' => self.pos += 2,
                b'"' => {
                    self.pos += 1;
                    return self.pos;
                }
                _ => self.pos += 1,
            }
        }
        self.pos
    }

    fn value(&mut self) -> Result<Node, EditError> {
        self.skip_ws();
        let start = self.pos;
        match self.bytes.get(self.pos) {
            Some(b'{') => self.object(),
            Some(b'[') => {
                self.pos += 1;
                loop {
                    self.skip_ws();
                    if self.bytes.get(self.pos) == Some(&b']') {
                        self.pos += 1;
                        break;
                    }
                    self.value()?;
                    self.skip_ws();
                    match self.bytes.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {
                            self.pos += 1;
                            break;
                        }
                        _ => return Err(EditError::NotJson),
                    }
                }
                Ok(Node::Other {
                    start,
                    end: self.pos,
                })
            }
            Some(b'"') => {
                let end = self.string_end();
                Ok(Node::Other { start, end })
            }
            Some(_) => {
                while self.pos < self.bytes.len()
                    && !matches!(
                        self.bytes[self.pos],
                        b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r'
                    )
                {
                    self.pos += 1;
                }
                Ok(Node::Other {
                    start,
                    end: self.pos,
                })
            }
            None => Err(EditError::NotJson),
        }
    }

    fn object(&mut self) -> Result<Node, EditError> {
        let open = self.pos;
        self.pos += 1;
        let mut members = Vec::new();
        loop {
            self.skip_ws();
            match self.bytes.get(self.pos) {
                Some(b'}') => {
                    let close = self.pos;
                    self.pos += 1;
                    return Ok(Node::Object {
                        open,
                        close,
                        members,
                    });
                }
                Some(b'"') => {
                    let key_start = self.pos;
                    let key_end = self.string_end();
                    let key: String = serde_json::from_str(&self.text[key_start..key_end])
                        .map_err(|_| EditError::NotJson)?;
                    self.skip_ws();
                    if self.bytes.get(self.pos) != Some(&b':') {
                        return Err(EditError::NotJson);
                    }
                    self.pos += 1;
                    let value = self.value()?;
                    members.push(Member {
                        key,
                        key_start,
                        value,
                    });
                    self.skip_ws();
                    if self.bytes.get(self.pos) == Some(&b',') {
                        self.pos += 1;
                    }
                }
                _ => return Err(EditError::NotJson),
            }
        }
    }
}

/// A parsed document. `text` keeps the original bytes; edits splice into it.
pub struct Doc {
    text: String,
    root: Node,
    newline: &'static str,
    indent: String,
}

impl Doc {
    /// Parses `text`. Empty or whitespace-only text is an empty object.
    pub fn parse(text: &str) -> Result<Self, EditError> {
        let text = if text.trim().is_empty() {
            "{}".to_string()
        } else {
            text.to_string()
        };
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(serde_json::Value::Object(_)) => {}
            Ok(_) => return Err(EditError::NotObject),
            Err(_) => return Err(EditError::NotJson),
        }
        let mut parser = Parser {
            bytes: text.as_bytes(),
            text: &text,
            pos: 0,
        };
        let root = parser.value()?;
        let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let indent = detect_indent(&text, &root);
        Ok(Self {
            text,
            root,
            newline,
            indent,
        })
    }

    /// The parsed value at `path`, if every key on it exists.
    pub fn get(&self, path: &[&str]) -> Option<serde_json::Value> {
        let mut node = &self.root;
        for key in path {
            let Node::Object { members, .. } = node else {
                return None;
            };
            node = &members
                .iter()
                .rev()
                .find(|member| member.key == *key)?
                .value;
        }
        serde_json::from_str(&self.text[node.start()..node.end()]).ok()
    }

    /// Sets the member at `path` to `value`, creating parent objects as
    /// needed. Returns the new text.
    pub fn upsert<T: Serialize>(&self, path: &[&str], value: &T) -> Result<String, EditError> {
        let Node::Object { .. } = &self.root else {
            return Err(EditError::NotObject);
        };
        let mut node = &self.root;
        let mut depth = 1;
        for (index, key) in path.iter().enumerate() {
            let Node::Object {
                open,
                close,
                members,
            } = node
            else {
                return Err(EditError::MemberNotObject(path[index - 1].to_string()));
            };
            if let Some(member) = members.iter().rev().find(|member| member.key == *key) {
                if index + 1 == path.len() {
                    let rendered = self.render(value, depth);
                    return Ok(splice(
                        &self.text,
                        member.value.start(),
                        member.value.end(),
                        &rendered,
                    ));
                }
                node = &member.value;
                depth += 1;
                continue;
            }
            let rendered = self.render_nested(&path[index + 1..], value, depth);
            let member_text = format!("{}: {rendered}", quote(key));
            let pad = self.indent.repeat(depth);
            return Ok(match members.last() {
                Some(last) => splice(
                    &self.text,
                    last.value.end(),
                    last.value.end(),
                    &format!(",{}{pad}{member_text}", self.newline),
                ),
                None => splice(
                    &self.text,
                    open + 1,
                    *close,
                    &format!(
                        "{nl}{pad}{member_text}{nl}{}",
                        self.indent.repeat(depth - 1),
                        nl = self.newline
                    ),
                ),
            });
        }
        Err(EditError::NotObject)
    }

    /// Removes the member at `path`. Returns `None` when it is not there.
    pub fn remove(&self, path: &[&str]) -> Option<String> {
        let (last, parents) = path.split_last()?;
        let mut node = &self.root;
        for key in parents {
            let Node::Object { members, .. } = node else {
                return None;
            };
            node = &members
                .iter()
                .rev()
                .find(|member| member.key == *key)?
                .value;
        }
        let Node::Object {
            open,
            close,
            members,
        } = node
        else {
            return None;
        };
        let index = members.iter().rposition(|member| member.key == *last)?;
        let (start, end) = if members.len() == 1 {
            (open + 1, *close)
        } else if index + 1 < members.len() {
            (members[index].key_start, members[index + 1].key_start)
        } else {
            (members[index - 1].value.end(), members[index].value.end())
        };
        Some(splice(&self.text, start, end, ""))
    }

    fn render<T: Serialize>(&self, value: &T, depth: usize) -> String {
        let mut out = Vec::new();
        let formatter = PrettyFormatter::with_indent(self.indent.as_bytes());
        let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
        if value.serialize(&mut serializer).is_err() {
            return "null".to_string();
        }
        let pretty = String::from_utf8(out).unwrap_or_default();
        let pad = format!("{}{}", self.newline, self.indent.repeat(depth));
        pretty.replace('\n', &pad)
    }

    fn render_nested<T: Serialize>(&self, rest: &[&str], value: &T, depth: usize) -> String {
        match rest.split_first() {
            None => self.render(value, depth),
            Some((key, rest)) => format!(
                "{{{nl}{}{}: {}{nl}{}}}",
                self.indent.repeat(depth + 1),
                quote(key),
                self.render_nested(rest, value, depth + 1),
                self.indent.repeat(depth),
                nl = self.newline
            ),
        }
    }
}

fn quote(key: &str) -> String {
    serde_json::to_string(key).unwrap_or_else(|_| format!("\"{key}\""))
}

fn splice(text: &str, start: usize, end: usize, with: &str) -> String {
    let mut out = String::with_capacity(text.len() + with.len());
    out.push_str(&text[..start]);
    out.push_str(with);
    out.push_str(&text[end..]);
    out
}

fn detect_indent(text: &str, root: &Node) -> String {
    if let Node::Object { members, .. } = root
        && let Some(first) = members.first()
    {
        let line_start = text[..first.key_start].rfind('\n').map_or(0, |at| at + 1);
        let pad = &text[line_start..first.key_start];
        if !pad.is_empty() && pad.chars().all(|c| c == ' ' || c == '\t') {
            return pad.to_string();
        }
    }
    "  ".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn insert_then_remove_round_trips() {
        let original =
            "{\n    \"a\": 1,\n    \"providers\": {\n        \"x\": {\"k\": [1, 2]}\n    }\n}\n";
        let doc = Doc::parse(original).unwrap();
        let added = doc
            .upsert(&["providers", "fonsviva"], &json!({"baseUrl": "u"}))
            .unwrap();
        assert!(
            added.contains("        \"fonsviva\": {\n            \"baseUrl\": \"u\"\n        }")
        );
        let removed = Doc::parse(&added)
            .unwrap()
            .remove(&["providers", "fonsviva"])
            .unwrap();
        assert_eq!(removed, original);
    }

    #[test]
    fn creates_missing_parent_and_removes_it() {
        let original = "{\"theme\": \"dark\"}";
        let added = Doc::parse(original)
            .unwrap()
            .upsert(&["providers", "f"], &json!(1))
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&added).unwrap(),
            json!({"theme": "dark", "providers": {"f": 1}})
        );
        let removed = Doc::parse(&added).unwrap().remove(&["providers"]).unwrap();
        assert_eq!(removed, original);
    }

    #[test]
    fn empty_object_and_empty_text() {
        let added = Doc::parse("")
            .unwrap()
            .upsert(&["a", "b"], &json!(true))
            .unwrap();
        assert_eq!(added, "{\n  \"a\": {\n    \"b\": true\n  }\n}");
        let removed = Doc::parse(&added).unwrap().remove(&["a", "b"]).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&removed).unwrap(),
            json!({"a": {}})
        );
    }

    #[test]
    fn removes_middle_member_and_keeps_crlf() {
        let original = "{\r\n  \"a\": 1,\r\n  \"b\": 2,\r\n  \"c\": 3\r\n}";
        let removed = Doc::parse(original).unwrap().remove(&["b"]).unwrap();
        assert_eq!(removed, "{\r\n  \"a\": 1,\r\n  \"c\": 3\r\n}");
        let added = Doc::parse(original)
            .unwrap()
            .upsert(&["d"], &json!({"x": 1}))
            .unwrap();
        assert!(added.ends_with("\"c\": 3,\r\n  \"d\": {\r\n    \"x\": 1\r\n  }\r\n}"));
    }

    #[test]
    fn replaces_existing_value_in_place() {
        let original = "{\"a\":1,\"b\":{\"old\":true},\"c\":\"\\\"}\"}";
        let out = Doc::parse(original)
            .unwrap()
            .upsert(&["b"], &json!(5))
            .unwrap();
        assert_eq!(out, "{\"a\":1,\"b\":5,\"c\":\"\\\"}\"}");
    }

    #[test]
    fn refuses_non_objects_and_bad_json() {
        assert_eq!(Doc::parse("[1]").err(), Some(EditError::NotObject));
        assert_eq!(Doc::parse("{\"a\": 1,}").err(), Some(EditError::NotJson));
        assert_eq!(
            Doc::parse("{\"a\": 1} // note").err(),
            Some(EditError::NotJson)
        );
        let doc = Doc::parse("{\"providers\": 3}").unwrap();
        assert_eq!(
            doc.upsert(&["providers", "x"], &json!(1)).err(),
            Some(EditError::MemberNotObject("providers".into()))
        );
    }
}
