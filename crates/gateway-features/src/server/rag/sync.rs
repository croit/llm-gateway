// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Working out what actually changed, so a re-index costs the delta rather
//! than the corpus.
//!
//! Until now every build wrote into a fresh store folder and swapped onto it
//! atomically. That is right for a git repo you re-clone in thirty seconds.
//! It is wrong for a document corpus whose first pass costs hours of GPU and
//! whose daily delta is a handful of files — and it is what made the walker's
//! subtree pruning useless, because a pruned subtree contributes no files and
//! a fresh folder has nothing to carry over.
//!
//! An incremental build updates the ref's **live** store in place, driven by
//! the plan this module computes.
//!
//! ## Identity, not paths
//!
//! Files are matched on the provider's stable id where it has one
//! (`oc:fileid`, a Graph item id): a moved folder of 400 invoices is then 400
//! path updates rather than 400 deletions and 400 re-extractions. Providers
//! without stable ids fall back to the path, which is correct but pays full
//! price for a rename.
//!
//! ## The diff is the resume cursor
//!
//! A build interrupted half-way leaves the store partially updated and the
//! stored directory versions untouched. The next run therefore re-walks the
//! whole tree — cheap — and its diff naturally reports only the files that
//! were never indexed, because the ones that *were* now match on version.
//! No cursor table, no bookkeeping to get wrong, and a crash costs one walk
//! rather than one corpus.
//!
//! ## Deletions need an authoritative walk
//!
//! A directory that returned 503 is indistinguishable from a directory that
//! was emptied. [`plan`] therefore only proposes deletions when the walk saw
//! the whole tree, and reports separately when it did not.

use std::collections::{BTreeMap, HashMap};

use super::source::RemoteEntry;

/// What one indexed file looked like at the end of the previous sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedState {
    pub file_id: i64,
    pub path: String,
    /// Provider-native stable id, when the source had one.
    pub remote_id: Option<String>,
    /// The source's change token at the time it was indexed.
    pub source_version: Option<String>,
}

/// One file to (re-)index, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upsert {
    pub entry: RemoteEntry,
    /// The existing row to replace, when this is a change rather than a new
    /// file. Carrying it means the chunk/vector cleanup targets one file id
    /// instead of re-deriving it.
    /// `None` for a file this corpus has not seen before.
    pub replaces: Option<i64>,
}

/// The work one incremental pass should do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncPlan {
    /// Files to fetch, extract, chunk and embed.
    pub upserts: Vec<Upsert>,
    /// `(file_id, new_path)` for files whose content is unchanged but which
    /// moved. Applied as a cheap column update.
    pub renames: Vec<(i64, String)>,
    /// File ids no longer present at the source. Empty unless the walk was
    /// authoritative.
    pub deletions: Vec<i64>,
    /// Files that need no work at all.
    pub unchanged: usize,
    /// True when deletions were withheld because the walk was incomplete.
    pub deletions_withheld: bool,
}

impl SyncPlan {
    /// Whether this pass has anything to do beyond bookkeeping.
    pub fn is_empty(&self) -> bool {
        self.upserts.is_empty() && self.renames.is_empty() && self.deletions.is_empty()
    }

    /// One line for the index-log timeline.
    pub fn summary(&self) -> String {
        let mut parts = vec![format!(
            "{} new/changed, {} unchanged",
            self.upserts.len(),
            self.unchanged
        )];
        if !self.renames.is_empty() {
            parts.push(format!("{} moved", self.renames.len()));
        }
        if !self.deletions.is_empty() {
            parts.push(format!("{} removed", self.deletions.len()));
        }
        if self.deletions_withheld {
            parts.push("deletions withheld — the walk did not see the whole tree".to_string());
        }
        parts.join(", ")
    }
}

/// The key a file is matched on: its stable id if the provider has one,
/// otherwise its path.
fn identity(remote_id: Option<&str>, path: &str) -> String {
    match remote_id.filter(|id| !id.is_empty()) {
        Some(id) => format!("id:{id}"),
        None => format!("path:{path}"),
    }
}

/// Diff what the source has now against what the store holds.
///
/// `stable_ids` says whether the provider's ids survive a move; when false
/// every entry is matched by path, so a rename reads as a delete plus an add
/// — correct, just not free.
pub fn plan(
    current: &[RemoteEntry],
    indexed: &[IndexedState],
    stable_ids: bool,
    walk_was_complete: bool,
) -> SyncPlan {
    let mut by_identity: HashMap<String, &IndexedState> = HashMap::new();
    for state in indexed {
        let remote_id = if stable_ids {
            state.remote_id.as_deref()
        } else {
            None
        };
        by_identity.insert(identity(remote_id, &state.path), state);
    }

    let mut out = SyncPlan::default();
    let mut seen: Vec<String> = Vec::new();
    for entry in current {
        let remote_id = if stable_ids {
            Some(entry.id.as_str())
        } else {
            None
        };
        let key = identity(remote_id, &entry.rel_path);
        seen.push(key.clone());
        match by_identity.get(&key) {
            None => out.upserts.push(Upsert {
                entry: entry.clone(),
                replaces: None,
            }),
            Some(prior) => {
                // Unchanged only when both sides actually have a token and
                // they match. A missing one on either side means "cannot
                // tell", which is a re-read, never a silent skip.
                let same_version = matches!(
                    (prior.source_version.as_deref(), entry.version.as_deref()),
                    (Some(a), Some(b)) if a == b
                );
                let moved = prior.path != entry.rel_path;
                match (same_version, moved) {
                    // Unchanged content, new path: the whole point of keying
                    // on a stable id. One column update.
                    (true, true) => {
                        out.renames.push((prior.file_id, entry.rel_path.clone()));
                    }
                    (true, false) => out.unchanged += 1,
                    (false, _) => out.upserts.push(Upsert {
                        entry: entry.clone(),
                        replaces: Some(prior.file_id),
                    }),
                }
            }
        }
    }

    if walk_was_complete {
        let seen: std::collections::HashSet<&String> = seen.iter().collect();
        for (key, state) in &by_identity {
            if !seen.contains(key) {
                out.deletions.push(state.file_id);
            }
        }
        out.deletions.sort_unstable();
    } else {
        // A folder that 503'd looks exactly like a folder that was emptied.
        // Treating it as the latter deletes a live subtree from the index.
        out.deletions_withheld = !by_identity.is_empty();
    }
    out
}

/// Files that a pruned subtree contributed nothing for, and which must
/// therefore be left alone rather than treated as deleted.
///
/// The walker reports pruned directories; every indexed file under one of
/// them is still live even though the current walk never listed it. Without
/// this, the first pruned re-sync would delete most of the corpus.
pub fn keep_pruned(
    indexed: &[IndexedState],
    pruned_dirs: &[String],
) -> std::collections::HashSet<i64> {
    // Computed once per directory, not once per (file, directory) pair: this
    // runs on every incremental sync and pruning is the common case, so the
    // inner loop is the corpus times the pruned tree.
    let prefixes: Vec<String> = pruned_dirs
        .iter()
        .map(|dir| {
            if dir.is_empty() {
                String::new()
            } else {
                format!("{}/", dir.trim_end_matches('/'))
            }
        })
        .collect();
    let mut keep = std::collections::HashSet::new();
    for state in indexed {
        for prefix in &prefixes {
            if prefix.is_empty() || state.path.starts_with(prefix) {
                keep.insert(state.file_id);
                break;
            }
        }
    }
    keep
}

/// Directory versions to store after a pass, folding the freshly-walked ones
/// over whatever the previous sync knew.
///
/// Merged rather than replaced because a pruned directory is not re-reported
/// with its children's versions; dropping them would make the next sync walk
/// everything again.
pub fn merged_dir_versions(
    prior: &BTreeMap<String, String>,
    fresh: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out = prior.clone();
    for (k, v) in fresh {
        out.insert(k.clone(), v.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::rag::source::EntryKind;

    fn entry(id: &str, path: &str, version: &str) -> RemoteEntry {
        RemoteEntry {
            id: id.into(),
            locator: path.into(),
            rel_path: path.into(),
            kind: EntryKind::File,
            version: Some(version.into()),
            size_bytes: 10,
            mime: None,
            modified_at: None,
        }
    }

    fn state(file_id: i64, id: &str, path: &str, version: &str) -> IndexedState {
        IndexedState {
            file_id,
            path: path.into(),
            remote_id: Some(id.into()),
            source_version: Some(version.into()),
        }
    }

    #[test]
    fn an_unchanged_corpus_produces_no_work() {
        let current = vec![entry("1", "a.pdf", "v1"), entry("2", "b.pdf", "v1")];
        let indexed = vec![state(10, "1", "a.pdf", "v1"), state(11, "2", "b.pdf", "v1")];
        let p = plan(&current, &indexed, true, true);
        assert!(p.is_empty(), "{p:?}");
        assert_eq!(p.unchanged, 2);
    }

    #[test]
    fn a_changed_etag_re_indexes_only_that_file() {
        let current = vec![entry("1", "a.pdf", "v2"), entry("2", "b.pdf", "v1")];
        let indexed = vec![state(10, "1", "a.pdf", "v1"), state(11, "2", "b.pdf", "v1")];
        let p = plan(&current, &indexed, true, true);
        assert_eq!(p.upserts.len(), 1);
        assert_eq!(p.upserts[0].entry.rel_path, "a.pdf");
        assert!(
            p.upserts[0].replaces.is_some(),
            "a changed file replaces the row already indexed for it"
        );
        assert_eq!(p.upserts[0].replaces, Some(10));
        assert_eq!(p.unchanged, 1);
    }

    #[test]
    fn a_moved_file_is_a_path_update_not_a_re_extraction() {
        // The whole reason identity is the file id: a reorganised folder of
        // 400 invoices must not cost 400 OCR runs.
        let current = vec![entry("1", "Finance/2025/a.pdf", "v1")];
        let indexed = vec![state(10, "1", "a.pdf", "v1")];
        let p = plan(&current, &indexed, true, true);
        assert!(p.upserts.is_empty(), "no re-fetch: {p:?}");
        assert_eq!(p.renames, vec![(10, "Finance/2025/a.pdf".to_string())]);
        assert!(p.deletions.is_empty());
    }

    #[test]
    fn a_move_with_an_edit_is_still_a_re_index() {
        let current = vec![entry("1", "new/a.pdf", "v2")];
        let indexed = vec![state(10, "1", "old/a.pdf", "v1")];
        let p = plan(&current, &indexed, true, true);
        assert_eq!(p.upserts.len(), 1);
        assert_eq!(p.upserts[0].replaces, Some(10));
        assert!(p.renames.is_empty());
    }

    #[test]
    fn without_stable_ids_a_move_is_a_delete_plus_an_add() {
        // Correct, just not free — and worth being explicit about, because
        // it is the price a plain WebDAV server pays.
        let current = vec![entry("1", "new/a.pdf", "v1")];
        let indexed = vec![state(10, "1", "old/a.pdf", "v1")];
        let p = plan(&current, &indexed, false, true);
        assert_eq!(p.upserts.len(), 1);
        assert!(
            p.upserts[0].replaces.is_none(),
            "a file this corpus has not seen replaces nothing"
        );
        assert_eq!(p.deletions, vec![10]);
    }

    #[test]
    fn a_vanished_file_is_deleted_after_a_complete_walk() {
        let current = vec![entry("1", "a.pdf", "v1")];
        let indexed = vec![
            state(10, "1", "a.pdf", "v1"),
            state(11, "2", "gone.pdf", "v1"),
        ];
        let p = plan(&current, &indexed, true, true);
        assert_eq!(p.deletions, vec![11]);
        assert!(!p.deletions_withheld);
    }

    #[test]
    fn an_incomplete_walk_never_deletes() {
        // A 503 on one folder looks exactly like that folder being emptied.
        let current = vec![entry("1", "a.pdf", "v1")];
        let indexed = vec![
            state(10, "1", "a.pdf", "v1"),
            state(11, "2", "gone.pdf", "v1"),
        ];
        let p = plan(&current, &indexed, true, false);
        assert!(
            p.deletions.is_empty(),
            "a partial walk must not be read as authoritative"
        );
        assert!(p.deletions_withheld);
    }

    #[test]
    fn a_file_indexed_without_a_version_is_re_indexed_once() {
        // Rows written before incremental sync carry no source version, so
        // the first incremental pass refreshes them and every later pass
        // skips them.
        let current = vec![entry("1", "a.pdf", "v1")];
        let indexed = vec![IndexedState {
            file_id: 10,
            path: "a.pdf".into(),
            remote_id: Some("1".into()),
            source_version: None,
        }];
        let p = plan(&current, &indexed, true, true);
        assert_eq!(p.upserts.len(), 1);
        assert_eq!(p.upserts[0].replaces, Some(10));
    }

    #[test]
    fn files_under_a_pruned_directory_are_kept() {
        // The walker did not list them because nothing beneath that directory
        // changed. Without this they would look deleted, and the first cheap
        // re-sync would wipe most of the corpus.
        let indexed = vec![
            state(10, "1", "Finance/2025/a.pdf", "v1"),
            state(11, "2", "Projects/b.pdf", "v1"),
        ];
        let keep = keep_pruned(&indexed, &["Finance".to_string()]);
        assert!(keep.contains(&10));
        assert!(!keep.contains(&11));
    }

    #[test]
    fn pruning_the_root_keeps_everything() {
        let indexed = vec![state(10, "1", "a.pdf", "v1")];
        let keep = keep_pruned(&indexed, &[String::new()]);
        assert!(keep.contains(&10));
    }

    #[test]
    fn directory_versions_are_merged_so_pruned_branches_stay_prunable() {
        let prior = BTreeMap::from([
            ("Finance".to_string(), "f1".to_string()),
            ("Projects".to_string(), "p1".to_string()),
        ]);
        let fresh = BTreeMap::from([("Projects".to_string(), "p2".to_string())]);
        let merged = merged_dir_versions(&prior, &fresh);
        assert_eq!(merged.get("Finance").unwrap(), "f1", "kept from before");
        assert_eq!(merged.get("Projects").unwrap(), "p2", "freshly walked");
    }

    #[test]
    fn the_summary_says_what_the_pass_will_do() {
        let current = vec![entry("1", "a.pdf", "v2")];
        let indexed = vec![
            state(10, "1", "a.pdf", "v1"),
            state(11, "2", "gone.pdf", "v1"),
        ];
        let s = plan(&current, &indexed, true, true).summary();
        assert!(s.contains("1 new/changed"), "{s}");
        assert!(s.contains("1 removed"), "{s}");
    }
}
