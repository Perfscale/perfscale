//! A jq *path* subset shared by `std/read-json@v1`, the `query` parameter of
//! `std/file-read@v1` / `std/file-write@v1`, and `${{ ... }}` interpolation.
//!
//! Grammar (not full jq — no pipes, filters, or functions):
//!
//! ```text
//! path    := "."? segment*
//! segment := ("." key | "[" quoted-key "]" | "[" index "]") "?"?
//! ```
//!
//! - A leading `.` is optional; the first segment may also be a bare key
//!   (`posts.0.id` ≡ `.posts.0.id`).
//! - A dot segment that is all digits indexes an array (`posts.0.id`), so dot
//!   and bracket indexing are interchangeable (`posts[0].id`).
//! - A `?` suffix on a segment makes it tolerant: a miss resolves to `null`
//!   instead of an error (get) — e.g. `.posts[0]?.id`.

use serde_json::Value;

/// One parsed path segment: an object key or a numeric array index, with an
/// optional tolerant-access marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Segment {
    kind: Kind,
    /// `?` suffix: a miss yields `null` instead of an error.
    optional: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Key(String),
    Index(usize),
}

impl std::fmt::Display for Segment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            Kind::Key(k) => write!(f, ".{k}"),
            Kind::Index(i) => write!(f, "[{i}]"),
        }
    }
}

/// Parse a path string into segments. Returns an error describing the first
/// malformed position. An empty path parses to zero segments (identity).
pub(crate) fn parse(query: &str) -> Result<Vec<Segment>, String> {
    let b = query.as_bytes();
    let mut pos = 0;
    let mut segments = Vec::new();
    let mut first = true;

    if b.first() == Some(&b'.') {
        pos = 1;
    }

    while pos < b.len() {
        let kind = match b[pos] {
            b'.' => {
                pos += 1;
                let start = pos;
                while pos < b.len() && !matches!(b[pos], b'.' | b'[' | b'?') {
                    pos += 1;
                }
                let key = &query[start..pos];
                if key.is_empty() {
                    return Err(format!(
                        "empty segment in path '{query}' at position {start}"
                    ));
                }
                dot_segment(key)
            }
            b'[' => {
                pos += 1;
                match b.get(pos) {
                    Some(&q @ (b'"' | b'\'')) => {
                        pos += 1;
                        let start = pos;
                        while pos < b.len() && b[pos] != q {
                            pos += 1;
                        }
                        if pos >= b.len() {
                            return Err(format!("unterminated string in path '{query}'"));
                        }
                        let key = &query[start..pos];
                        pos += 1; // closing quote
                        if b.get(pos) != Some(&b']') {
                            return Err(format!(
                                "expected ']' in path '{query}' at position {pos}"
                            ));
                        }
                        pos += 1;
                        Kind::Key(key.to_owned())
                    }
                    _ => {
                        let start = pos;
                        while pos < b.len() && b[pos].is_ascii_digit() {
                            pos += 1;
                        }
                        if b.get(pos) != Some(&b']') || pos == start {
                            return Err(format!(
                                "expected [index] or [\"key\"] in path '{query}' at position {start}"
                            ));
                        }
                        let index = query[start..pos]
                            .parse::<usize>()
                            .map_err(|_| format!("index too large in path '{query}'"))?;
                        pos += 1;
                        Kind::Index(index)
                    }
                }
            }
            _ if first => {
                // Bare first key: `posts.0.id` ≡ `.posts.0.id`.
                let start = pos;
                while pos < b.len() && !matches!(b[pos], b'.' | b'[' | b'?') {
                    pos += 1;
                }
                let key = &query[start..pos];
                if key.is_empty() {
                    return Err(format!(
                        "empty segment in path '{query}' at position {start}"
                    ));
                }
                dot_segment(key)
            }
            _ => {
                return Err(format!(
                    "unexpected character '{}' in path '{query}' at position {pos}",
                    b[pos] as char
                ))
            }
        };
        let optional = b.get(pos) == Some(&b'?');
        if optional {
            pos += 1;
        }
        segments.push(Segment { kind, optional });
        first = false;
    }
    Ok(segments)
}

/// A dot segment that is all digits indexes an array (`posts.0.id`).
fn dot_segment(key: &str) -> Kind {
    match key.parse::<usize>() {
        Ok(i) if key.bytes().all(|c| c.is_ascii_digit()) => Kind::Index(i),
        _ => Kind::Key(key.to_owned()),
    }
}

/// Follow one segment into `v`. A key reads an object field, an index reads
/// an array element; anything else (type mismatch, missing field, out of
/// bounds) is a miss.
fn descend<'a>(v: &'a Value, kind: &Kind) -> Option<&'a Value> {
    match (v, kind) {
        (Value::Object(map), Kind::Key(k)) => map.get(k),
        (Value::Array(items), Kind::Index(i)) => items.get(*i),
        _ => None,
    }
}

/// Read `segments` out of `v`.
///
/// - `Ok(Some(v))` — the value at the path.
/// - `Ok(None)` — a tolerant (`?`) segment missed; callers surface `null`.
/// - `Err` — a mandatory segment missed; names the failing segment.
pub(crate) fn get<'a>(mut v: &'a Value, segments: &[Segment]) -> Result<Option<&'a Value>, String> {
    for segment in segments {
        match descend(v, &segment.kind) {
            Some(next) => v = next,
            None if segment.optional => return Ok(None),
            None => return Err(format!("path segment '{segment}' not found")),
        }
    }
    Ok(Some(v))
}

/// Set `value` at `segments` in `root`, creating intermediate objects/arrays
/// like jq: a missing key grows an object, an index into `null` grows an
/// array. An array index must be within bounds or exactly `len` (append);
/// anything further out is an error. An empty path replaces the root.
pub(crate) fn set(root: &mut Value, segments: &[Segment], value: Value) -> Result<(), String> {
    let Some((head, rest)) = segments.split_first() else {
        *root = value;
        return Ok(());
    };
    match &head.kind {
        Kind::Key(k) => {
            if !root.is_object() {
                if root.is_null() {
                    *root = Value::Object(serde_json::Map::new());
                } else {
                    return Err(format!("cannot set '{head}': value is not an object"));
                }
            }
            let entry = root
                .as_object_mut()
                .expect("just ensured object")
                .entry(k.clone())
                .or_insert(Value::Null);
            set(entry, rest, value)
        }
        Kind::Index(i) => {
            if !root.is_array() {
                if root.is_null() {
                    *root = Value::Array(Vec::new());
                } else {
                    return Err(format!("cannot set '{head}': value is not an array"));
                }
            }
            let items = root.as_array_mut().expect("just ensured array");
            if *i > items.len() {
                return Err(format!(
                    "array index {i} out of bounds (len {}) — index {} appends",
                    items.len(),
                    items.len()
                ));
            }
            if *i == items.len() {
                if rest.is_empty() {
                    items.push(value);
                    return Ok(());
                }
                items.push(Value::Null);
            }
            set(&mut items[*i], rest, value)
        }
    }
}

/// Follow `segments` from `v`, tolerantly (every miss is `None`, for
/// interpolation where a bad path resolves to an empty string). Borrowed
/// while the path stays inside the stored value; a string leaf with segments
/// left is parsed as JSON on demand (an HTTP `body`) and the walk continues
/// in the parsed document.
pub(crate) fn walk<'a>(v: &'a Value, segments: &[Segment]) -> Option<std::borrow::Cow<'a, Value>> {
    let mut current = v;
    for (i, segment) in segments.iter().enumerate() {
        if let Value::String(s) = current {
            let parsed: Value = serde_json::from_str(s).ok()?;
            return walk(&parsed, &segments[i..]).map(|c| std::borrow::Cow::Owned(c.into_owned()));
        }
        current = descend(current, &segment.kind)?;
    }
    Some(std::borrow::Cow::Borrowed(current))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn kinds(query: &str) -> Vec<(Kind, bool)> {
        parse(query)
            .unwrap()
            .into_iter()
            .map(|s| (s.kind, s.optional))
            .collect()
    }

    #[test]
    fn parses_dot_and_bracket_syntax_equivalently() {
        assert_eq!(
            kinds(".posts[0].id"),
            vec![
                (Kind::Key("posts".into()), false),
                (Kind::Index(0), false),
                (Kind::Key("id".into()), false),
            ]
        );
        // `posts.0.id` ≡ `.posts[0].id`; quoted brackets ≡ dot keys.
        assert_eq!(kinds("posts.0.id"), kinds(".posts[0].id"));
        assert_eq!(kinds(r#".posts["0"]["id"]"#).len(), 3);
        assert_eq!(
            kinds(r#"["posts"].id"#),
            vec![
                (Kind::Key("posts".into()), false),
                (Kind::Key("id".into()), false),
            ]
        );
        // Tolerant segments.
        assert_eq!(kinds(".posts[0]?.id")[1], (Kind::Index(0), true));
        assert_eq!(kinds(".meta?.id")[0], (Kind::Key("meta".into()), true));
        // Empty path is identity.
        assert!(parse("").unwrap().is_empty());
        assert!(parse(".").unwrap().is_empty());
    }

    #[test]
    fn rejects_malformed_paths() {
        for bad in [
            ".posts..id",
            "[0",
            ".posts[",
            r#".posts["x]"#,
            ".posts[x]",
            ".posts[99999999999999999999]",
        ] {
            assert!(parse(bad).is_err(), "{bad} should fail");
        }
    }

    #[test]
    fn get_reads_objects_and_arrays() {
        let doc = json!({ "posts": [{ "id": 7 }, { "id": 9 }] });
        let segs = parse(".posts[1].id").unwrap();
        assert_eq!(get(&doc, &segs).unwrap(), Some(&json!(9)));
        let segs = parse("posts.0").unwrap();
        assert_eq!(get(&doc, &segs).unwrap(), Some(&json!({ "id": 7 })));
        let segs = parse("").unwrap();
        assert_eq!(get(&doc, &segs).unwrap(), Some(&doc));
    }

    #[test]
    fn get_names_the_failing_segment_and_honours_tolerant_suffix() {
        let doc = json!({ "posts": [] });
        let err = get(&doc, &parse(".posts[3].id").unwrap()).unwrap_err();
        assert!(err.contains("[3]"), "{err}");
        let err = get(&doc, &parse(".users[0]").unwrap()).unwrap_err();
        assert!(err.contains(".users"), "{err}");
        // Type mismatch is also a miss.
        assert!(get(&json!(42), &parse(".id").unwrap()).is_err());
        // Tolerant miss → Ok(None), both missing key and out of bounds.
        assert_eq!(get(&doc, &parse(".users?[0]").unwrap()).unwrap(), None);
        assert_eq!(get(&doc, &parse(".posts[3]?.id").unwrap()).unwrap(), None);
        // A tolerant segment that hits keeps walking.
        assert_eq!(
            get(&doc, &parse(".posts?").unwrap()).unwrap(),
            Some(&json!([]))
        );
    }

    #[test]
    fn set_creates_intermediates_and_appends() {
        let mut doc = Value::Null;
        set(&mut doc, &parse(".a.b[0].c").unwrap(), json!(1)).unwrap();
        assert_eq!(doc, json!({ "a": { "b": [{ "c": 1 }] } }));
        // Append at len.
        set(&mut doc, &parse(".a.b[1]").unwrap(), json!(2)).unwrap();
        assert_eq!(doc, json!({ "a": { "b": [{ "c": 1 }, 2] } }));
        // Overwrite in place.
        set(&mut doc, &parse(".a.b[0].c").unwrap(), json!(3)).unwrap();
        assert_eq!(doc["a"]["b"][0]["c"], json!(3));
        // Empty path replaces the root.
        set(&mut doc, &parse("").unwrap(), json!([1])).unwrap();
        assert_eq!(doc, json!([1]));
    }

    #[test]
    fn set_rejects_out_of_bounds_and_type_mismatches() {
        let mut doc = json!({ "a": [1] });
        let err = set(&mut doc, &parse(".a[3]").unwrap(), json!(2)).unwrap_err();
        assert!(err.contains("out of bounds"), "{err}");
        let err = set(&mut doc, &parse(".a.b").unwrap(), json!(2)).unwrap_err();
        assert!(err.contains("not an object"), "{err}");
        let err = set(&mut doc, &parse(".a[0][1]").unwrap(), json!(2)).unwrap_err();
        assert!(err.contains("not an array"), "{err}");
    }

    #[test]
    fn walk_parses_string_leaves_on_demand() {
        let feed = json!({ "status": 200, "body": r#"{"posts":[{"id":7}]}"# });
        let segs = parse(".body.posts[0].id").unwrap();
        assert_eq!(walk(&feed, &segs).map(|c| c.into_owned()), Some(json!(7)));
        // A non-JSON string leaf, out-of-range index, or missing key → None.
        let text = json!({ "body": "plain" });
        assert!(walk(&text, &parse(".body.x").unwrap()).is_none());
        assert!(walk(&feed, &parse(".body.posts[5]").unwrap()).is_none());
        assert!(walk(&feed, &parse(".body.nope").unwrap()).is_none());
        // The string itself is returned when the path ends on it.
        assert_eq!(
            walk(&text, &parse(".body").unwrap()).map(|c| c.into_owned()),
            Some(json!("plain"))
        );
    }
}
