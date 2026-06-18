// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Repo file walker + glob filtering.
//!
//! Walks a cloned working tree and yields the files the indexer should
//! actually chunk. We deliberately avoid pulling a glob crate (`globset`,
//! `glob`) — the patterns operators write for include/exclude are simple,
//! and hand-rolling the three shapes we accept keeps the dep tree honest
//! per `docs/dependencies.md`.
//!
//! Supported patterns (matched against the *repo-relative*, forward-slash
//! normalized path):
//!   * `*.ext`     → file extension match (`*.rs`, `*.md`, …).
//!   * `prefix/`   → directory prefix (path starts with `prefix/`).
//!   * anything else → literal substring (good enough for `node_modules`,
//!     `vendor`, etc., without inventing real glob semantics).
//!
//! `.git/` and symlinks are always skipped. An empty include list means
//! "include everything that isn't excluded". The size guard caps the
//! biggest file we'll read end-to-end — a single huge minified bundle
//! shouldn't be able to OOM the indexer.

use std::path::{Path, PathBuf};

/// Caller-supplied filter. Build with [`Filter::new`].
#[derive(Debug, Clone)]
pub struct Filter {
    includes: Vec<Pattern>,
    excludes: Vec<Pattern>,
    max_bytes: u64,
}

impl Filter {
    /// Construct a filter from the strings stored in `rag_collections`.
    /// `max_bytes` caps the per-file size the walker will return — files
    /// larger than this are silently skipped.
    pub fn new(include_globs: &[String], exclude_globs: &[String], max_bytes: u64) -> Self {
        Self {
            includes: include_globs.iter().map(|s| Pattern::parse(s)).collect(),
            excludes: exclude_globs.iter().map(|s| Pattern::parse(s)).collect(),
            max_bytes,
        }
    }

    /// True if `rel_path` should be indexed.
    pub fn accepts(&self, rel_path: &str) -> bool {
        // `.git/` always excluded.
        if rel_path == ".git" || rel_path.starts_with(".git/") {
            return false;
        }
        if self.excludes.iter().any(|p| p.matches(rel_path)) {
            return false;
        }
        if self.includes.is_empty() {
            return true;
        }
        self.includes.iter().any(|p| p.matches(rel_path))
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }
}

#[derive(Debug, Clone)]
enum Pattern {
    Extension(String), // ".rs" stored without the leading star+dot
    Prefix(String),    // "target/" stored as-is
    Contains(String),  // literal substring
}

impl Pattern {
    fn parse(raw: &str) -> Self {
        if let Some(rest) = raw.strip_prefix("*.") {
            Pattern::Extension(format!(".{rest}"))
        } else if raw.ends_with('/') {
            Pattern::Prefix(raw.to_string())
        } else {
            Pattern::Contains(raw.to_string())
        }
    }

    fn matches(&self, path: &str) -> bool {
        match self {
            Pattern::Extension(ext) => path.ends_with(ext),
            Pattern::Prefix(p) => path.starts_with(p),
            Pattern::Contains(s) => path.contains(s),
        }
    }
}

/// One file the walker has decided to emit. `rel_path` is the
/// forward-slash repo-relative path used both as the SQLite key and in
/// the search-result provenance the tool returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkedFile {
    pub abs_path: PathBuf,
    pub rel_path: String,
    pub size_bytes: u64,
}

/// Recursively walk `root`, returning files that pass the filter,
/// sorted by `rel_path` so the indexer's output is deterministic across
/// runs (and so test assertions don't depend on `read_dir` order).
pub fn walk(root: &Path, filter: &Filter) -> std::io::Result<Vec<WalkedFile>> {
    let mut out = Vec::new();
    visit(root, root, filter, &mut out)?;
    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(out)
}

fn visit(
    root: &Path,
    dir: &Path,
    filter: &Filter,
    out: &mut Vec<WalkedFile>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        // `symlink_metadata` so we never traverse into a symlink (cycle
        // + escape-the-repo safety); the walker is content with files
        // git has materialized into the tree.
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let path = entry.path();
        let rel = match path.strip_prefix(root) {
            Ok(p) => normalize(p),
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            // Cheap dir-level prune: if `.git/`-style excludes match the
            // dir prefix we can skip the whole subtree.
            if !filter.accepts(&format!("{rel}/x")) && rel == ".git" {
                continue;
            }
            // Otherwise descend; per-file accept still gates emission.
            visit(root, &path, filter, out)?;
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        if meta.len() > filter.max_bytes() {
            continue;
        }
        if !filter.accepts(&rel) {
            continue;
        }
        out.push(WalkedFile {
            abs_path: path,
            rel_path: rel,
            size_bytes: meta.len(),
        });
    }
    Ok(())
}

fn normalize(p: &Path) -> String {
    let mut s = String::new();
    for (i, comp) in p.components().enumerate() {
        if i > 0 {
            s.push('/');
        }
        s.push_str(&comp.as_os_str().to_string_lossy());
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn extension_pattern_matches() {
        let f = Filter::new(&["*.rs".into()], &[], 1024);
        assert!(f.accepts("src/main.rs"));
        assert!(!f.accepts("src/main.py"));
    }

    #[test]
    fn prefix_pattern_excludes_subtree() {
        let f = Filter::new(&[], &["target/".into()], 1024);
        assert!(!f.accepts("target/debug/x"));
        assert!(f.accepts("src/main.rs"));
    }

    #[test]
    fn substring_pattern_excludes_anywhere_in_path() {
        let f = Filter::new(&[], &["node_modules".into()], 1024);
        assert!(!f.accepts("ui/node_modules/foo/index.js"));
        assert!(f.accepts("src/main.rs"));
    }

    #[test]
    fn empty_includes_means_everything_not_excluded() {
        let f = Filter::new(&[], &["target/".into()], 1024);
        assert!(f.accepts("README.md"));
        assert!(f.accepts("src/main.rs"));
    }

    #[test]
    fn dot_git_is_always_excluded() {
        let f = Filter::new(&[], &[], 1024);
        assert!(!f.accepts(".git/HEAD"));
        assert!(!f.accepts(".git"));
    }

    #[test]
    fn excludes_override_includes() {
        let f = Filter::new(&["*.rs".into()], &["vendored/".into()], 1024);
        assert!(!f.accepts("vendored/lib.rs"));
        assert!(f.accepts("src/lib.rs"));
    }

    #[test]
    fn walker_skips_oversize_files_and_returns_sorted() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src").join("a.rs"), b"small").unwrap();
        fs::write(root.join("src").join("z.rs"), b"small").unwrap();
        fs::write(root.join("big.rs"), vec![b'x'; 4096]).unwrap();
        let f = Filter::new(&["*.rs".into()], &[], 100);
        let walked = walk(root, &f).unwrap();
        // big.rs filtered by max_bytes; a.rs < z.rs by name.
        assert_eq!(
            walked
                .iter()
                .map(|w| w.rel_path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/a.rs", "src/z.rs"]
        );
    }

    #[test]
    fn walker_skips_dot_git_and_symlinks() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".git").join("HEAD"), b"ref:").unwrap();
        fs::write(root.join("README.md"), b"hi").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("README.md", root.join("link.md")).unwrap();
        }
        let f = Filter::new(&[], &[], 1024);
        let walked = walk(root, &f).unwrap();
        let names: Vec<_> = walked.iter().map(|w| w.rel_path.as_str()).collect();
        assert!(names.contains(&"README.md"));
        assert!(!names.iter().any(|n| n.starts_with(".git")));
        assert!(!names.contains(&"link.md"));
    }
}
