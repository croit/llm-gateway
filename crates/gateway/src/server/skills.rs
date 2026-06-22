// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Agent Skills — operator-managed instruction bundles the chat model can
//! load on demand.
//!
//! A skill is **context, not code**: a `SKILL.md` (YAML frontmatter +
//! markdown body) plus optional `references/` and `assets/` files. The
//! frontmatter's `name` + `description` are advertised to the model in the
//! chat system message (cheap — metadata only); when a request is relevant
//! the model calls the `read_skill` tool to pull the full body, and
//! `read_skill(name, path)` to pull a reference or asset (e.g. an SVG it
//! inlines into HTML). This is the standard progressive-disclosure shape:
//! the model only pays for the detail it actually needs.
//!
//! Operators drop bundles under `[skills] dir`, exactly like Typst
//! templates under `[typst] templates_dir`:
//!
//! - a directory containing a `SKILL.md` (optionally nested one level —
//!   the folder a `*.skill` archive unzips to), or
//! - a `*.skill` file (a zip of such a directory), extracted into
//!   `<dir>/.cache/` at startup.
//!
//! At gateway startup [`discover`] walks the directory and returns one
//! [`Skill`] per valid bundle; `main` wraps them in a hot-reloadable
//! [`SkillStore`] and registers the `read_skill` tool. The store supports
//! live admin **upload** and **delete** ([`SkillStore::install_archive`] /
//! [`SkillStore::remove`]): each re-scans the directory and atomically swaps
//! the registry, so changes take effect with no gateway restart. RBAC gates
//! which roles see which skill, by `name`, via the role's `skills` list (see
//! `rbac::Resolver`).

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};

use thiserror::Error;

/// The single manifest file every skill bundle must contain.
pub const MANIFEST_FILE: &str = "SKILL.md";

/// Cache subdirectory `*.skill` archives are extracted into. Excluded from
/// the top-level bundle scan as a *name* so an extracted bundle living in
/// `<dir>/.cache/<bundle>/` is still found (we recurse one level), while a
/// bundle dropped directly as `<dir>/<bundle>/` is found too.
const CACHE_DIR: &str = ".cache";

/// One loaded skill. Cheap to clone (a couple of `String`s + a `PathBuf`);
/// held in `Arc<SkillRegistry>` so the chat driver and the `read_skill`
/// tool share one copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// From the frontmatter `name`. The lookup key the model passes to
    /// `read_skill`, and the id RBAC grants. Validated to `[A-Za-z0-9._-]`,
    /// non-empty, <= 64 chars so it's safe as a tool argument and matches
    /// cleanly against a role's `skills` list.
    pub name: String,
    /// Human-readable display name for UI surfaces (the composer's "+" menu).
    /// From the optional frontmatter `title`; when absent, derived by
    /// prettifying `name` (`commit-message-helper` → "Commit Message Helper").
    /// Not part of the model-facing advertisement — the model still keys off
    /// `name` — so this is a pure presentation field and a safe extension of
    /// the base skill frontmatter (`name` + `description`).
    pub title: String,
    /// From the frontmatter `description`. Advertised verbatim to the model
    /// so it can decide when the skill is relevant.
    pub description: String,
    /// Directory holding `SKILL.md` + any `references/` / `assets/`. Used as
    /// the jail root for [`Skill::read_file`].
    pub root: PathBuf,
}

impl Skill {
    /// The markdown body of `SKILL.md` with the YAML frontmatter stripped —
    /// the instructions the model acts on. Read fresh from disk so a large
    /// body isn't held in memory for every skill.
    pub fn body(&self) -> Result<String, std::io::Error> {
        let raw = std::fs::read_to_string(self.root.join(MANIFEST_FILE))?;
        Ok(strip_frontmatter(&raw).to_string())
    }

    /// Relative paths of every readable file in the bundle except
    /// `SKILL.md` itself, sorted, so the model can discover what
    /// `read_skill(name, path)` can pull (references, assets). Paths use
    /// `/` separators regardless of platform.
    pub fn files(&self) -> Vec<String> {
        let mut out = Vec::new();
        collect_files(&self.root, &self.root, &mut out);
        out.retain(|p| p != MANIFEST_FILE);
        out.sort();
        out
    }

    /// Read a single bundled file by its bundle-relative `path`. Path-jailed:
    /// the resolved target must stay inside `root`, so a malicious or
    /// mistaken `../` can't escape the bundle. Returns the contents as UTF-8
    /// text (skill assets are SVG/markdown/text); a non-UTF-8 file is a
    /// clean error rather than lossy bytes.
    pub fn read_file(&self, path: &str) -> Result<String, ReadFileError> {
        let rel = Path::new(path);
        // Reject absolute paths and any `..` / root / prefix component up
        // front — we resolve purely lexically (no `canonicalize`) so this
        // works for paths that don't exist yet and never touches symlinks.
        for comp in rel.components() {
            match comp {
                Component::Normal(_) | Component::CurDir => {}
                _ => return Err(ReadFileError::Escapes(path.to_string())),
            }
        }
        let target = self.root.join(rel);
        if !target.starts_with(&self.root) {
            return Err(ReadFileError::Escapes(path.to_string()));
        }
        if !target.is_file() {
            return Err(ReadFileError::NotFound(path.to_string()));
        }
        let bytes = std::fs::read(&target).map_err(|e| ReadFileError::Io(path.to_string(), e))?;
        String::from_utf8(bytes).map_err(|_| ReadFileError::NotText(path.to_string()))
    }

    /// Re-package this bundle into an in-memory `.skill` archive: a zip with
    /// `SKILL.md` and every asset at the archive root — exactly the layout
    /// [`SkillStore::install_archive`] accepts, so a downloaded skill
    /// re-uploads cleanly. Reads each file as raw bytes (assets may be
    /// binary), unlike [`Skill::read_file`]'s text-only path.
    pub fn to_archive(&self) -> Result<Vec<u8>, StoreError> {
        use std::io::Write as _;
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut cursor);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            // SKILL.md first, then every asset. `files()` already excludes
            // SKILL.md and uses `/` separators (valid zip entry names).
            let mut entries = vec![MANIFEST_FILE.to_string()];
            entries.extend(self.files());
            for rel in entries {
                let bytes = std::fs::read(self.root.join(&rel))?;
                zip.start_file(rel.as_str(), opts)
                    .map_err(std::io::Error::other)?;
                zip.write_all(&bytes)?;
            }
            zip.finish().map_err(std::io::Error::other)?;
        }
        Ok(cursor.into_inner())
    }
}

/// Failure reading a bundled file via [`Skill::read_file`]. All variants
/// carry the offending relative path so the tool can echo a precise message
/// back to the model.
#[derive(Debug, Error)]
pub enum ReadFileError {
    #[error("path `{0}` escapes the skill bundle")]
    Escapes(String),
    #[error("no file `{0}` in this skill")]
    NotFound(String),
    #[error("file `{0}` is not UTF-8 text")]
    NotText(String),
    #[error("reading `{0}`: {1}")]
    Io(String, #[source] std::io::Error),
}

/// Every loaded skill, keyed by name. Built once at startup; shared behind
/// an `Arc`. The `BTreeMap` keeps iteration order stable (by name) so the
/// system-message skill listing is byte-stable across boots — the same
/// reason `enable_tools` sorts its catalog (keeps the upstream prefix cache
/// warm).
#[derive(Debug, Default, Clone)]
pub struct SkillRegistry {
    skills: BTreeMap<String, Skill>,
}

impl SkillRegistry {
    /// Build from a list of discovered skills. On a duplicate `name` the
    /// first wins and the rest are logged and dropped — a name is the
    /// model's lookup key, so collisions are ambiguous, not mergeable.
    pub fn new(skills: impl IntoIterator<Item = Skill>) -> Self {
        let mut map = BTreeMap::new();
        for skill in skills {
            if let Some(existing) = map.get(&skill.name) {
                let existing: &Skill = existing;
                tracing::warn!(
                    name = %skill.name,
                    kept = %existing.root.display(),
                    dropped = %skill.root.display(),
                    "duplicate skill name — keeping the first, dropping the rest"
                );
                continue;
            }
            map.insert(skill.name.clone(), skill);
        }
        Self { skills: map }
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    /// Skill names in stable (sorted) order. Used by RBAC to expand `"*"`
    /// and to intersect with a role's `skills` grant.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.skills.keys().map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.skills.values()
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

/// Failure installing or removing a skill at runtime via [`SkillStore`].
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("not a readable .skill archive (expected a zip): {0}")]
    BadArchive(String),
    #[error("archive contains no SKILL.md bundle")]
    NoManifest,
    #[error("SKILL.md is missing the required `{0}` field")]
    MissingField(&'static str),
    #[error("skill name `{0}` is not valid (use letters, digits, `.`, `_`, `-`; max 64)")]
    BadName(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// A hot-reloadable view over the loaded skills and the directory they live
/// in. Reads ([`current`](Self::current)) take a brief read lock and clone
/// the shared `Arc<SkillRegistry>`; writes ([`install_archive`](Self::install_archive)
/// / [`remove`](Self::remove)) mutate the directory then re-scan and
/// atomically swap the registry — so an admin upload or delete takes effect
/// immediately, with no gateway restart. In-flight requests keep whatever
/// `Arc` they already cloned.
pub struct SkillStore {
    dir: PathBuf,
    current: RwLock<Arc<SkillRegistry>>,
}

impl SkillStore {
    /// Scan `dir` once and build the store. A read error yields an empty
    /// registry (logged) rather than failing — same boot-tolerance as
    /// [`discover`].
    pub fn load(dir: PathBuf) -> Self {
        let registry = Arc::new(SkillRegistry::new(scan(&dir)));
        Self {
            dir,
            current: RwLock::new(registry),
        }
    }

    /// Build a store around an already-built registry without touching disk.
    /// For tests and in-memory wiring; [`reload`](Self::reload) would re-scan
    /// `dir`, so only call it when `dir` actually backs `registry`.
    pub fn with_registry(dir: PathBuf, registry: SkillRegistry) -> Self {
        Self {
            dir,
            current: RwLock::new(Arc::new(registry)),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The current registry snapshot — cheap (an `Arc` clone under a brief
    /// read lock), so the request path can call it freely.
    pub fn current(&self) -> Arc<SkillRegistry> {
        self.current.read().expect("skills lock poisoned").clone()
    }

    /// Re-scan the directory and atomically swap in a fresh registry. Returns
    /// the new skill count.
    pub fn reload(&self) -> usize {
        let registry = Arc::new(SkillRegistry::new(scan(&self.dir)));
        let n = registry.len();
        *self.current.write().expect("skills lock poisoned") = registry;
        n
    }

    /// Install a skill from a `.skill` archive's bytes: extract it, find the
    /// `SKILL.md` bundle inside, validate its frontmatter, and write it as a
    /// plain directory `<dir>/<name>/` (replacing an existing skill of the
    /// same name). Re-scans, then returns the installed skill's `name`.
    pub fn install_archive(&self, bytes: &[u8]) -> Result<String, StoreError> {
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
            .map_err(|e| StoreError::BadArchive(e.to_string()))?;
        let tmp = tempfile::tempdir()?;
        zip.extract(tmp.path())
            .map_err(|e| StoreError::BadArchive(e.to_string()))?;

        let bundle = find_bundle_root(tmp.path()).ok_or(StoreError::NoManifest)?;
        // Validate the manifest before touching the live directory, so a bad
        // upload can't leave a half-written bundle behind.
        let raw = std::fs::read_to_string(bundle.join(MANIFEST_FILE))?;
        let front = parse_frontmatter(&raw);
        let name = front
            .get("name")
            .ok_or(StoreError::MissingField("name"))?
            .clone();
        if !front.contains_key("description") {
            return Err(StoreError::MissingField("description"));
        }
        if !is_valid_name(&name) {
            return Err(StoreError::BadName(name));
        }

        std::fs::create_dir_all(&self.dir)?;
        let target = self.dir.join(&name);
        if target.exists() {
            std::fs::remove_dir_all(&target)?;
        }
        copy_dir_all(&bundle, &target)?;
        self.reload();
        Ok(name)
    }

    /// Remove a loaded skill by `name`, deleting whatever produced it — the
    /// `<dir>/<name>/` directory for an installed/dropped bundle, or the
    /// originating `*.skill` archive (plus its cache extraction) for a
    /// manually dropped archive. Re-scans. Returns whether a skill was removed.
    pub fn remove(&self, name: &str) -> Result<bool, StoreError> {
        let registry = self.current();
        let Some(skill) = registry.get(name) else {
            return Ok(false);
        };
        let cache = self.dir.join(CACHE_DIR);
        if let Ok(rel) = skill.root.strip_prefix(&cache) {
            // Archive-sourced (`<dir>/.cache/<topdir>/…`): delete the archive
            // whose top-level entry matches, then the extracted cache dir.
            if let Some(top) = rel.components().next() {
                let top_name = top.as_os_str();
                if let Some(archive) = archive_with_top_dir(&self.dir, top_name) {
                    let _ = std::fs::remove_file(&archive);
                }
                let _ = std::fs::remove_dir_all(cache.join(top_name));
            }
        } else if let Ok(rel) = skill.root.strip_prefix(&self.dir)
            && let Some(top) = rel.components().next()
        {
            // Plain bundle: remove the top-level subdir of `dir` that holds it.
            std::fs::remove_dir_all(self.dir.join(top.as_os_str()))?;
        }
        self.reload();
        Ok(true)
    }
}

/// `discover`, but tolerant: a read error logs and yields no skills (so the
/// store stays usable and an upload can still create the dir).
fn scan(dir: &Path) -> Vec<Skill> {
    discover(dir).unwrap_or_else(|err| {
        tracing::warn!(error = %err, dir = %dir.display(), "skills discovery failed");
        Vec::new()
    })
}

/// Locate the bundle root inside an extracted archive: the directory that
/// directly contains a `SKILL.md`, whether that's the archive root itself or
/// a single wrapping directory (depth 1 or 2).
fn find_bundle_root(base: &Path) -> Option<PathBuf> {
    if base.join(MANIFEST_FILE).is_file() {
        return Some(base.to_path_buf());
    }
    for entry in std::fs::read_dir(base).ok()?.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        if p.join(MANIFEST_FILE).is_file() {
            return Some(p);
        }
        if let Ok(children) = std::fs::read_dir(&p) {
            for child in children.flatten() {
                let cp = child.path();
                if cp.is_dir() && cp.join(MANIFEST_FILE).is_file() {
                    return Some(cp);
                }
            }
        }
    }
    None
}

/// Recursively copy `src`'s contents into `dst` (creating `dst`).
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// The `*.skill` archive in `dir` whose top-level entry directory matches
/// `top_name` — i.e. the archive that extracted to `.cache/<top_name>/`.
fn archive_with_top_dir(dir: &Path, top_name: &std::ffi::OsStr) -> Option<PathBuf> {
    let want = top_name.to_str()?;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if p.is_file()
            && p.extension().is_some_and(|e| e == "skill")
            && archive_top_component(&p).as_deref() == Some(want)
        {
            return Some(p);
        }
    }
    None
}

/// The top-level path component of an archive's first entry (its wrapping
/// directory), used to match an extracted `.cache/<topdir>` back to its
/// source archive.
fn archive_top_component(archive: &Path) -> Option<String> {
    let file = std::fs::File::open(archive).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    let first = zip.by_index(0).ok()?;
    first.name().split('/').next().map(str::to_string)
}

#[derive(Debug, Error)]
pub enum DiscoverError {
    #[error("skills dir `{0}` not readable")]
    DirRead(PathBuf, #[source] std::io::Error),
}

/// Walk `dir` and return one [`Skill`] per valid bundle. First extracts any
/// `*.skill` archives into `<dir>/.cache/`, then collects every directory
/// (at depth 1 or 2) that directly contains a `SKILL.md`. A bundle whose
/// manifest is missing the required fields, or whose `name` is invalid, is
/// logged and skipped — one bad bundle never keeps the gateway from booting,
/// mirroring `typst::discover_templates`.
pub fn discover(dir: &Path) -> Result<Vec<Skill>, DiscoverError> {
    // Best-effort: stale extractions / a bad archive are logged inside and
    // never fail discovery (the operator's good bundles still load).
    extract_archives(dir);

    let entries =
        std::fs::read_dir(dir).map_err(|e| DiscoverError::DirRead(dir.to_path_buf(), e))?;
    let mut roots: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.join(MANIFEST_FILE).is_file() {
            // Bundle dropped directly: `<dir>/<bundle>/SKILL.md`.
            roots.push(path);
            continue;
        }
        // Otherwise look one level down — covers `<dir>/.cache/<bundle>/`
        // (extracted archives) and a manually-unzipped wrapper directory.
        if let Ok(children) = std::fs::read_dir(&path) {
            for child in children.flatten() {
                let cpath = child.path();
                if cpath.is_dir() && cpath.join(MANIFEST_FILE).is_file() {
                    roots.push(cpath);
                }
            }
        }
    }

    let mut out = Vec::new();
    for root in roots {
        match load_skill(&root) {
            Ok(skill) => out.push(skill),
            Err(err) => {
                tracing::warn!(error = %err, dir = %root.display(), "skipping skill");
            }
        }
    }
    Ok(out)
}

/// Per-bundle load failure. Kept private — discovery logs and skips, so
/// these never surface above `discover`.
#[derive(Debug, Error)]
enum LoadError {
    #[error("reading {0}")]
    Read(PathBuf, #[source] std::io::Error),
    #[error("missing required frontmatter field `{0}` in SKILL.md")]
    MissingField(&'static str),
    #[error("skill name `{0}` is not valid (use letters, digits, `.`, `_`, `-`; max 64)")]
    BadName(String),
}

fn load_skill(root: &Path) -> Result<Skill, LoadError> {
    let manifest = root.join(MANIFEST_FILE);
    let raw = std::fs::read_to_string(&manifest).map_err(|e| LoadError::Read(manifest, e))?;
    let front = parse_frontmatter(&raw);
    let name = front.get("name").ok_or(LoadError::MissingField("name"))?;
    let description = front
        .get("description")
        .ok_or(LoadError::MissingField("description"))?;
    if !is_valid_name(name) {
        return Err(LoadError::BadName(name.clone()));
    }
    let title = front
        .get("title")
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| prettify_name(name));
    Ok(Skill {
        name: name.clone(),
        title,
        description: description.clone(),
        root: root.to_path_buf(),
    })
}

/// Derive a human-readable display name from a slug `name`: split on `-`/`_`/`.`
/// and title-case each word (`commit-message-helper` → "Commit Message Helper").
/// Used when a skill's frontmatter doesn't set an explicit `title`.
fn prettify_name(slug: &str) -> String {
    slug.split(['-', '_', '.'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A skill `name` must be safe to pass back as a tool argument and to match
/// against a role's `skills` list: non-empty, `[A-Za-z0-9._-]`, <= 64 chars.
fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Extract every `*.skill` archive in `dir` into `<dir>/.cache/`. Best-effort
/// and idempotent-ish: the cache is cleared first so a removed/renamed
/// archive doesn't leave a stale bundle behind. Archive entries carry their
/// own top-level directory, so extracting into `.cache/` yields
/// `.cache/<bundle>/SKILL.md`. Failures (unreadable dir, corrupt zip) are
/// logged per-archive and never abort discovery.
fn extract_archives(dir: &Path) {
    let archives: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "skill"))
            .collect(),
        Err(_) => return,
    };
    if archives.is_empty() {
        return;
    }
    let cache = dir.join(CACHE_DIR);
    if cache.exists()
        && let Err(err) = std::fs::remove_dir_all(&cache)
    {
        tracing::warn!(error = %err, dir = %cache.display(), "could not clear skills cache");
    }
    if let Err(err) = std::fs::create_dir_all(&cache) {
        tracing::warn!(error = %err, dir = %cache.display(), "could not create skills cache");
        return;
    }
    for archive in archives {
        if let Err(err) = extract_one(&archive, &cache) {
            tracing::warn!(error = %err, archive = %archive.display(), "skipping unreadable .skill archive");
        }
    }
}

fn extract_one(archive: &Path, cache: &Path) -> Result<(), anyhow::Error> {
    let file = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file)?;
    zip.extract(cache)?;
    Ok(())
}

/// Recursively gather files under `base`, as `/`-joined paths relative to
/// `root`. Skips the extraction cache so a `read_skill` file listing never
/// leaks `.cache/` internals.
fn collect_files(base: &Path, root: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == CACHE_DIR) {
            continue;
        }
        if path.is_dir() {
            collect_files(&path, root, out);
        } else if path.is_file()
            && let Ok(rel) = path.strip_prefix(root)
        {
            out.push(
                rel.components()
                    .filter_map(|c| c.as_os_str().to_str())
                    .collect::<Vec<_>>()
                    .join("/"),
            );
        }
    }
}

/// Return the body of a `SKILL.md`, i.e. everything after a leading
/// `---`-fenced YAML frontmatter block. If there's no frontmatter the whole
/// string is the body.
fn strip_frontmatter(raw: &str) -> &str {
    let trimmed = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let Some(rest) = trimmed.strip_prefix("---") else {
        return raw;
    };
    // The opening fence must be its own line.
    let Some(rest) = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
    else {
        return raw;
    };
    // Find the closing fence line (`---` alone) and return what follows it.
    let mut idx = 0;
    for line in rest.split_inclusive('\n') {
        let trimmed_line = line.trim_end_matches(['\n', '\r']);
        if trimmed_line == "---" {
            return rest[idx + line.len()..].trim_start_matches(['\n', '\r']);
        }
        idx += line.len();
    }
    raw
}

/// Parse the `key: value` scalars out of a `SKILL.md`'s leading
/// `---`-fenced YAML frontmatter. Deliberately minimal — skills only need
/// `name` + `description`, both inline scalars — so we don't pull a full
/// YAML parser. Surrounding single/double quotes are stripped; lines
/// without a `:` (and the body) are ignored. A file with no frontmatter
/// yields an empty map, which `load_skill` reports as a missing field.
fn parse_frontmatter(raw: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let trimmed = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let Some(rest) = trimmed.strip_prefix("---") else {
        return map;
    };
    let Some(rest) = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
    else {
        return map;
    };
    for line in rest.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim_end() == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || key.starts_with('#') {
            continue;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        map.insert(key.to_string(), value.to_string());
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a minimal valid skill bundle directory under `parent`.
    fn write_skill(parent: &Path, name: &str, body: &str) -> PathBuf {
        let dir = parent.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let md =
            format!("---\nname: {name}\ndescription: A {name} skill for tests.\n---\n\n{body}\n");
        std::fs::write(dir.join(MANIFEST_FILE), md).unwrap();
        dir
    }

    #[test]
    fn frontmatter_parses_name_and_description() {
        let raw =
            "---\nname: croit-brand-guardian\ndescription: Enforces the brand.\n---\n\n# Body\n";
        let front = parse_frontmatter(raw);
        assert_eq!(front.get("name").unwrap(), "croit-brand-guardian");
        assert_eq!(front.get("description").unwrap(), "Enforces the brand.");
    }

    #[test]
    fn prettify_name_title_cases_slug_words() {
        assert_eq!(
            prettify_name("commit-message-helper"),
            "Commit Message Helper"
        );
        assert_eq!(
            prettify_name("croit_brand.guardian"),
            "Croit Brand Guardian"
        );
        assert_eq!(prettify_name("simple"), "Simple");
    }

    #[test]
    fn title_defaults_to_prettified_name_else_honours_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        // No `title` → prettified slug.
        let d1 = write_skill(dir.path(), "commit-message-helper", "# Body");
        assert_eq!(load_skill(&d1).unwrap().title, "Commit Message Helper");
        // Explicit `title` wins.
        let d2 = dir.path().join("custom");
        std::fs::create_dir_all(&d2).unwrap();
        std::fs::write(
            d2.join(MANIFEST_FILE),
            "---\nname: custom\ntitle: My Fancy Skill\ndescription: d.\n---\nbody",
        )
        .unwrap();
        assert_eq!(load_skill(&d2).unwrap().title, "My Fancy Skill");
    }

    #[test]
    fn frontmatter_strips_surrounding_quotes_and_bom() {
        let raw = "\u{feff}---\nname: \"quoted\"\ndescription: 'single'\n---\nbody";
        let front = parse_frontmatter(raw);
        assert_eq!(front.get("name").unwrap(), "quoted");
        assert_eq!(front.get("description").unwrap(), "single");
    }

    #[test]
    fn frontmatter_value_with_colons_is_kept_whole() {
        // A description full of colons (URLs, "key: value" prose) must not be
        // truncated at the first colon — only the key/value split is.
        let raw = "---\nname: x\ndescription: See https://e/x and use a:b:c.\n---\n";
        let front = parse_frontmatter(raw);
        assert_eq!(
            front.get("description").unwrap(),
            "See https://e/x and use a:b:c."
        );
    }

    #[test]
    fn strip_frontmatter_returns_body_only() {
        let raw = "---\nname: x\ndescription: y\n---\n\n# Heading\n\nText.\n";
        assert_eq!(strip_frontmatter(raw), "# Heading\n\nText.\n");
    }

    #[test]
    fn strip_frontmatter_no_fence_returns_all() {
        let raw = "# Just a doc\n\nNo frontmatter.\n";
        assert_eq!(strip_frontmatter(raw), raw);
    }

    #[test]
    fn name_validation() {
        assert!(is_valid_name("croit-brand-guardian"));
        assert!(is_valid_name("a.b_c-1"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("has space"));
        assert!(!is_valid_name("has/slash"));
        assert!(!is_valid_name(&"x".repeat(65)));
    }

    #[test]
    fn discover_loads_a_plain_bundle_directory() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "alpha", "instructions");
        let skills = discover(dir.path()).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "alpha");
        assert_eq!(skills[0].description, "A alpha skill for tests.");
        assert_eq!(skills[0].body().unwrap().trim(), "instructions");
    }

    #[test]
    fn discover_skips_bundle_missing_required_field() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        // No `description`.
        std::fs::write(bad.join(MANIFEST_FILE), "---\nname: bad\n---\nbody").unwrap();
        write_skill(dir.path(), "good", "ok");
        let skills = discover(dir.path()).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "good");
    }

    #[test]
    fn discover_extracts_a_skill_archive() {
        let dir = tempfile::tempdir().unwrap();
        // Build `brand.skill` = a zip with `brand/SKILL.md` inside, mirroring
        // the real `croit-brand-guardian.skill` layout (top-level dir).
        let archive_path = dir.path().join("brand.skill");
        {
            let file = std::fs::File::create(&archive_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            // Stored (no compression) needs no zip feature flags.
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            use std::io::Write as _;
            zip.start_file("brand/SKILL.md", opts).unwrap();
            zip.write_all(b"---\nname: brand\ndescription: Brand rules.\n---\n\nUse purple.\n")
                .unwrap();
            zip.start_file("brand/assets/logo.svg", opts).unwrap();
            zip.write_all(b"<svg/>").unwrap();
            zip.finish().unwrap();
        }
        let skills = discover(dir.path()).unwrap();
        assert_eq!(
            skills.len(),
            1,
            "archive should yield one skill: {skills:?}"
        );
        let brand = &skills[0];
        assert_eq!(brand.name, "brand");
        assert_eq!(brand.body().unwrap().trim(), "Use purple.");
        assert_eq!(brand.files(), vec!["assets/logo.svg".to_string()]);
        assert_eq!(brand.read_file("assets/logo.svg").unwrap(), "<svg/>");
    }

    #[test]
    fn to_archive_round_trips_through_install() {
        // Build a skill on disk (SKILL.md + a nested asset)…
        let src = tempfile::tempdir().unwrap();
        let root = write_skill(src.path(), "alpha", "Body text.");
        std::fs::create_dir_all(root.join("references")).unwrap();
        std::fs::write(root.join("references/spec.md"), "SPEC").unwrap();
        let skill = load_skill(&root).unwrap();

        // …download it (zip it up)…
        let archive = skill.to_archive().unwrap();

        // …and re-install the archive into a fresh store: it must reconstruct
        // the same bundle (name, body, assets).
        let dest = tempfile::tempdir().unwrap();
        let store = SkillStore::load(dest.path().to_path_buf());
        let name = store.install_archive(&archive).unwrap();
        assert_eq!(name, "alpha");
        let reg = store.current();
        let installed = reg.get("alpha").unwrap();
        assert_eq!(installed.body().unwrap().trim(), "Body text.");
        assert_eq!(installed.files(), vec!["references/spec.md".to_string()]);
        assert_eq!(installed.read_file("references/spec.md").unwrap(), "SPEC");
    }

    #[test]
    fn read_file_is_path_jailed() {
        let dir = tempfile::tempdir().unwrap();
        let root = write_skill(dir.path(), "alpha", "x");
        std::fs::create_dir_all(root.join("references")).unwrap();
        std::fs::write(root.join("references/specs.md"), "SPEC").unwrap();
        // Plant a secret as a sibling of the bundle to attempt to escape to.
        std::fs::write(dir.path().join("secret.txt"), "TOPSECRET").unwrap();
        let skill = Skill {
            name: "alpha".into(),
            title: "Alpha".into(),
            description: "d".into(),
            root: root.clone(),
        };
        assert_eq!(skill.read_file("references/specs.md").unwrap(), "SPEC");
        assert!(matches!(
            skill.read_file("../secret.txt"),
            Err(ReadFileError::Escapes(_))
        ));
        assert!(matches!(
            skill.read_file("references/../../secret.txt"),
            Err(ReadFileError::Escapes(_))
        ));
        assert!(matches!(
            skill.read_file("nope.md"),
            Err(ReadFileError::NotFound(_))
        ));
    }

    #[test]
    fn registry_dedupes_by_name_first_wins() {
        let dir = tempfile::tempdir().unwrap();
        let a = Skill {
            name: "dup".into(),
            title: "Dup".into(),
            description: "first".into(),
            root: dir.path().join("a"),
        };
        let b = Skill {
            name: "dup".into(),
            title: "Dup".into(),
            description: "second".into(),
            root: dir.path().join("b"),
        };
        let reg = SkillRegistry::new([a, b]);
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.get("dup").unwrap().description, "first");
    }

    /// Build a `.skill` zip in memory (top dir `<name>/` holding the given
    /// files), mirroring the real archive layout.
    fn skill_zip(name: &str, files: &[(&str, &str)]) -> Vec<u8> {
        use std::io::Write as _;
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for (path, body) in files {
                zip.start_file(format!("{name}/{path}"), opts).unwrap();
                zip.write_all(body.as_bytes()).unwrap();
            }
            zip.finish().unwrap();
        }
        buf
    }

    #[test]
    fn store_install_then_remove_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::load(dir.path().to_path_buf());
        assert_eq!(store.current().len(), 0);

        let zip = skill_zip(
            "brand",
            &[
                (
                    "SKILL.md",
                    "---\nname: brand\ndescription: Brand rules.\n---\n\nUse purple.\n",
                ),
                ("assets/logo.svg", "<svg/>"),
            ],
        );
        let name = store.install_archive(&zip).unwrap();
        assert_eq!(name, "brand");

        let reg = store.current();
        assert_eq!(reg.len(), 1);
        let skill = reg.get("brand").unwrap();
        assert_eq!(skill.body().unwrap().trim(), "Use purple.");
        assert_eq!(skill.files(), vec!["assets/logo.svg".to_string()]);
        // Installed as a plain, deletable directory.
        assert!(dir.path().join("brand/SKILL.md").is_file());

        assert!(store.remove("brand").unwrap());
        assert_eq!(store.current().len(), 0);
        assert!(!dir.path().join("brand").exists());
        assert!(
            !store.remove("brand").unwrap(),
            "removing a gone skill is a clean false"
        );
    }

    #[test]
    fn store_remove_deletes_source_archive() {
        // A manually-dropped *.skill archive: remove() must delete the archive
        // itself, so a reload doesn't resurrect it from the cache.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("brand.skill"),
            skill_zip(
                "brand",
                &[("SKILL.md", "---\nname: brand\ndescription: d\n---\nbody\n")],
            ),
        )
        .unwrap();
        let store = SkillStore::load(dir.path().to_path_buf());
        assert_eq!(store.current().len(), 1, "archive should be discovered");

        assert!(store.remove("brand").unwrap());
        assert!(
            !dir.path().join("brand.skill").exists(),
            "source archive removed"
        );
        assert_eq!(store.reload(), 0, "stays gone after a re-scan");
    }

    #[test]
    fn store_install_rejects_archive_without_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::load(dir.path().to_path_buf());
        let zip = skill_zip("junk", &[("notes.txt", "no skill here")]);
        assert!(matches!(
            store.install_archive(&zip),
            Err(StoreError::NoManifest)
        ));
        assert_eq!(store.current().len(), 0);
    }
}
