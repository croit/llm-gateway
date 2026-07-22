// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Hot-reloadable catalog of ComfyUI workflows.
//!
//! A [`ComfyuiStore`] holds an [`Arc`]`<`[`Snapshot`]`>` behind a
//! `RwLock`. The snapshot bundles the loaded manifests with the
//! `comfyui_<id>` tool wrappers built from them, so a reload atomically
//! swaps both at once: in-flight requests keep their old `Arc`, new
//! requests see the fresh catalog. Operators trigger a reload from the
//! admin UI (`POST /api/v0/comfyui/reload`) — no gateway restart.
//!
//! Pattern lifted from [`crate::server::skills::SkillStore`] (the other
//! hot-reloadable catalog in the gateway). The tool wrappers themselves
//! are stateless — they read the current snapshot on every `schema()` /
//! `run()` — so they don't need to be rebuilt on reload; only the catalog
//! lookup underneath them swaps.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::Serialize;

use super::manifest::{ManifestError, WorkflowManifest, load as load_manifest};

/// One immutable catalog snapshot. Cheaply clone-able via the outer
/// `Arc<Snapshot>`; the wrapper tools hold that `Arc` for the duration of
/// a `schema()` / `run()` call so they always see a consistent view.
#[derive(Debug)]
pub struct Snapshot {
    /// Manifests keyed by their declared `id` (e.g. `"text_to_image"`).
    by_id: HashMap<String, Arc<WorkflowManifest>>,
}

impl Snapshot {
    /// Empty snapshot — used at boot when the directory is unreadable so
    /// the gateway still comes up (admin can fix the dir + trigger a
    /// reload).
    pub fn empty() -> Self {
        Self {
            by_id: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Look up a workflow by its manifest `id` (NOT the tool-id — pass
    /// `"text_to_image"`, not `"comfyui_text_to_image"`).
    pub fn lookup(&self, id: &str) -> Option<Arc<WorkflowManifest>> {
        self.by_id.get(id).cloned()
    }

    /// Every loaded workflow, in stable (alphabetical) order. Used by the
    /// reload-report + admin UI listings.
    pub fn workflows(&self) -> Vec<Arc<WorkflowManifest>> {
        let mut all: Vec<_> = self.by_id.values().cloned().collect();
        all.sort_by(|a, b| a.id.cmp(&b.id));
        all
    }
}

/// Hot-reloadable workflow catalog.
pub struct ComfyuiStore {
    dir: PathBuf,
    current: RwLock<Arc<Snapshot>>,
}

/// Summary of one reload — surfaces what landed, what was skipped, and
/// what broke. Surfaced to the admin UI verbatim so an operator sees the
/// effect of a hot-reload without grepping logs.
#[derive(Debug, Clone, Serialize)]
pub struct ReloadReport {
    pub loaded: Vec<String>,
    pub skipped: Vec<ReloadSkip>,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReloadSkip {
    pub source: String,
    pub reason: String,
}

impl ComfyuiStore {
    /// Scan `dir` once and build the store. A read error yields an empty
    /// snapshot (logged) rather than failing — same boot-tolerance as
    /// [`super::manifest::load`]. The directory is recorded so later
    /// [`Self::reload`] calls can re-scan it.
    pub fn load(dir: PathBuf) -> Self {
        let (snapshot, _) = scan(&dir);
        Self {
            dir,
            current: RwLock::new(Arc::new(snapshot)),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The current snapshot — cheap (`Arc` clone under a brief read lock).
    /// The request path calls this freely.
    pub fn current(&self) -> Arc<Snapshot> {
        self.current.read().expect("comfyui lock poisoned").clone()
    }

    /// Re-scan the directory and atomically swap in a fresh snapshot.
    /// Returns a [`ReloadReport`] describing what landed. In-flight tool
    /// calls keep their old snapshot `Arc`; new calls see the new catalog.
    pub fn reload(&self) -> ReloadReport {
        let (snapshot, report) = scan(&self.dir);
        *self.current.write().expect("comfyui lock poisoned") = Arc::new(snapshot);
        report
    }
}

/// Walk `dir` once, loading every subdirectory that owns a parseable
/// `manifest.toml` + `workflow.json` pair. Subdirs that fail validation
/// are skipped with a `ReloadSkip` entry naming the path + the reason —
/// one bad workflow must not poison the rest.
fn scan(dir: &Path) -> (Snapshot, ReloadReport) {
    let mut by_id = HashMap::new();
    let mut loaded = Vec::new();
    let mut skipped = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!(error = %err, dir = %dir.display(), "comfyui: content_dir unreadable");
            return (
                Snapshot::empty(),
                ReloadReport {
                    loaded,
                    skipped: vec![ReloadSkip {
                        source: dir.display().to_string(),
                        reason: format!("could not read directory: {err}"),
                    }],
                    total: 0,
                },
            );
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let source = path.display().to_string();
        match load_manifest(&path) {
            Ok(m) => {
                if by_id.contains_key(&m.id) {
                    let reason = format!("duplicate workflow id `{}`", m.id);
                    tracing::warn!(%source, %reason, "comfyui workflow skipped");
                    skipped.push(ReloadSkip { source, reason });
                    continue;
                }
                tracing::info!(id = %m.id, %source, "comfyui workflow loaded");
                loaded.push(m.id.clone());
                by_id.insert(m.id.clone(), Arc::new(m));
            }
            Err(err) => {
                let reason = match err {
                    ManifestError::Read { path, source } => {
                        format!("reading manifest `{}`: {source}", path.display())
                    }
                    ManifestError::Parse { path, source } => {
                        format!("parsing manifest `{}`: {source}", path.display())
                    }
                    ManifestError::Invalid { message, .. } => message,
                    ManifestError::WorkflowParse { path, source } => {
                        format!("parsing workflow.json `{}`: {source}", path.display())
                    }
                };
                tracing::warn!(%source, %reason, "comfyui workflow skipped");
                skipped.push(ReloadSkip { source, reason });
            }
        }
    }

    loaded.sort();
    let total = loaded.len();
    let snapshot = Snapshot { by_id };
    (
        snapshot,
        ReloadReport {
            loaded,
            skipped,
            total,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_workflow(dir: &Path, id: &str) -> PathBuf {
        let sub = dir.join(id);
        std::fs::create_dir_all(&sub).unwrap();
        let manifest = format!(
            r#"
id = "{id}"
title = "{id} title"
description = "{id} description"
output_kind = "image"
output_node_id = "9"
output_filename_prefix = "{id}"

[[params]]
key = "prompt"
node_id = "6"
input_key = "text"
required = true
description = "what to draw"

[params.schema]
type = "string"
"#
        );
        std::fs::write(sub.join("manifest.toml"), manifest).unwrap();
        std::fs::write(sub.join("workflow.json"), "{}").unwrap();
        sub
    }

    #[test]
    fn load_returns_empty_when_dir_missing() {
        let store = ComfyuiStore::load(PathBuf::from("/nonexistent/comfyui-content"));
        assert!(store.current().is_empty());
    }

    #[test]
    fn load_picks_up_one_workflow() {
        let tmp = TempDir::new().unwrap();
        write_workflow(tmp.path(), "alpha");
        let store = ComfyuiStore::load(tmp.path().to_path_buf());
        let snap = store.current();
        assert_eq!(snap.len(), 1);
        assert!(snap.lookup("alpha").is_some());
        assert!(snap.lookup("bravo").is_none());
    }

    #[test]
    fn reload_picks_up_new_workflow_added_after_boot() {
        let tmp = TempDir::new().unwrap();
        write_workflow(tmp.path(), "alpha");
        let store = ComfyuiStore::load(tmp.path().to_path_buf());
        assert_eq!(store.current().len(), 1);

        write_workflow(tmp.path(), "bravo");
        let report = store.reload();
        assert_eq!(report.total, 2);
        assert!(report.loaded.contains(&"alpha".to_string()));
        assert!(report.loaded.contains(&"bravo".to_string()));
        assert!(store.current().lookup("bravo").is_some());
    }

    #[test]
    fn reload_reports_skipped_invalid_workflow() {
        let tmp = TempDir::new().unwrap();
        write_workflow(tmp.path(), "alpha");
        let bad = tmp.path().join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("manifest.toml"), "not = valid = toml").unwrap();

        let store = ComfyuiStore::load(tmp.path().to_path_buf());
        let report = store.reload();
        assert_eq!(report.total, 1);
        assert!(report.loaded.contains(&"alpha".to_string()));
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].source.contains("bad"));
        assert!(!report.skipped[0].reason.is_empty());
    }

    #[test]
    fn reload_skips_duplicate_ids() {
        let tmp = TempDir::new().unwrap();
        write_workflow(tmp.path(), "alpha");
        // Second subdir, same id inside its manifest.
        let dup = tmp.path().join("dup");
        std::fs::create_dir_all(&dup).unwrap();
        std::fs::write(
            dup.join("manifest.toml"),
            r#"
id = "alpha"
title = "dup"
description = "duplicate id"
output_kind = "image"
output_node_id = "9"

[[params]]
key = "prompt"
node_id = "6"
input_key = "text"
required = true
description = "d"

[params.schema]
type = "string"
"#,
        )
        .unwrap();
        std::fs::write(dup.join("workflow.json"), "{}").unwrap();

        let store = ComfyuiStore::load(tmp.path().to_path_buf());
        let snap = store.current();
        assert_eq!(snap.len(), 1);
        let report = store.reload();
        assert_eq!(report.total, 1);
        assert!(
            report
                .skipped
                .iter()
                .any(|s| s.reason.contains("duplicate"))
        );
    }

    #[test]
    fn current_returns_same_arc_until_reload() {
        let tmp = TempDir::new().unwrap();
        write_workflow(tmp.path(), "alpha");
        let store = ComfyuiStore::load(tmp.path().to_path_buf());
        let a = store.current();
        let b = store.current();
        assert!(Arc::ptr_eq(&a, &b), "no reload → same snapshot Arc");
        store.reload();
        let c = store.current();
        assert!(!Arc::ptr_eq(&a, &c), "reload swaps snapshot");
    }

    #[test]
    fn workflows_returns_alphabetical() {
        let tmp = TempDir::new().unwrap();
        write_workflow(tmp.path(), "zeta");
        write_workflow(tmp.path(), "alpha");
        write_workflow(tmp.path(), "mid");
        let store = ComfyuiStore::load(tmp.path().to_path_buf());
        let ordered: Vec<_> = store
            .current()
            .workflows()
            .into_iter()
            .map(|m| m.id.clone())
            .collect();
        assert_eq!(ordered, vec!["alpha", "mid", "zeta"]);
    }
}
