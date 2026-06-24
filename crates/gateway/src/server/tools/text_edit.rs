// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Anchored find/replace edits over free text — the text-format half of
//! `edit_document` (the structured half reuses [`super::json_patch`]).
//!
//! Mirrors the semantics of a code editor's surgical edit: the model
//! supplies a `find` snippet that must occur **exactly once** in the
//! document and the `new` text to swap in. Requiring a unique match is
//! what makes this safe to apply blindly — an ambiguous anchor (0 or >1
//! matches) is rejected with a count so the model can lengthen the anchor
//! instead of silently editing the wrong passage. An empty/absent `find`
//! means "append `new` to the end", the one growth case that needs no
//! anchor.
//!
//! Edits in a batch apply in order, each against the result of the last,
//! so the model can make several related changes in one call.

/// One find/replace edit. `find` empty ⇒ append `replace` to the end.
#[derive(Debug, Clone)]
pub struct Edit {
    pub find: String,
    pub replace: String,
}

/// An edit that could not be applied. Carries a human-readable reason the
/// `edit_document` tool surfaces to the model so it can correct the edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditError(pub String);

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Apply `edits` to `content` in order, returning the new content. The
/// first failing edit aborts the whole batch (the caller keeps the
/// original on error, so partial application is never observed).
pub fn apply_edits(content: &str, edits: &[Edit]) -> Result<String, EditError> {
    let mut out = content.to_string();
    for (i, edit) in edits.iter().enumerate() {
        out = apply_one(&out, edit).map_err(|e| EditError(format!("edit {i}: {e}")))?;
    }
    Ok(out)
}

fn apply_one(content: &str, edit: &Edit) -> Result<String, EditError> {
    // Empty anchor → append (with a newline separator unless the document
    // is empty or already ends in one).
    if edit.find.is_empty() {
        if content.is_empty() {
            return Ok(edit.replace.clone());
        }
        let sep = if content.ends_with('\n') { "" } else { "\n" };
        return Ok(format!("{content}{sep}{}", edit.replace));
    }

    let count = content.matches(&edit.find).count();
    match count {
        1 => Ok(content.replacen(&edit.find, &edit.replace, 1)),
        0 => Err(EditError(format!(
            "`find` text was not found in the document. It must match \
             exactly (including whitespace and punctuation). Re-read the \
             document and copy the snippet verbatim. Anchor was: {:?}",
            truncate(&edit.find, 120)
        ))),
        n => Err(EditError(format!(
            "`find` text matched {n} places — it must be unique. Include \
             more surrounding context so the anchor matches exactly one \
             passage. Anchor was: {:?}",
            truncate(&edit.find, 120)
        ))),
    }
}

/// First `max` chars of `s`, char-boundary safe, with an ellipsis when
/// clipped — for readable error messages.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(find: &str, replace: &str) -> Edit {
        Edit {
            find: find.to_string(),
            replace: replace.to_string(),
        }
    }

    #[test]
    fn unique_match_replaces() {
        let out = apply_edits("hello world", &[edit("world", "there")]).unwrap();
        assert_eq!(out, "hello there");
    }

    #[test]
    fn no_match_errors() {
        let e = apply_edits("hello", &[edit("xyz", "q")]).unwrap_err();
        assert!(e.to_string().contains("was not found"), "{e}");
    }

    #[test]
    fn ambiguous_match_errors_with_count() {
        let e = apply_edits("a a a", &[edit("a", "b")]).unwrap_err();
        assert!(e.to_string().contains("matched 3 places"), "{e}");
    }

    #[test]
    fn empty_find_appends_with_newline() {
        let out = apply_edits("line1", &[edit("", "line2")]).unwrap();
        assert_eq!(out, "line1\nline2");
    }

    #[test]
    fn empty_find_appends_without_double_newline() {
        let out = apply_edits("line1\n", &[edit("", "line2")]).unwrap();
        assert_eq!(out, "line1\nline2");
    }

    #[test]
    fn empty_find_on_empty_doc_sets_content() {
        let out = apply_edits("", &[edit("", "first")]).unwrap();
        assert_eq!(out, "first");
    }

    #[test]
    fn edits_apply_in_order_against_running_result() {
        // Second edit's anchor only exists after the first edit ran.
        let out = apply_edits(
            "the cat sat",
            &[edit("cat", "dog"), edit("dog sat", "dog ran")],
        )
        .unwrap();
        assert_eq!(out, "the dog ran");
    }

    #[test]
    fn multiline_anchor_replaces_passage() {
        let doc = "# Title\n\nOld paragraph here.\n\n## Next\n";
        let out = apply_edits(doc, &[edit("Old paragraph here.", "New paragraph.")]).unwrap();
        assert_eq!(out, "# Title\n\nNew paragraph.\n\n## Next\n");
    }
}
