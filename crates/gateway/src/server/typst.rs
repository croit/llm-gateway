// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Typst-templated document generation.
//!
//! Operators drop one subdirectory per template under
//! `[typst] templates_dir`. Each subdir holds:
//!
//! - `template.typ`  — the typst source the CLI compiles
//! - `template.toml` — manifest describing the tool the model sees:
//!   id (becomes the tool name suffix), description (goes into the
//!   schema), output_basename (filename prefix for the 3 uploads),
//!   and a list of input fields (name, type, required, description)
//!   the model fills in
//! - any co-located assets (logos, fonts, fragments) the template
//!   reads. The typst compile is invoked with `--root` pointing at
//!   the subdir, so the template can `include "logo.svg"` without
//!   the gateway needing to know about it
//!
//! At gateway startup [`discover_templates`] walks the directory and
//! returns one [`Template`] per valid subdir; main wraps each one in
//! a `TypstRenderTool` and registers it. No hot-reload — restart to
//! pick up new templates.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;
use tokio::process::Command;

/// Per-call wall-clock limit on the typst compile. Keeps a runaway
/// template (or one trying to render a huge document) from holding
/// up the chat-stream loop forever.
const COMPILE_TIMEOUT: Duration = Duration::from_secs(30);

/// Default DPI for the model-facing PNG preview. 144 matches the
/// typst CLI default; gives a readable thumbnail at chat-bubble
/// size without blowing up the base64 payload that ships in the
/// tool result.
const PNG_PPI: u32 = 144;

/// Hard cap on the rendered PDF bytes. A 25 MB ceiling matches the
/// 30 s compile timeout in spirit — if we're producing more than
/// this, the template is probably embedding video / huge images and
/// should be reworked instead of round-tripping through chat.
const MAX_PDF_BYTES: usize = 25 * 1024 * 1024;

/// One template the gateway can render. Held in `Arc` inside each
/// tool instance so cloning the registry stays cheap.
#[derive(Debug, Clone)]
pub struct Template {
    /// Stable id; becomes the tool name `typst_<id>`.
    pub id: String,
    /// What this template produces, in plain English. Goes into the
    /// tool schema's `description` so the model picks the right one.
    pub description: String,
    /// Filename prefix for the three uploads (`<basename>.pdf`,
    /// `<basename>.png`, `<basename>.typ`). Defaults to `id`.
    pub output_basename: String,
    /// Declared input fields. Map directly into the tool's JSON
    /// schema `properties`.
    pub fields: Vec<Field>,
    /// Path to the directory holding `template.typ` + assets. Used
    /// as `--root` so the template can read sibling files.
    pub root: PathBuf,
    /// Filename inside `root` that the compile points at. Almost
    /// always `template.typ` — the manifest can override it for
    /// multi-entrypoint templates.
    pub source_file: String,
}

/// One field the model fills in. The JSON-schema type maps to a
/// stringified `--input k=v` pair the typst template reads via
/// `sys.inputs.<name>`. Numeric / boolean fields are still passed
/// to typst as strings (typst's `sys.inputs` is always
/// `dict<str, str>`); the template can `int(sys.inputs.foo)` if it
/// wants a number. We carry the type in the JSON schema so the
/// model produces something parseable rather than free-form text.
#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub ty: FieldType,
    pub required: bool,
    pub description: String,
    /// Request-context value to fall back to when the model omits this
    /// field (or leaves it blank). Lets a manifest say "fill this from
    /// the signed-in user" so the *tool* guarantees the value rather
    /// than relying on the model to copy it from the context message.
    pub default_from: Option<DefaultSource>,
}

/// A signed-in-user value a [`Field`] can default to. Named in the
/// manifest as `default_from = "user.name"` / `"user.email"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultSource {
    /// The signed-in user's display name (`users.name`).
    UserName,
    /// The signed-in user's email (`users.email`).
    UserEmail,
}

impl DefaultSource {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "user.name" => Some(Self::UserName),
            "user.email" => Some(Self::UserEmail),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    String,
    Integer,
    Boolean,
}

impl FieldType {
    pub fn json_schema_name(&self) -> &'static str {
        match self {
            FieldType::String => "string",
            FieldType::Integer => "integer",
            FieldType::Boolean => "boolean",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    id: String,
    description: String,
    #[serde(default)]
    output_basename: Option<String>,
    #[serde(default = "default_source_file")]
    source_file: String,
    #[serde(default, rename = "field")]
    fields: Vec<ManifestField>,
}

fn default_source_file() -> String {
    "template.typ".to_string()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestField {
    name: String,
    #[serde(default = "default_field_type")]
    ty: String,
    #[serde(default)]
    required: bool,
    description: String,
    /// Optional: name a request-context value to fill in when the model
    /// omits this field. Currently `"user.name"` or `"user.email"`.
    #[serde(default)]
    default_from: Option<String>,
}

fn default_field_type() -> String {
    "string".to_string()
}

#[derive(Debug, Error)]
pub enum DiscoverError {
    #[error("templates_dir `{0}` not readable")]
    DirRead(PathBuf, #[source] std::io::Error),
    #[error("manifest `{path}` parse")]
    ManifestParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("manifest `{path}` read")]
    ManifestRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("manifest `{0}` field type `{1}` unsupported (use string/integer/boolean)")]
    BadFieldType(PathBuf, String),
    #[error(
        "manifest `{0}` field default_from `{1}` unknown (use \"user.name\" or \"user.email\")"
    )]
    BadDefaultFrom(PathBuf, String),
    #[error("manifest `{path}` references source_file `{file}` which doesn't exist")]
    MissingSource { path: PathBuf, file: String },
    #[error("template id `{0}` is not a valid identifier (alnum + underscore only)")]
    BadId(String),
}

/// Walk `dir` and return one [`Template`] per subdirectory that
/// contains a parseable `template.toml`. Subdirs without a manifest
/// are silently skipped (lets operators stage in-progress work);
/// subdirs with a broken manifest log a warning and are skipped (so
/// one bad template doesn't take the whole gateway down).
pub fn discover_templates(dir: &Path) -> Result<Vec<Template>, DiscoverError> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| DiscoverError::DirRead(dir.to_path_buf(), e))?;
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest_path = path.join("template.toml");
        if !manifest_path.is_file() {
            continue;
        }
        match load_template(&path, &manifest_path) {
            Ok(t) => out.push(t),
            Err(err) => {
                tracing::warn!(error = %err, dir = %path.display(), "skipping typst template");
            }
        }
    }
    Ok(out)
}

fn load_template(root: &Path, manifest_path: &Path) -> Result<Template, DiscoverError> {
    let raw = std::fs::read_to_string(manifest_path).map_err(|e| DiscoverError::ManifestRead {
        path: manifest_path.to_path_buf(),
        source: e,
    })?;
    let manifest: Manifest = toml::from_str(&raw).map_err(|e| DiscoverError::ManifestParse {
        path: manifest_path.to_path_buf(),
        source: e,
    })?;
    validate_id(&manifest.id)?;
    let source_path = root.join(&manifest.source_file);
    if !source_path.is_file() {
        return Err(DiscoverError::MissingSource {
            path: manifest_path.to_path_buf(),
            file: manifest.source_file,
        });
    }
    let mut fields = Vec::with_capacity(manifest.fields.len());
    for f in manifest.fields {
        let ty = match f.ty.as_str() {
            "string" => FieldType::String,
            "integer" => FieldType::Integer,
            "boolean" => FieldType::Boolean,
            other => {
                return Err(DiscoverError::BadFieldType(
                    manifest_path.to_path_buf(),
                    other.to_string(),
                ));
            }
        };
        let default_from = match f.default_from.as_deref() {
            None => None,
            Some(s) => Some(DefaultSource::parse(s).ok_or_else(|| {
                DiscoverError::BadDefaultFrom(manifest_path.to_path_buf(), s.to_string())
            })?),
        };
        fields.push(Field {
            name: f.name,
            ty,
            required: f.required,
            description: f.description,
            default_from,
        });
    }
    let output_basename = manifest
        .output_basename
        .unwrap_or_else(|| manifest.id.clone());
    Ok(Template {
        id: manifest.id,
        description: manifest.description,
        output_basename,
        fields,
        root: root.to_path_buf(),
        source_file: manifest.source_file,
    })
}

fn validate_id(id: &str) -> Result<(), DiscoverError> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(DiscoverError::BadId(id.to_string()));
    }
    Ok(())
}

/// Bytes for one rendered document. Carries everything the tool
/// needs to splice three attachments into the assistant turn.
#[derive(Debug)]
pub struct Rendered {
    pub pdf: Vec<u8>,
    pub png: Vec<u8>,
    pub source: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum CompileError {
    #[error("typst CLI not installed (need `typst` on PATH)")]
    BinaryNotFound,
    #[error("typst compile failed: {0}")]
    Failed(String),
    #[error("typst compile timed out after {}s", COMPILE_TIMEOUT.as_secs())]
    Timeout,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("rendered PDF is {0} bytes; max {max} bytes", max = MAX_PDF_BYTES)]
    TooLarge(usize),
}

/// Run typst against `template` with the model-provided `inputs`.
/// Returns the three byte blobs we upload to S3. Stdout is
/// discarded; on non-zero exit the captured stderr is returned to
/// the caller so the model can see syntax errors and iterate.
pub async fn compile(
    template: &Template,
    inputs: &[(String, String)],
) -> Result<Rendered, CompileError> {
    let workdir = tempfile::tempdir()?;
    let pdf_path = workdir
        .path()
        .join(format!("{}.pdf", template.output_basename));
    // Page-templated PNG path because typst emits one file per page
    // for PNG output. We pin `--pages 1` so only the cover page is
    // rendered for the preview — the full PDF carries everything.
    let png_path = workdir
        .path()
        .join(format!("{}-page-{{p}}.png", template.output_basename));
    let png_first = workdir
        .path()
        .join(format!("{}-page-1.png", template.output_basename));

    let source_path = template.root.join(&template.source_file);
    // A template may bundle its own fonts in a `fonts/` subdir (the corporate
    // letter ships Urbanist there); add it to typst's font search so brand
    // typefaces render. Missing dir → omit the flag and use system/bundled
    // fonts. `--font-path` augments, it doesn't replace, the font set.
    let font_dir = template.root.join("fonts");
    let has_fonts = font_dir.is_dir();

    // PDF pass.
    let mut pdf_cmd = Command::new("typst");
    pdf_cmd.arg("compile").arg("--root").arg(&template.root);
    if has_fonts {
        pdf_cmd.arg("--font-path").arg(&font_dir);
    }
    pdf_cmd.arg(&source_path).arg(&pdf_path);
    for (k, v) in inputs {
        pdf_cmd.arg("--input").arg(format!("{k}={v}"));
    }
    run_typst(pdf_cmd).await?;

    // PNG pass (first page only) for the model preview.
    let mut png_cmd = Command::new("typst");
    png_cmd.arg("compile").arg("--root").arg(&template.root);
    if has_fonts {
        png_cmd.arg("--font-path").arg(&font_dir);
    }
    png_cmd
        .arg("--format")
        .arg("png")
        .arg("--ppi")
        .arg(PNG_PPI.to_string())
        .arg("--pages")
        .arg("1")
        .arg(&source_path)
        .arg(&png_path);
    for (k, v) in inputs {
        png_cmd.arg("--input").arg(format!("{k}={v}"));
    }
    run_typst(png_cmd).await?;

    let pdf = tokio::fs::read(&pdf_path).await?;
    if pdf.len() > MAX_PDF_BYTES {
        return Err(CompileError::TooLarge(pdf.len()));
    }
    let png = tokio::fs::read(&png_first).await?;
    let source = tokio::fs::read(&source_path).await?;
    Ok(Rendered { pdf, png, source })
}

/// Spawn `cmd` with stderr captured, enforce [`COMPILE_TIMEOUT`],
/// surface a clean error type per failure shape. Stdin is closed so
/// typst doesn't ever try to read from us.
async fn run_typst(mut cmd: Command) -> Result<(), CompileError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(CompileError::BinaryNotFound);
        }
        Err(err) => return Err(CompileError::Io(err)),
    };
    let out = match tokio::time::timeout(COMPILE_TIMEOUT, child.wait_with_output()).await {
        Ok(r) => r?,
        Err(_) => return Err(CompileError::Timeout),
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        return Err(CompileError::Failed(stderr));
    }
    Ok(())
}

/// Compile a complete, self-contained Typst `source` string to a PDF.
///
/// Unlike [`compile`], there's no manifest, no `--input` plumbing and no
/// PNG/source side-products: the caller (chat export) hands us a full
/// document it generated and just wants the bytes back. The source is
/// written into a fresh tempdir which doubles as `--root`, so it can't
/// read anything outside it. Typst's bundled fonts cover the export
/// template, so no `--font-path` is needed.
pub async fn compile_source(source: &str) -> Result<Vec<u8>, CompileError> {
    let workdir = tempfile::tempdir()?;
    let src_path = workdir.path().join("export.typ");
    let pdf_path = workdir.path().join("export.pdf");
    tokio::fs::write(&src_path, source).await?;

    let mut cmd = Command::new("typst");
    cmd.arg("compile")
        .arg("--root")
        .arg(workdir.path())
        .arg(&src_path)
        .arg(&pdf_path);
    run_typst(cmd).await?;

    let pdf = tokio::fs::read(&pdf_path).await?;
    if pdf.len() > MAX_PDF_BYTES {
        return Err(CompileError::TooLarge(pdf.len()));
    }
    Ok(pdf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: write a minimal valid template directory under `dir`
    /// with the given `id` and return its path. Templates produced
    /// here don't need real fonts/assets — discovery only parses
    /// the manifest and confirms `template.typ` exists.
    fn write_stub_template(dir: &Path, id: &str, extra_toml: &str) -> PathBuf {
        let tdir = dir.join(id);
        std::fs::create_dir_all(&tdir).unwrap();
        std::fs::write(tdir.join("template.typ"), "= Stub\n").unwrap();
        let toml = format!(
            r#"id = "{id}"
description = "Stub template for tests"
{extra_toml}
"#
        );
        std::fs::write(tdir.join("template.toml"), toml).unwrap();
        tdir
    }

    #[tokio::test]
    async fn compile_source_produces_a_pdf() {
        // Exercises the chat-export path end to end against the real
        // typst CLI. Skips (rather than fails) when typst isn't on PATH
        // so the suite still passes on a machine without it.
        let source = "#set page(width: 6cm, height: 4cm)\n= Export\n\nHello, world.\n";
        match compile_source(source).await {
            Ok(pdf) => assert!(pdf.starts_with(b"%PDF"), "output is not a PDF"),
            Err(CompileError::BinaryNotFound) => {
                eprintln!("skipping: typst not installed");
            }
            Err(err) => panic!("compile_source failed: {err}"),
        }
    }

    #[test]
    fn discover_returns_one_per_manifest() {
        let dir = tempfile::tempdir().unwrap();
        write_stub_template(dir.path(), "alpha", "");
        write_stub_template(dir.path(), "beta", "");
        let mut templates = discover_templates(dir.path()).unwrap();
        templates.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(
            templates.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
    }

    #[test]
    fn discover_skips_subdirs_without_manifest() {
        let dir = tempfile::tempdir().unwrap();
        write_stub_template(dir.path(), "real", "");
        std::fs::create_dir_all(dir.path().join("draft")).unwrap();
        std::fs::write(dir.path().join("draft").join("notes.md"), "wip").unwrap();
        let templates = discover_templates(dir.path()).unwrap();
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].id, "real");
    }

    #[test]
    fn discover_skips_template_with_broken_manifest() {
        // One good + one corrupt manifest: the good one still loads,
        // the bad one is logged and skipped (no panic, no top-level
        // error — the gateway must boot regardless).
        let dir = tempfile::tempdir().unwrap();
        write_stub_template(dir.path(), "good", "");
        let bad = dir.path().join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("template.typ"), "= bad\n").unwrap();
        std::fs::write(bad.join("template.toml"), "this is not toml = = =").unwrap();
        let templates = discover_templates(dir.path()).unwrap();
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].id, "good");
    }

    #[test]
    fn discover_rejects_unsupported_field_type() {
        let dir = tempfile::tempdir().unwrap();
        let tdir = write_stub_template(
            dir.path(),
            "bad",
            r#"
[[field]]
name = "x"
ty = "blob"
description = "nope"
"#,
        );
        let manifest = tdir.join("template.toml");
        // Direct load_template call — discover_templates would log+skip.
        let err = load_template(&tdir, &manifest).unwrap_err();
        assert!(matches!(err, DiscoverError::BadFieldType(_, ref s) if s == "blob"));
    }

    #[test]
    fn discover_rejects_bad_id() {
        let dir = tempfile::tempdir().unwrap();
        let tdir = write_stub_template(dir.path(), "valid_dir", r#""#);
        // Overwrite the manifest with an id that contains a hyphen.
        std::fs::write(
            tdir.join("template.toml"),
            r#"id = "has-hyphen"
description = "x"
"#,
        )
        .unwrap();
        let err = load_template(&tdir, &tdir.join("template.toml")).unwrap_err();
        assert!(matches!(err, DiscoverError::BadId(_)));
    }

    #[test]
    fn manifest_field_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        write_stub_template(
            dir.path(),
            "letter",
            r#"output_basename = "outgoing"

[[field]]
name = "recipient"
description = "Full name"
required = true

[[field]]
name = "count"
ty = "integer"
description = "Page count override"
"#,
        );
        let templates = discover_templates(dir.path()).unwrap();
        let t = &templates[0];
        assert_eq!(t.id, "letter");
        assert_eq!(t.output_basename, "outgoing");
        assert_eq!(t.fields.len(), 2);
        assert_eq!(t.fields[0].name, "recipient");
        assert!(t.fields[0].required);
        assert_eq!(t.fields[0].ty, FieldType::String);
        assert_eq!(t.fields[1].ty, FieldType::Integer);
        assert!(!t.fields[1].required);
    }

    // Compile-time integration tests live in `tests/typst_render.rs`
    // (the integration test path) so we don't force `typst` onto the
    // unit-test machine. Locally they run when the binary is on
    // PATH; in CI the Dockerfile installs it before `mise run test`.
}
