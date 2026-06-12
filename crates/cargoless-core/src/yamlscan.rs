//! Hand-rolled YAML-subset scanner shared by the manifest parsers
//! (`cargoless.checks.yaml` in [`crate::project_checks`], `cargoless.app.yaml`
//! in `appmanifest`, and the app-serve instances file in `appinstances`).
//!
//! Moved **verbatim** out of `project_checks` (which carried it privately
//! since project-checks v2) so the app-serve tier can parse its manifests
//! with the *same* subset and the same line-attributed errors — without
//! adding a YAML dependency (the no-external-deps discipline) and without
//! `project_checks` becoming a grab-bag import target. The existing
//! `project_checks` manifest tests guard that behavior did not change.
//!
//! ## The subset (deliberately small)
//!
//! - Block maps and block lists, two-space indentation, no tabs.
//! - Inline scalar lists `[a, b]`; **no inline `{...}` maps** — write block
//!   form.
//! - `# comments`, single/double-quoted scalars, `true`/`false`/`null`,
//!   integers.
//! - **Rejected loudly:** anchors, aliases, multiline scalars (`|` / `>`),
//!   tabs, odd indentation.
//!
//! Everything here is `pub(crate)`: the scanner is an internal seam, not
//! public API. Manifest modules expose typed configs, never `YamlNode`.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParseError {
    pub(crate) line: usize,
    pub(crate) message: String,
}

pub(crate) fn reject_unknown(
    map: &BTreeMap<String, YamlNode>,
    allowed: &[&str],
    line: usize,
) -> Result<(), ParseError> {
    for key in map.keys() {
        if !allowed.iter().any(|a| a == key) {
            return Err(ParseError {
                line,
                message: format!("unknown key `{key}`"),
            });
        }
    }
    Ok(())
}

pub(crate) fn required_string(
    map: &BTreeMap<String, YamlNode>,
    key: &str,
    line: usize,
) -> Result<String, ParseError> {
    get_string(map, key)?.ok_or(ParseError {
        line,
        message: format!("required key `{key}` is missing"),
    })
}

pub(crate) fn get_string(
    map: &BTreeMap<String, YamlNode>,
    key: &str,
) -> Result<Option<String>, ParseError> {
    map.get(key).map(YamlNode::expect_string).transpose()
}

pub(crate) fn get_bool(
    map: &BTreeMap<String, YamlNode>,
    key: &str,
) -> Result<Option<bool>, ParseError> {
    map.get(key).map(YamlNode::expect_bool).transpose()
}

pub(crate) fn get_u64(
    map: &BTreeMap<String, YamlNode>,
    key: &str,
) -> Result<Option<u64>, ParseError> {
    map.get(key)
        .map(|v| v.expect_int().map(|i| i.max(0) as u64))
        .transpose()
}

pub(crate) fn get_string_list(
    map: &BTreeMap<String, YamlNode>,
    key: &str,
) -> Result<Option<Vec<String>>, ParseError> {
    map.get(key).map(YamlNode::expect_string_list).transpose()
}

/// Validate a short identifier that ends up in directory names, state files,
/// and telemetry attributes: `[A-Za-z0-9_-]+`, nothing else.
pub(crate) fn check_label(s: &str, what: &str, line: usize) -> Result<(), ParseError> {
    let ok = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        return Err(ParseError {
            line,
            message: format!("{what} must match [A-Za-z0-9_-]+, got `{s}`"),
        });
    }
    Ok(())
}

/// Validate an environment-variable *name* (not value): non-empty, no `=`,
/// no whitespace — anything else would silently corrupt the child's env.
pub(crate) fn check_env_key(s: &str, what: &str, line: usize) -> Result<(), ParseError> {
    if s.is_empty() || s.contains('=') || s.chars().any(char::is_whitespace) {
        return Err(ParseError {
            line,
            message: format!("{what} must be a valid environment variable name, got `{s}`"),
        });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum YamlNode {
    Map(BTreeMap<String, YamlNode>, usize),
    List(Vec<YamlNode>, usize),
    String(String, usize),
    Bool(bool, usize),
    Int(i64, usize),
    Null(usize),
}

impl YamlNode {
    pub(crate) fn line(&self) -> usize {
        match self {
            Self::Map(_, l)
            | Self::List(_, l)
            | Self::String(_, l)
            | Self::Bool(_, l)
            | Self::Int(_, l)
            | Self::Null(l) => *l,
        }
    }

    pub(crate) fn as_map(&self) -> Option<&BTreeMap<String, YamlNode>> {
        match self {
            Self::Map(v, _) => Some(v),
            _ => None,
        }
    }

    pub(crate) fn expect_map(&self, name: &str) -> Result<&BTreeMap<String, YamlNode>, ParseError> {
        self.as_map().ok_or(ParseError {
            line: self.line(),
            message: format!("{name} must be a map"),
        })
    }

    pub(crate) fn expect_list(&self, name: &str) -> Result<&[YamlNode], ParseError> {
        match self {
            Self::List(v, _) => Ok(v),
            _ => Err(ParseError {
                line: self.line(),
                message: format!("{name} must be a list"),
            }),
        }
    }

    pub(crate) fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int(v, _) => Some(*v),
            _ => None,
        }
    }

    pub(crate) fn expect_string(&self) -> Result<String, ParseError> {
        match self {
            Self::String(v, _) => Ok(v.clone()),
            Self::Int(v, _) => Ok(v.to_string()),
            Self::Bool(v, _) => Ok(v.to_string()),
            _ => Err(ParseError {
                line: self.line(),
                message: "expected string scalar".to_string(),
            }),
        }
    }

    pub(crate) fn expect_bool(&self) -> Result<bool, ParseError> {
        match self {
            Self::Bool(v, _) => Ok(*v),
            _ => Err(ParseError {
                line: self.line(),
                message: "expected boolean scalar".to_string(),
            }),
        }
    }

    pub(crate) fn expect_int(&self) -> Result<i64, ParseError> {
        match self {
            Self::Int(v, _) => Ok(*v),
            _ => Err(ParseError {
                line: self.line(),
                message: "expected integer scalar".to_string(),
            }),
        }
    }

    pub(crate) fn expect_string_list(&self) -> Result<Vec<String>, ParseError> {
        match self {
            Self::List(items, _) => items.iter().map(YamlNode::expect_string).collect(),
            Self::String(v, _) => Ok(vec![v.clone()]),
            _ => Err(ParseError {
                line: self.line(),
                message: "expected string list".to_string(),
            }),
        }
    }

    pub(crate) fn value_at_path(&self, path: &str) -> Option<&YamlNode> {
        let mut cur = self;
        for part in path.trim_start_matches("$.").split('.') {
            if part.is_empty() {
                continue;
            }
            let map = cur.as_map()?;
            cur = map.get(part)?;
        }
        Some(cur)
    }

    pub(crate) fn scalar_string(&self) -> String {
        match self {
            Self::String(v, _) => v.clone(),
            Self::Bool(v, _) => v.to_string(),
            Self::Int(v, _) => v.to_string(),
            Self::Null(_) => "null".to_string(),
            Self::Map(_, _) => "<map>".to_string(),
            Self::List(_, _) => "<list>".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
struct YamlLine {
    indent: usize,
    text: String,
    line_no: usize,
}

pub(crate) fn parse_yaml_value(text: &str) -> Result<YamlNode, ParseError> {
    let mut lines = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        if raw.contains('\t') {
            return Err(ParseError {
                line: idx + 1,
                message: "tabs are not supported in project-check YAML".to_string(),
            });
        }
        let no_comment = strip_yaml_comment(raw);
        if no_comment.trim().is_empty() {
            continue;
        }
        let indent = no_comment.chars().take_while(|c| *c == ' ').count();
        if indent % 2 != 0 {
            return Err(ParseError {
                line: idx + 1,
                message: "indentation must use multiples of two spaces".to_string(),
            });
        }
        lines.push(YamlLine {
            indent,
            text: no_comment.trim().to_string(),
            line_no: idx + 1,
        });
    }
    if lines.is_empty() {
        return Ok(YamlNode::Null(1));
    }
    let (node, idx) = parse_block(&lines, 0, lines[0].indent)?;
    if idx != lines.len() {
        return Err(ParseError {
            line: lines[idx].line_no,
            message: "unexpected extra YAML content".to_string(),
        });
    }
    Ok(node)
}

fn parse_block(
    lines: &[YamlLine],
    idx: usize,
    indent: usize,
) -> Result<(YamlNode, usize), ParseError> {
    if idx >= lines.len() {
        return Ok((YamlNode::Null(1), idx));
    }
    if lines[idx].indent < indent {
        return Ok((YamlNode::Null(lines[idx].line_no), idx));
    }
    if lines[idx].indent > indent {
        return Err(ParseError {
            line: lines[idx].line_no,
            message: "unexpected indentation".to_string(),
        });
    }
    if lines[idx].text.starts_with("- ") || lines[idx].text == "-" {
        parse_list(lines, idx, indent)
    } else {
        parse_map(lines, idx, indent)
    }
}

fn parse_map(
    lines: &[YamlLine],
    mut idx: usize,
    indent: usize,
) -> Result<(YamlNode, usize), ParseError> {
    let line_no = lines[idx].line_no;
    let mut map = BTreeMap::new();
    while idx < lines.len() {
        let line = &lines[idx];
        if line.indent < indent {
            break;
        }
        if line.indent > indent {
            return Err(ParseError {
                line: line.line_no,
                message: "unexpected indentation in map".to_string(),
            });
        }
        if line.text.starts_with("- ") || line.text == "-" {
            break;
        }
        let (key, val) = split_key_value(&line.text).ok_or(ParseError {
            line: line.line_no,
            message: "expected `key: value`".to_string(),
        })?;
        idx += 1;
        let node = if val.is_empty() {
            if idx < lines.len() && lines[idx].indent > indent {
                let (child, next) = parse_block(lines, idx, indent + 2)?;
                idx = next;
                child
            } else {
                YamlNode::Null(line.line_no)
            }
        } else {
            parse_scalar(val, line.line_no)?
        };
        map.insert(key.to_string(), node);
    }
    Ok((YamlNode::Map(map, line_no), idx))
}

fn parse_list(
    lines: &[YamlLine],
    mut idx: usize,
    indent: usize,
) -> Result<(YamlNode, usize), ParseError> {
    let line_no = lines[idx].line_no;
    let mut out = Vec::new();
    while idx < lines.len() {
        let line = &lines[idx];
        if line.indent < indent {
            break;
        }
        if line.indent > indent {
            return Err(ParseError {
                line: line.line_no,
                message: "unexpected indentation in list".to_string(),
            });
        }
        if !(line.text.starts_with("- ") || line.text == "-") {
            break;
        }
        let rest = line.text.strip_prefix("-").unwrap().trim();
        idx += 1;
        if rest.is_empty() {
            let (child, next) = parse_block(lines, idx, indent + 2)?;
            idx = next;
            out.push(child);
        } else if let Some((key, val)) = split_key_value(rest) {
            let mut map = BTreeMap::new();
            let first = if val.is_empty() {
                if idx < lines.len() && lines[idx].indent > indent {
                    let (child, next) = parse_block(lines, idx, indent + 2)?;
                    idx = next;
                    child
                } else {
                    YamlNode::Null(line.line_no)
                }
            } else {
                parse_scalar(val, line.line_no)?
            };
            map.insert(key.to_string(), first);
            while idx < lines.len() && lines[idx].indent == indent + 2 {
                if lines[idx].text.starts_with("- ") || lines[idx].text == "-" {
                    break;
                }
                let (k, v) = split_key_value(&lines[idx].text).ok_or(ParseError {
                    line: lines[idx].line_no,
                    message: "expected map entry".to_string(),
                })?;
                let item_line = lines[idx].line_no;
                idx += 1;
                let node = if v.is_empty() {
                    if idx < lines.len() && lines[idx].indent > indent + 2 {
                        let (child, next) = parse_block(lines, idx, indent + 4)?;
                        idx = next;
                        child
                    } else {
                        YamlNode::Null(item_line)
                    }
                } else {
                    parse_scalar(v, item_line)?
                };
                map.insert(k.to_string(), node);
            }
            out.push(YamlNode::Map(map, line.line_no));
        } else {
            out.push(parse_scalar(rest, line.line_no)?);
        }
    }
    Ok((YamlNode::List(out, line_no), idx))
}

fn split_key_value(s: &str) -> Option<(&str, &str)> {
    let mut in_single = false;
    let mut in_double = false;
    for (idx, ch) in s.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ':' if !in_single && !in_double => {
                let key = s[..idx].trim();
                let value = s[idx + 1..].trim();
                if key.is_empty() {
                    return None;
                }
                return Some((key, value));
            }
            _ => {}
        }
    }
    None
}

fn parse_scalar(s: &str, line: usize) -> Result<YamlNode, ParseError> {
    if s == "true" {
        return Ok(YamlNode::Bool(true, line));
    }
    if s == "false" {
        return Ok(YamlNode::Bool(false, line));
    }
    if s == "null" || s == "~" {
        return Ok(YamlNode::Null(line));
    }
    if s.starts_with('|') || s.starts_with('>') || s.starts_with('&') || s.starts_with('*') {
        return Err(ParseError {
            line,
            message: "YAML anchors, aliases, and multiline scalars are not supported".to_string(),
        });
    }
    if let Some(inner) = s.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
        let mut items = Vec::new();
        if !inner.trim().is_empty() {
            for item in split_inline_list(inner) {
                items.push(parse_scalar(item.trim(), line)?);
            }
        }
        return Ok(YamlNode::List(items, line));
    }
    if let Ok(v) = s.parse::<i64>() {
        return Ok(YamlNode::Int(v, line));
    }
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        return Ok(YamlNode::String(unquote_yaml(s), line));
    }
    Ok(YamlNode::String(s.to_string(), line))
}

fn strip_yaml_comment(s: &str) -> String {
    let mut in_single = false;
    let mut in_double = false;
    let mut out = String::new();
    for ch in s.chars() {
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
                out.push(ch);
            }
            '"' if !in_single => {
                in_double = !in_double;
                out.push(ch);
            }
            '#' if !in_single && !in_double => break,
            _ => out.push(ch),
        }
    }
    out
}

fn split_inline_list(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    for (idx, ch) in s.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ',' if !in_single && !in_double => {
                out.push(&s[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

fn unquote_yaml(s: &str) -> String {
    let inner = &s[1..s.len() - 1];
    if s.starts_with('"') {
        inner
            .replace(r#"\""#, r#"""#)
            .replace(r"\n", "\n")
            .replace(r"\t", "\t")
            .replace(r"\\", "\\")
    } else {
        inner.replace("''", "'")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The subset's *rejections* are part of its contract: each forbidden
    // construct must fail loudly with the offending line, never parse to
    // something silently different. (Acceptance of the subset itself is
    // exercised end-to-end by the `project_checks` manifest tests.)

    #[test]
    fn tabs_and_odd_indent_are_rejected_with_line() {
        let tab = parse_yaml_value("key:\n\tnested: 1").unwrap_err();
        assert_eq!(tab.line, 2);
        assert!(tab.message.contains("tabs"));

        let odd = parse_yaml_value("key:\n   nested: 1").unwrap_err();
        assert_eq!(odd.line, 2);
        assert!(odd.message.contains("two spaces"));
    }

    #[test]
    fn anchors_aliases_and_multiline_scalars_are_rejected() {
        for text in ["a: &anchor 1", "a: *alias", "a: |", "a: >"] {
            let err = parse_yaml_value(text).unwrap_err();
            assert!(
                err.message.contains("not supported"),
                "{text:?} must be rejected, got: {}",
                err.message
            );
        }
    }

    #[test]
    fn comments_and_quotes_round_trip() {
        let text = "plain: a value # comment\nhash: \"a # not-comment\"\nsq: 'it''s'\n";
        let root = parse_yaml_value(text).unwrap();
        let map = root.as_map().unwrap();
        assert_eq!(map["plain"].scalar_string(), "a value");
        assert_eq!(map["hash"].scalar_string(), "a # not-comment");
        assert_eq!(map["sq"].scalar_string(), "it's");
    }

    #[test]
    fn inline_list_and_typed_scalars() {
        let text = "list: [a, \"b,c\", 3]\nflag: true\nnone: ~\nnum: 42\n";
        let root = parse_yaml_value(text).unwrap();
        let map = root.as_map().unwrap();
        assert_eq!(
            map["list"].expect_string_list().unwrap(),
            vec!["a", "b,c", "3"]
        );
        assert!(map["flag"].expect_bool().unwrap());
        assert!(matches!(map["none"], YamlNode::Null(_)));
        assert_eq!(map["num"].as_i64(), Some(42));
    }

    #[test]
    fn list_of_maps_with_nested_blocks() {
        let text = "items:\n  - id: one\n    child:\n      deep: 1\n  - id: two\n";
        let root = parse_yaml_value(text).unwrap();
        let items = root.as_map().unwrap()["items"]
            .expect_list("items")
            .unwrap();
        assert_eq!(items.len(), 2);
        let deep = items[0].value_at_path("$.child.deep").unwrap();
        assert_eq!(deep.as_i64(), Some(1));
        let id = items[1].value_at_path("$.id").unwrap();
        assert_eq!(id.scalar_string(), "two");
    }

    #[test]
    fn empty_document_is_null_and_trailing_content_is_rejected() {
        assert!(matches!(
            parse_yaml_value("# only comments\n").unwrap(),
            YamlNode::Null(1)
        ));
        let err = parse_yaml_value("a: 1\n  b: 2\n").unwrap_err();
        assert_eq!(err.line, 2);
    }
}
