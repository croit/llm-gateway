// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Provider-agnostic tree walker.
//!
//! Breadth-first over a [`FileProvider`], one level at a time, listing each
//! level's directories concurrently. Knows nothing about WebDAV, Graph or
//! Dropbox — it asks the provider for a listing and honours whatever
//! [`ProviderCapabilities`] the provider reports.
//!
//! The walk's one real trick is **subtree pruning**. When the provider
//! reports [`ProviderCapabilities::subtree_pruning`], each directory is
//! listed with the version the previous sync recorded; a provider that
//! recognises the version answers [`DirListing::Unchanged`] and the whole
//! subtree is skipped. The caller then carries its files over from the
//! previous sync rather than re-fetching them. On a corpus where nothing
//! changed, this turns a full walk into one request per unchanged branch.
//!
//! Bounds are not optional here. A shared folder can contain a symlink loop,
//! a million-file dump, or a directory that lists itself; the walker caps
//! total directories and total entries and reports having hit the cap rather
//! than running until the process dies.
//!
//! [`ProviderCapabilities`]: super::ProviderCapabilities

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use tokio::task::JoinSet;

use super::{DirListing, DirRef, FileProvider, ProviderError, RemoteEntry};
use crate::server::rag::walk::Filter;

/// Directory versions recorded by the previous sync, keyed by the
/// directory's collection-relative path. Empty on a first run, which makes
/// every directory look changed — the correct behaviour.
pub type DirVersions = BTreeMap<String, String>;

#[derive(Debug, Clone)]
pub struct WalkOptions {
    /// Files bigger than this are listed but not emitted; the indexer would
    /// only skip them later, and not emitting keeps them out of the counts.
    pub max_file_bytes: u64,
    /// Hard cap on directories visited in one walk.
    pub max_dirs: usize,
    /// Hard cap on files emitted in one walk.
    pub max_files: usize,
    /// Directory listings in flight.
    pub concurrency: usize,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            max_file_bytes: 64 * 1024 * 1024,
            max_dirs: 50_000,
            max_files: 500_000,
            concurrency: 8,
        }
    }
}

/// What one walk found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeSnapshot {
    /// Files that passed the filter, sorted by path for deterministic
    /// downstream ordering.
    pub files: Vec<RemoteEntry>,
    /// Current version of every directory visited, to be stored for the
    /// next sync's pruning.
    pub dir_versions: DirVersions,
    /// Directories the provider reported unchanged. Their previously
    /// indexed files are still live and must be carried over by the caller —
    /// they are absent from `files` precisely because nothing changed.
    pub pruned: Vec<String>,
    /// Directories that failed to list. A walk with any of these is *not*
    /// authoritative about deletions: a folder that 503'd looks identical to
    /// a folder that was emptied, and treating it as the latter deletes a
    /// live subtree from the index.
    pub failed: Vec<FailedDir>,
    /// True when `max_dirs` or `max_files` stopped the walk early. Same
    /// consequence as `failed`: not authoritative.
    pub truncated: bool,
    /// Files skipped for being over `max_file_bytes`.
    pub oversized: usize,
    /// Files skipped by the include/exclude filter.
    pub filtered_out: usize,
}

impl TreeSnapshot {
    /// Whether this walk saw the whole tree, and may therefore drive
    /// deletions. The one question the caller must ask before removing
    /// anything from the index.
    pub fn is_complete(&self) -> bool {
        self.failed.is_empty() && !self.truncated
    }

    /// Stable marker for the snapshot: changes iff any file's identity or
    /// version changed. Stored as the ref's `last_indexed_commit`, which is
    /// how the existing UI shows "what is indexed right now".
    pub fn marker(&self) -> String {
        let mut parts: Vec<String> = self
            .files
            .iter()
            .map(|f| format!("{}:{}", f.id, f.version.as_deref().unwrap_or("?")))
            .collect();
        // Pruned subtrees contribute their directory version, so a change
        // deep inside one still moves the marker.
        for dir in &self.pruned {
            if let Some(v) = self.dir_versions.get(dir) {
                parts.push(format!("{dir}/:{v}"));
            }
        }
        parts.sort();
        crate::server::rag::sha256_hex(&parts.join("\n"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedDir {
    pub rel_path: String,
    pub message: String,
}

/// Walk `provider` from its configured root.
///
/// `prior` supplies the directory versions from the last sync; pass an empty
/// map to force a full walk.
pub async fn walk(
    provider: Arc<dyn FileProvider>,
    prior: &DirVersions,
    filter: &Filter,
    opts: &WalkOptions,
) -> Result<TreeSnapshot, ProviderError> {
    let concurrency = opts.concurrency.max(1);

    let mut snapshot = TreeSnapshot::default();
    let mut level: Vec<DirRef> = vec![provider.root()];
    // A provider that lists a directory as its own child (or a share that
    // loops) would otherwise walk forever.
    let mut seen: HashSet<String> = HashSet::new();
    seen.insert(provider.root().rel_path.clone());
    let mut dirs_visited = 0usize;

    while !level.is_empty() {
        let mut next: Vec<DirRef> = Vec::new();
        for batch in level.chunks(concurrency) {
            if dirs_visited >= opts.max_dirs {
                snapshot.truncated = true;
                break;
            }
            let mut set: JoinSet<(DirRef, Result<DirListing, ProviderError>)> = JoinSet::new();
            for dir in batch {
                let provider = Arc::clone(&provider);
                let mut dir = dir.clone();
                // Always hand the provider what we knew last time and let it
                // decide. Reading `capabilities()` here instead would sample it
                // before the first request, and a provider that *learns* its
                // capabilities from a response (WebDAV latches `oc:fileid` on
                // its first PROPFIND) has not learned them yet — pruning would
                // never engage on the one deployment shape it was built for.
                dir.known_version = prior.get(&dir.rel_path).cloned();
                set.spawn(async move {
                    let res = provider.list_dir(&dir).await;
                    (dir, res)
                });
            }
            while let Some(joined) = set.join_next().await {
                let (dir, result) = joined.map_err(|e| {
                    ProviderError::Malformed(format!("directory listing task failed: {e}"))
                })?;
                dirs_visited += 1;
                match result {
                    Err(err) => {
                        snapshot.failed.push(FailedDir {
                            rel_path: dir.rel_path.clone(),
                            message: err.to_string(),
                        });
                    }
                    Ok(DirListing::Unchanged) => {
                        snapshot.pruned.push(dir.rel_path.clone());
                        if let Some(v) = dir.known_version.clone() {
                            snapshot.dir_versions.insert(dir.rel_path.clone(), v);
                        }
                    }
                    Ok(DirListing::Listed { entries, version }) => {
                        if let Some(v) = version {
                            snapshot.dir_versions.insert(dir.rel_path.clone(), v);
                        }
                        for entry in entries {
                            if entry.is_dir() {
                                if seen.insert(entry.rel_path.clone()) {
                                    next.push(DirRef {
                                        locator: entry.locator,
                                        rel_path: entry.rel_path,
                                        known_version: None,
                                    });
                                }
                                continue;
                            }
                            if !filter.accepts(&entry.rel_path) {
                                snapshot.filtered_out += 1;
                                continue;
                            }
                            if entry.size_bytes > opts.max_file_bytes {
                                snapshot.oversized += 1;
                                continue;
                            }
                            if snapshot.files.len() >= opts.max_files {
                                snapshot.truncated = true;
                                continue;
                            }
                            snapshot.files.push(entry);
                        }
                    }
                }
            }
        }
        if snapshot.truncated {
            break;
        }
        level = next;
    }

    snapshot.files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::rag::source::{DeltaPage, EntryKind, ProbeReport, ProviderCapabilities};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// In-memory provider: a path→children map plus per-directory versions.
    /// A real collaborator rather than an interaction mock — the assertions
    /// below are about what the walk *produced*, with listing counts used
    /// only where "did not descend" is the actual behaviour under test.
    struct FakeProvider {
        dirs: BTreeMap<String, (String, Vec<RemoteEntry>)>,
        caps: ProviderCapabilities,
        listed: Mutex<Vec<String>>,
        fail: Option<String>,
        /// Model a provider that *learns* its capabilities from a response
        /// rather than knowing them up front — what WebDAV does with
        /// `oc:fileid`. Until the first listing lands it claims nothing.
        latches: bool,
        latched: AtomicBool,
    }

    impl FakeProvider {
        fn new(caps: ProviderCapabilities) -> Self {
            Self {
                dirs: BTreeMap::new(),
                caps,
                listed: Mutex::new(Vec::new()),
                fail: None,
                latches: false,
                latched: AtomicBool::new(false),
            }
        }

        /// Report capabilities only once a listing has been seen.
        fn learns_from_first_response(mut self) -> Self {
            self.latches = true;
            self
        }

        fn dir(mut self, path: &str, version: &str, children: Vec<RemoteEntry>) -> Self {
            self.dirs
                .insert(path.to_string(), (version.to_string(), children));
            self
        }

        fn failing_on(mut self, path: &str) -> Self {
            self.fail = Some(path.to_string());
            self
        }
    }

    fn file(path: &str, version: &str, size: u64) -> RemoteEntry {
        RemoteEntry {
            id: format!("id-{path}"),
            locator: path.to_string(),
            rel_path: path.to_string(),
            kind: EntryKind::File,
            version: Some(version.to_string()),
            size_bytes: size,
            mime: None,
            modified_at: None,
        }
    }

    fn dir_entry(path: &str) -> RemoteEntry {
        RemoteEntry {
            id: format!("id-{path}"),
            locator: path.to_string(),
            rel_path: path.to_string(),
            kind: EntryKind::Dir,
            version: Some("d".to_string()),
            size_bytes: 0,
            mime: None,
            modified_at: None,
        }
    }

    #[async_trait::async_trait]
    impl FileProvider for FakeProvider {
        fn kind(&self) -> &'static str {
            "fake"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            if self.latches && !self.latched.load(Ordering::Relaxed) {
                return ProviderCapabilities::default();
            }
            self.caps
        }
        fn root(&self) -> DirRef {
            DirRef::root(String::new())
        }
        async fn list_dir(&self, dir: &DirRef) -> Result<DirListing, ProviderError> {
            self.listed
                .lock()
                .expect("test mutex")
                .push(dir.rel_path.clone());
            if self.fail.as_deref() == Some(dir.rel_path.as_str()) {
                return Err(ProviderError::Status {
                    provider: "fake",
                    status: 503,
                    body: "busy".into(),
                });
            }
            let Some((version, children)) = self.dirs.get(&dir.rel_path) else {
                return Err(ProviderError::NotFound {
                    provider: "fake",
                    path: dir.rel_path.clone(),
                    hint: "",
                });
            };
            self.latched.store(true, Ordering::Relaxed);
            if self.capabilities().subtree_pruning
                && dir.known_version.as_deref() == Some(version.as_str())
            {
                return Ok(DirListing::Unchanged);
            }
            Ok(DirListing::Listed {
                entries: children.clone(),
                version: Some(version.clone()),
            })
        }
        async fn fetch(&self, _e: &RemoteEntry, _max: u64) -> Result<Vec<u8>, ProviderError> {
            Ok(Vec::new())
        }
        async fn probe(&self) -> Result<ProbeReport, ProviderError> {
            Ok(ProbeReport {
                account: None,
                root_entries: 0,
                server: None,
            })
        }
        async fn delta(&self, _c: Option<&str>) -> Result<DeltaPage, ProviderError> {
            Err(ProviderError::Unsupported {
                provider: "fake",
                feature: "delta",
            })
        }
    }

    /// Coerce to the trait object the walker takes.
    fn shared(p: FakeProvider) -> Arc<dyn FileProvider> {
        Arc::new(p)
    }

    fn pruning_caps() -> ProviderCapabilities {
        ProviderCapabilities {
            subtree_pruning: true,
            stable_ids: true,
            ..Default::default()
        }
    }

    /// Pruning must engage for a provider that only learns it can prune
    /// once it has seen a response.
    ///
    /// Regression: the walker used to sample `capabilities().subtree_pruning`
    /// once, before the first request, and skip populating `known_version`
    /// when it read false. WebDAV latches that capability off the first
    /// PROPFIND, and the worker builds a fresh provider every pass — so the
    /// sample was *always* taken cold and pruning never engaged on the one
    /// deployment shape it exists for. Nightly re-syncs silently did a full
    /// walk forever. The walker now always passes what it knew last time and
    /// lets the provider decide, at a point where the provider knows.
    #[tokio::test]
    async fn a_provider_that_learns_it_can_prune_still_prunes() {
        let opts = WalkOptions::default();
        let filter = all_files();

        let first = two_level().learns_from_first_response();
        let snapshot = walk(shared(first), &DirVersions::new(), &filter, &opts)
            .await
            .expect("the cold walk succeeds");
        assert_eq!(
            snapshot.files.len(),
            2,
            "the first pass sees the whole tree"
        );
        assert!(
            snapshot.pruned.is_empty(),
            "nothing to prune on a cold walk"
        );

        // Second pass, nothing changed upstream: a fresh provider (as the
        // worker builds), so its latch starts cold all over again.
        let second = two_level().learns_from_first_response();
        let listed = walk(shared(second), &snapshot.dir_versions, &filter, &opts)
            .await
            .expect("the warm walk succeeds");
        assert!(
            !listed.pruned.is_empty(),
            "an unchanged subtree was pruned rather than re-walked"
        );
        assert!(
            listed.files.is_empty(),
            "a pruned subtree contributes no entries — keep_pruned is what \
             stops those files being read as deletions"
        );
    }

    /// root/ → a.txt, sub/ ; sub/ → b.txt
    fn two_level() -> FakeProvider {
        FakeProvider::new(pruning_caps())
            .dir(
                "",
                "root-v1",
                vec![file("a.txt", "a1", 10), dir_entry("sub")],
            )
            .dir("sub", "sub-v1", vec![file("sub/b.txt", "b1", 20)])
    }

    fn all_files() -> Filter {
        Filter::new(&[], &[], u64::MAX)
    }

    #[tokio::test]
    async fn walks_the_whole_tree_on_a_first_run() {
        let snap = walk(
            shared(two_level()),
            &DirVersions::new(),
            &all_files(),
            &WalkOptions::default(),
        )
        .await
        .unwrap();

        let paths: Vec<&str> = snap.files.iter().map(|f| f.rel_path.as_str()).collect();
        assert_eq!(paths, vec!["a.txt", "sub/b.txt"]);
        assert!(snap.is_complete());
        assert_eq!(
            snap.dir_versions.get("sub").map(String::as_str),
            Some("sub-v1")
        );
    }

    #[tokio::test]
    async fn an_unchanged_subtree_is_not_descended_into() {
        // Kept concrete so the listing log can be inspected afterwards.
        let provider = Arc::new(two_level());
        let prior: DirVersions = [("sub".to_string(), "sub-v1".to_string())]
            .into_iter()
            .collect();

        let snap = walk(
            Arc::clone(&provider) as Arc<dyn FileProvider>,
            &prior,
            &all_files(),
            &WalkOptions::default(),
        )
        .await
        .unwrap();

        assert_eq!(
            snap.files
                .iter()
                .map(|f| f.rel_path.as_str())
                .collect::<Vec<_>>(),
            vec!["a.txt"],
            "the pruned subtree contributes no files; the caller carries them over"
        );
        assert_eq!(snap.pruned, vec!["sub".to_string()]);
        let listed = provider.listed.lock().unwrap().clone();
        assert!(
            !listed.iter().any(|p| p.starts_with("sub/")),
            "nothing below the pruned directory was requested: {listed:?}"
        );
        assert_eq!(
            snap.dir_versions.get("sub").map(String::as_str),
            Some("sub-v1"),
            "a pruned directory keeps its version so the next sync can prune again"
        );
    }

    #[tokio::test]
    async fn pruning_is_ignored_when_the_provider_does_not_promise_it() {
        let provider = FakeProvider::new(ProviderCapabilities::default())
            .dir("", "root-v1", vec![dir_entry("sub")])
            .dir("sub", "sub-v1", vec![file("sub/b.txt", "b1", 5)]);
        let prior: DirVersions = [("sub".to_string(), "sub-v1".to_string())]
            .into_iter()
            .collect();

        let snap = walk(
            shared(provider),
            &prior,
            &all_files(),
            &WalkOptions::default(),
        )
        .await
        .unwrap();

        assert_eq!(
            snap.files.len(),
            1,
            "without the capability a matching version means nothing and we walk"
        );
        assert!(snap.pruned.is_empty());
    }

    #[tokio::test]
    async fn a_failed_directory_makes_the_walk_non_authoritative() {
        let provider = two_level().failing_on("sub");
        let snap = walk(
            shared(provider),
            &DirVersions::new(),
            &all_files(),
            &WalkOptions::default(),
        )
        .await
        .unwrap();

        assert!(
            !snap.is_complete(),
            "a 503 on one folder must never be read as 'that folder is empty now'"
        );
        assert_eq!(snap.failed.len(), 1);
        assert_eq!(snap.failed[0].rel_path, "sub");
        assert_eq!(snap.files.len(), 1, "what did list is still usable");
    }

    #[tokio::test]
    async fn a_directory_cycle_terminates() {
        // `sub` lists itself as a child — a share loop, or a provider bug.
        let provider = FakeProvider::new(pruning_caps())
            .dir("", "v", vec![dir_entry("sub")])
            .dir(
                "sub",
                "v",
                vec![dir_entry("sub"), file("sub/x.txt", "x", 1)],
            );

        let snap = walk(
            shared(provider),
            &DirVersions::new(),
            &all_files(),
            &WalkOptions::default(),
        )
        .await
        .unwrap();

        assert_eq!(snap.files.len(), 1);
    }

    #[tokio::test]
    async fn oversized_and_filtered_files_are_counted_not_silently_dropped() {
        let provider = FakeProvider::new(pruning_caps()).dir(
            "",
            "v",
            vec![
                file("keep.pdf", "k", 10),
                file("huge.pdf", "h", 10_000),
                file("skip.bin", "s", 10),
            ],
        );
        let filter = Filter::new(&["*.pdf".to_string()], &[], u64::MAX);
        let opts = WalkOptions {
            max_file_bytes: 100,
            ..Default::default()
        };

        let snap = walk(shared(provider), &DirVersions::new(), &filter, &opts)
            .await
            .unwrap();

        assert_eq!(
            snap.files
                .iter()
                .map(|f| f.rel_path.as_str())
                .collect::<Vec<_>>(),
            vec!["keep.pdf"]
        );
        assert_eq!(snap.oversized, 1);
        assert_eq!(snap.filtered_out, 1);
    }

    #[tokio::test]
    async fn hitting_the_file_cap_marks_the_walk_truncated() {
        let provider = FakeProvider::new(pruning_caps()).dir(
            "",
            "v",
            vec![file("a.txt", "1", 1), file("b.txt", "1", 1)],
        );
        let opts = WalkOptions {
            max_files: 1,
            ..Default::default()
        };
        let snap = walk(shared(provider), &DirVersions::new(), &all_files(), &opts)
            .await
            .unwrap();
        assert!(snap.truncated);
        assert!(
            !snap.is_complete(),
            "a truncated walk cannot drive deletions"
        );
    }

    #[tokio::test]
    async fn the_marker_moves_when_a_file_version_moves() {
        let before = walk(
            shared(two_level()),
            &DirVersions::new(),
            &all_files(),
            &WalkOptions::default(),
        )
        .await
        .unwrap();

        let changed = FakeProvider::new(pruning_caps())
            .dir(
                "",
                "root-v1",
                vec![file("a.txt", "a2", 10), dir_entry("sub")],
            )
            .dir("sub", "sub-v1", vec![file("sub/b.txt", "b1", 20)]);
        let after = walk(
            shared(changed),
            &DirVersions::new(),
            &all_files(),
            &WalkOptions::default(),
        )
        .await
        .unwrap();

        assert_ne!(before.marker(), after.marker());
    }

    #[tokio::test]
    async fn the_marker_is_stable_across_identical_walks() {
        let a = walk(
            shared(two_level()),
            &DirVersions::new(),
            &all_files(),
            &WalkOptions::default(),
        )
        .await
        .unwrap();
        let b = walk(
            shared(two_level()),
            &DirVersions::new(),
            &all_files(),
            &WalkOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(a.marker(), b.marker());
    }
}
