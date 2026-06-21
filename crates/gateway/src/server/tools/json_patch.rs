// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Minimal RFC 6902 JSON Patch over RFC 6901 JSON Pointers.
//!
//! Hand-rolled (no crate dependency) so the typst edit tool can apply
//! a small model-supplied patch to a previously-rendered document's
//! data — "change slide 3's title and nothing else" — instead of the
//! model resending the whole input. Supports the full op set
//! (`add`/`remove`/`replace`/`move`/`copy`/`test`); arrays are indexed
//! by RFC 6901 numeric tokens with `-` for append.

use serde_json::Value;

/// A patch that could not be applied (bad pointer, missing target,
/// failed `test`, …). Carries a human-readable reason that the tool
/// surfaces to the model so it can correct the patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchError(pub String);

impl std::fmt::Display for PatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn err(msg: impl Into<String>) -> PatchError {
    PatchError(msg.into())
}

/// Apply an RFC 6902 patch (an array of operation objects) to `doc` in
/// place. Operations run in order; the first failure aborts and leaves
/// `doc` partially mutated (callers patch a throwaway clone, so this is
/// fine — they discard it on error).
pub fn apply(doc: &mut Value, patch: &[Value]) -> Result<(), PatchError> {
    for (i, op) in patch.iter().enumerate() {
        apply_one(doc, op).map_err(|e| err(format!("patch op {i}: {e}")))?;
    }
    Ok(())
}

fn apply_one(doc: &mut Value, op: &Value) -> Result<(), PatchError> {
    let obj = op
        .as_object()
        .ok_or_else(|| err("operation must be a JSON object"))?;
    let kind = obj
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| err("operation is missing a string `op`"))?;
    let path = obj
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| err("operation is missing a string `path`"))?;
    match kind {
        "add" => add(doc, path, req_value(obj)?.clone()),
        "replace" => replace(doc, path, req_value(obj)?.clone()),
        "remove" => remove(doc, path).map(|_| ()),
        "test" => test(doc, path, req_value(obj)?),
        "move" => mov(doc, req_from(obj)?, path),
        "copy" => copy(doc, req_from(obj)?, path),
        other => Err(err(format!("unsupported op `{other}`"))),
    }
}

fn req_value(obj: &serde_json::Map<String, Value>) -> Result<&Value, PatchError> {
    obj.get("value")
        .ok_or_else(|| err("operation requires a `value`"))
}

fn req_from(obj: &serde_json::Map<String, Value>) -> Result<&str, PatchError> {
    obj.get("from")
        .and_then(Value::as_str)
        .ok_or_else(|| err("operation requires a string `from`"))
}

/// Parse an RFC 6901 JSON Pointer into its reference tokens, undoing
/// the `~1`→`/` and `~0`→`~` escapes. The empty string is the root
/// (no tokens).
fn parse_pointer(path: &str) -> Result<Vec<String>, PatchError> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    if !path.starts_with('/') {
        return Err(err(format!(
            "pointer `{path}` must start with `/` (or be empty for the document root)"
        )));
    }
    Ok(path[1..]
        // `~1` before `~0` per RFC 6901, else `~01` would mis-decode.
        .split('/')
        .map(|t| t.replace("~1", "/").replace("~0", "~"))
        .collect())
}

/// Resolve a token against an array length into a real index. `-` is
/// only meaningful to `add` (append); every other caller passes
/// `allow_dash = false`.
fn array_index(token: &str, len: usize, allow_dash: bool) -> Result<usize, PatchError> {
    if token == "-" {
        return if allow_dash {
            Ok(len)
        } else {
            Err(err("array index `-` is only valid for `add`"))
        };
    }
    let i: usize = token
        .parse()
        .map_err(|_| err(format!("`{token}` is not a valid array index")))?;
    Ok(i)
}

/// Walk every token except the last, returning a mutable handle to the
/// container that holds the final token.
fn parent_mut<'a>(doc: &'a mut Value, tokens: &[String]) -> Result<&'a mut Value, PatchError> {
    let mut cur = doc;
    for t in &tokens[..tokens.len() - 1] {
        cur = match cur {
            Value::Object(m) => m
                .get_mut(t)
                .ok_or_else(|| err(format!("missing object key `{t}`")))?,
            Value::Array(a) => {
                let len = a.len();
                let i = array_index(t, len, false)?;
                a.get_mut(i)
                    .ok_or_else(|| err(format!("array index {i} out of bounds (len {len})")))?
            }
            _ => return Err(err(format!("cannot descend into scalar at `{t}`"))),
        };
    }
    Ok(cur)
}

fn get<'a>(doc: &'a Value, path: &str) -> Result<&'a Value, PatchError> {
    let tokens = parse_pointer(path)?;
    let mut cur = doc;
    for t in &tokens {
        cur = match cur {
            Value::Object(m) => m
                .get(t)
                .ok_or_else(|| err(format!("missing object key `{t}`")))?,
            Value::Array(a) => {
                let i = array_index(t, a.len(), false)?;
                a.get(i)
                    .ok_or_else(|| err(format!("array index {i} out of bounds")))?
            }
            _ => return Err(err(format!("cannot descend into scalar at `{t}`"))),
        };
    }
    Ok(cur)
}

fn add(doc: &mut Value, path: &str, value: Value) -> Result<(), PatchError> {
    let tokens = parse_pointer(path)?;
    if tokens.is_empty() {
        *doc = value;
        return Ok(());
    }
    let last = tokens.last().unwrap().clone();
    let parent = parent_mut(doc, &tokens)?;
    match parent {
        Value::Object(m) => {
            m.insert(last, value);
            Ok(())
        }
        Value::Array(a) => {
            let i = array_index(&last, a.len(), true)?;
            if i > a.len() {
                return Err(err(format!(
                    "array insert index {i} out of bounds (len {})",
                    a.len()
                )));
            }
            a.insert(i, value);
            Ok(())
        }
        _ => Err(err("cannot add into a scalar")),
    }
}

fn replace(doc: &mut Value, path: &str, value: Value) -> Result<(), PatchError> {
    let tokens = parse_pointer(path)?;
    if tokens.is_empty() {
        *doc = value;
        return Ok(());
    }
    let last = tokens.last().unwrap().clone();
    let parent = parent_mut(doc, &tokens)?;
    match parent {
        Value::Object(m) => {
            if !m.contains_key(&last) {
                return Err(err(format!("cannot replace missing key `{last}`")));
            }
            m.insert(last, value);
            Ok(())
        }
        Value::Array(a) => {
            let len = a.len();
            let i = array_index(&last, len, false)?;
            let slot = a
                .get_mut(i)
                .ok_or_else(|| err(format!("array index {i} out of bounds (len {len})")))?;
            *slot = value;
            Ok(())
        }
        _ => Err(err("cannot replace inside a scalar")),
    }
}

fn remove(doc: &mut Value, path: &str) -> Result<Value, PatchError> {
    let tokens = parse_pointer(path)?;
    if tokens.is_empty() {
        return Err(err("cannot remove the document root"));
    }
    let last = tokens.last().unwrap().clone();
    let parent = parent_mut(doc, &tokens)?;
    match parent {
        Value::Object(m) => m
            .remove(&last)
            .ok_or_else(|| err(format!("cannot remove missing key `{last}`"))),
        Value::Array(a) => {
            let len = a.len();
            let i = array_index(&last, len, false)?;
            if i >= len {
                return Err(err(format!("array index {i} out of bounds (len {len})")));
            }
            Ok(a.remove(i))
        }
        _ => Err(err("cannot remove inside a scalar")),
    }
}

fn test(doc: &Value, path: &str, expected: &Value) -> Result<(), PatchError> {
    let actual = get(doc, path)?;
    if actual == expected {
        Ok(())
    } else {
        Err(err(format!("test failed at `{path}`: value mismatch")))
    }
}

fn mov(doc: &mut Value, from: &str, path: &str) -> Result<(), PatchError> {
    // RFC 6902: `from` must not be a proper prefix of `path` (you can't
    // move a node into its own subtree).
    if is_prefix(&parse_pointer(from)?, &parse_pointer(path)?) {
        return Err(err("`move` cannot relocate a node into its own child"));
    }
    let v = remove(doc, from)?;
    add(doc, path, v)
}

fn copy(doc: &mut Value, from: &str, path: &str) -> Result<(), PatchError> {
    let v = get(doc, from)?.clone();
    add(doc, path, v)
}

/// True when `a` is a prefix of (or equal to) `b` token-wise.
fn is_prefix(a: &[String], b: &[String]) -> bool {
    a.len() <= b.len() && a.iter().zip(b).all(|(x, y)| x == y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn deck() -> Value {
        json!({
            "deck_title": "Old",
            "slides": [
                {"layout": "cover", "title": "First"},
                {"layout": "content", "title": "Second"}
            ]
        })
    }

    #[test]
    fn replace_one_nested_value_leaves_rest_intact() {
        let mut d = deck();
        apply(
            &mut d,
            &[json!({"op": "replace", "path": "/slides/1/title", "value": "New headline"})],
        )
        .unwrap();
        assert_eq!(d["slides"][1]["title"], "New headline");
        // Everything else is untouched.
        assert_eq!(d["slides"][0]["title"], "First");
        assert_eq!(d["deck_title"], "Old");
    }

    #[test]
    fn replace_missing_key_errors() {
        let mut d = deck();
        let e = apply(
            &mut d,
            &[json!({"op": "replace", "path": "/nope", "value": 1})],
        )
        .unwrap_err();
        assert!(e.to_string().contains("missing key"), "{e}");
    }

    #[test]
    fn add_appends_to_array_with_dash() {
        let mut d = deck();
        apply(
            &mut d,
            &[json!({"op": "add", "path": "/slides/-", "value": {"layout": "closing"}})],
        )
        .unwrap();
        assert_eq!(d["slides"].as_array().unwrap().len(), 3);
        assert_eq!(d["slides"][2]["layout"], "closing");
    }

    #[test]
    fn add_inserts_at_index() {
        let mut d = deck();
        apply(
            &mut d,
            &[json!({"op": "add", "path": "/slides/1", "value": {"layout": "section"}})],
        )
        .unwrap();
        assert_eq!(d["slides"][1]["layout"], "section");
        assert_eq!(d["slides"][2]["title"], "Second");
    }

    #[test]
    fn remove_drops_an_element() {
        let mut d = deck();
        apply(&mut d, &[json!({"op": "remove", "path": "/slides/0"})]).unwrap();
        assert_eq!(d["slides"].as_array().unwrap().len(), 1);
        assert_eq!(d["slides"][0]["title"], "Second");
    }

    #[test]
    fn add_new_object_key() {
        let mut d = deck();
        apply(
            &mut d,
            &[json!({"op": "add", "path": "/theme", "value": "light"})],
        )
        .unwrap();
        assert_eq!(d["theme"], "light");
    }

    #[test]
    fn test_op_gates_application() {
        let mut d = deck();
        // Passing test → ok.
        apply(
            &mut d,
            &[json!({"op": "test", "path": "/deck_title", "value": "Old"})],
        )
        .unwrap();
        // Failing test → error, no mutation.
        let e = apply(
            &mut d,
            &[json!({"op": "test", "path": "/deck_title", "value": "Wrong"})],
        )
        .unwrap_err();
        assert!(e.to_string().contains("test failed"), "{e}");
    }

    #[test]
    fn move_and_copy() {
        let mut d = json!({"a": {"x": 1}, "b": {}});
        apply(
            &mut d,
            &[json!({"op": "move", "from": "/a/x", "path": "/b/y"})],
        )
        .unwrap();
        assert_eq!(d["b"]["y"], 1);
        assert!(d["a"].get("x").is_none());
        apply(
            &mut d,
            &[json!({"op": "copy", "from": "/b/y", "path": "/b/z"})],
        )
        .unwrap();
        assert_eq!(d["b"]["z"], 1);
        assert_eq!(d["b"]["y"], 1);
    }

    #[test]
    fn move_into_own_subtree_rejected() {
        let mut d = json!({"a": {"b": 1}});
        let e = apply(
            &mut d,
            &[json!({"op": "move", "from": "/a", "path": "/a/c"})],
        )
        .unwrap_err();
        assert!(e.to_string().contains("own child"), "{e}");
    }

    #[test]
    fn pointer_escapes_decoded() {
        let mut d = json!({"a/b": 1, "c~d": 2});
        apply(
            &mut d,
            &[
                json!({"op": "replace", "path": "/a~1b", "value": 10}),
                json!({"op": "replace", "path": "/c~0d", "value": 20}),
            ],
        )
        .unwrap();
        assert_eq!(d["a/b"], 10);
        assert_eq!(d["c~d"], 20);
    }

    #[test]
    fn bad_pointer_shape_errors() {
        let mut d = deck();
        let e = apply(
            &mut d,
            &[json!({"op": "replace", "path": "no-leading-slash", "value": 1})],
        )
        .unwrap_err();
        assert!(e.to_string().contains("must start with"), "{e}");
    }

    #[test]
    fn array_index_out_of_bounds_errors() {
        let mut d = deck();
        let e = apply(
            &mut d,
            &[json!({"op": "replace", "path": "/slides/9/title", "value": "x"})],
        )
        .unwrap_err();
        assert!(e.to_string().contains("out of bounds"), "{e}");
    }

    #[test]
    fn replace_whole_root() {
        let mut d = deck();
        apply(
            &mut d,
            &[json!({"op": "replace", "path": "", "value": {"fresh": true}})],
        )
        .unwrap();
        assert_eq!(d, json!({"fresh": true}));
    }
}
