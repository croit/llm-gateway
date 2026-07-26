// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Workflow manifest: the typed tool surface the model sees.
//!
//! One `manifest.toml` per workflow subdirectory under `[comfyui]
//! content_dir`. The manifest is the **only** part of the workflow the
//! LLM ever sees — anything not declared here (model paths, weight dtype,
//! sampler defaults, node graph) stays operator-curated and invisible.
//!
//! The loader parses + validates each manifest at startup and produces a
//! [`WorkflowManifest`] the rest of the comfyui module works against.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use shared::api::ToolDef;
use thiserror::Error;

/// One loaded workflow. Cheaply cloneable; held in `Arc` by the registry.
#[derive(Debug, Clone)]
pub struct WorkflowManifest {
    pub id: String,
    pub title: String,
    pub description: String,
    pub output_kind: OutputKind,
    pub output_node_id: String,
    pub output_filename_prefix: String,
    pub params: Vec<Param>,
    /// Parsed `workflow.json` content, cached at load time so the runner
    /// doesn't re-read + re-parse the file on every tool call. Cloned
    /// cheaply (it's behind `Arc`).
    pub workflow_json: Arc<Value>,
}

/// What the workflow produces. Drives how the gateway re-hosts the output
/// (image bytes → chat attachment, video → chat attachment, audio → chat
/// attachment, json → plain tool result).
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    Image,
    Video,
    Audio,
    Json,
}

impl std::fmt::Display for OutputKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OutputKind::Image => "image",
            OutputKind::Video => "video",
            OutputKind::Audio => "audio",
            OutputKind::Json => "json",
        }
        .fmt(f)
    }
}

/// One declared parameter. Maps directly to a property in the OpenAI tool
/// schema the model sees, and to a `(node_id, input_key)` target inside
/// `workflow.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Param {
    /// Placeholder name; matches `{{key}}` in workflow.json and becomes a
    /// top-level property name in the OpenAI tool schema.
    pub key: String,
    /// ComfyUI node id (string-keyed in the prompt-API document) the value
    /// is injected into.
    pub node_id: String,
    /// Input field on that node. `workflow[<node_id>].inputs[<input_key>]`
    /// is where the resolved value lands.
    pub input_key: String,
    /// Model-facing description. The model reads this verbatim; the gateway
    /// never embellishes. Must explain what changing the value does.
    pub description: String,
    /// Default value used when the model omits the parameter. `None` + no
    /// default in the manifest = no default; the gateway then enforces
    /// `required = false` by passing an empty string / zero / null.
    #[serde(default)]
    pub default: Option<Value>,
    /// Whether the model must supply a value. `false` is only meaningful
    /// when `default` is set or the schema permits null/empty.
    #[serde(default)]
    pub required: bool,
    /// Type / range / enum contract. The gateway validates incoming args
    /// against this before dispatching to ComfyUI.
    pub schema: ParamSchema,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamSchema {
    #[serde(rename = "type")]
    pub ty: ParamType,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub enum_values: Option<Vec<String>>,
    #[serde(default, rename = "max_length")]
    pub max_length: Option<u64>,
    /// When `true`, a resolved value of `-1` is replaced with a fresh random
    /// value before the workflow is submitted (the conventional ComfyUI
    /// "seed = -1 → randomize" contract). Declared per-param in the manifest
    /// so the behavior is explicit and data-driven, not inferred from the
    /// parameter's name. Only meaningful for integer params.
    #[serde(default)]
    pub randomize_on_sentinel: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ParamType {
    String,
    Integer,
    Number,
    Boolean,
    /// Chat-attachment id of the form `<turn_id>/<filename>` pointing at
    /// an image in the conversation's S3 bucket. The runner resolves it
    /// to bytes, uploads them to ComfyUI's `/upload/image`, and
    /// substitutes the returned filename into the target node input.
    ImageAttachment,
    /// Same as [`Self::ImageAttachment`] but for a video file (mp4/webm).
    VideoAttachment,
    /// Same as [`Self::ImageAttachment`] but for an audio file (wav/mp3).
    AudioAttachment,
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("reading manifest `{path}`")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing manifest `{path}`")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("manifest `{path}`: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error("parsing workflow.json at `{path}`")]
    WorkflowParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

impl Param {
    /// Build the OpenAI-tool-schema property fragment for this parameter.
    /// Combined by [`WorkflowManifest::tool_properties`] into the full
    /// `properties` object the model sees.
    pub fn schema_property(&self) -> Value {
        // Attachment-kind params are surfaced to the model as plain
        // strings — the OpenAI tool schema has no notion of file ids, and
        // the model already knows the `<turn_id>/<filename>` shape from
        // the chat replay stubs. Resolution from id → ComfyUI filename
        // is a gateway-internal concern (handled by the runner).
        let mut prop = match self.schema.ty {
            ParamType::String
            | ParamType::ImageAttachment
            | ParamType::VideoAttachment
            | ParamType::AudioAttachment => json!({ "type": "string" }),
            ParamType::Integer => json!({ "type": "integer" }),
            ParamType::Number => json!({ "type": "number" }),
            ParamType::Boolean => json!({ "type": "boolean" }),
        };
        if let Some(obj) = prop.as_object_mut() {
            obj.insert(
                "description".into(),
                Value::String(self.description.clone()),
            );
            if let Some(enum_values) = &self.schema.enum_values {
                obj.insert(
                    "enum".into(),
                    Value::Array(enum_values.iter().cloned().map(Value::String).collect()),
                );
            }
            if matches!(self.schema.ty, ParamType::Integer | ParamType::Number) {
                if let Some(min) = self.schema.min {
                    // Render integer-typed params as integers (256, not 256.0)
                    // so the schema OpenAI sees is clean and matches the type.
                    let v = if matches!(self.schema.ty, ParamType::Integer) {
                        json!(min as i64)
                    } else {
                        json!(min)
                    };
                    obj.insert("minimum".into(), v);
                }
                if let Some(max) = self.schema.max {
                    let v = if matches!(self.schema.ty, ParamType::Integer) {
                        json!(max as i64)
                    } else {
                        json!(max)
                    };
                    obj.insert("maximum".into(), v);
                }
            }
            if matches!(self.schema.ty, ParamType::String)
                && let Some(max_len) = self.schema.max_length
            {
                obj.insert("maxLength".into(), json!(max_len));
            }
        }
        prop
    }
}

impl WorkflowManifest {
    /// Full `properties` object for the OpenAI tool schema, in declared
    /// order. The model sees parameters exactly as the manifest lists them.
    pub fn tool_properties(&self) -> Value {
        let mut props = serde_json::Map::new();
        for p in &self.params {
            props.insert(p.key.clone(), p.schema_property());
        }
        Value::Object(props)
    }

    /// `required` array for the OpenAI tool schema. Stable order matching
    /// [`Self::tool_properties`].
    pub fn required_keys(&self) -> Vec<String> {
        self.params
            .iter()
            .filter(|p| p.required)
            .map(|p| p.key.clone())
            .collect()
    }

    /// The full [`ToolDef`] the model sees for this workflow, keyed by the
    /// caller-supplied tool id (`comfyui_<manifest id>`).
    pub fn tool_def(&self, tool_id: impl Into<String>) -> ToolDef {
        ToolDef::function(
            tool_id.into(),
            self.description.clone(),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": self.required_keys(),
                "properties": self.tool_properties(),
            }),
        )
    }

    /// Validate + resolve incoming args against the manifest. Applies
    /// defaults for missing optional params, enforces type / range / enum
    /// / required-ness, and returns a flat `key -> resolved value` map
    /// the runtime then injects into the workflow JSON.
    pub fn resolve_args(&self, args: &Value) -> Result<Value, ArgError> {
        let args_map = args.as_object().ok_or(ArgError::NotObject)?;
        let mut out = serde_json::Map::new();

        for p in &self.params {
            let raw = args_map.get(&p.key);
            let value = match (raw, &p.default) {
                (Some(v), _) => v.clone(),
                (None, Some(d)) => d.clone(),
                (None, None) if p.required => {
                    return Err(ArgError::MissingRequired(p.key.clone()));
                }
                (None, None) => continue,
            };
            validate_value(p, &value)?;
            out.insert(p.key.clone(), value);
        }

        Ok(Value::Object(out))
    }
}

#[derive(Debug, Error)]
pub enum ArgError {
    #[error("tool arguments must be a JSON object")]
    NotObject,
    #[error("missing required parameter `{0}`")]
    MissingRequired(String),
    #[error("parameter `{key}`: {message}")]
    BadValue { key: String, message: String },
}

fn validate_value(param: &Param, value: &Value) -> Result<(), ArgError> {
    let key = &param.key;
    let schema = &param.schema;
    match schema.ty {
        ParamType::String
        | ParamType::ImageAttachment
        | ParamType::VideoAttachment
        | ParamType::AudioAttachment => {
            let s = value.as_str().ok_or_else(|| ArgError::BadValue {
                key: key.clone(),
                message: "expected a string".into(),
            })?;
            // Attachment-kind params expect the `<turn_id>/<filename>`
            // id shape. Reject anything else early so the runner can
            // assume well-formed ids during upload.
            if matches!(
                schema.ty,
                ParamType::ImageAttachment
                    | ParamType::VideoAttachment
                    | ParamType::AudioAttachment
            ) && !s.contains('/')
            {
                return Err(ArgError::BadValue {
                    key: key.clone(),
                    message: format!(
                        "expected a chat-attachment id of the form `<turn_id>/<filename>` (got `{s}`)"
                    ),
                });
            }
            if let Some(max_len) = schema.max_length
                && s.chars().count() as u64 > max_len
            {
                return Err(ArgError::BadValue {
                    key: key.clone(),
                    message: format!("longer than the {max_len}-character limit"),
                });
            }
            if let Some(allowed) = &schema.enum_values
                && !allowed.iter().any(|a| a == s)
            {
                return Err(ArgError::BadValue {
                    key: key.clone(),
                    message: format!(
                        "must be one of: {}",
                        allowed
                            .iter()
                            .map(|a| format!("`{a}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                });
            }
        }
        ParamType::Integer => {
            let n = value.as_i64().ok_or_else(|| ArgError::BadValue {
                key: key.clone(),
                message: "expected an integer".into(),
            })?;
            check_numeric_range(key, n as f64, schema)?;
        }
        ParamType::Number => {
            let n = value.as_f64().ok_or_else(|| ArgError::BadValue {
                key: key.clone(),
                message: "expected a number".into(),
            })?;
            check_numeric_range(key, n, schema)?;
        }
        ParamType::Boolean => {
            if !value.is_boolean() {
                return Err(ArgError::BadValue {
                    key: key.clone(),
                    message: "expected a boolean".into(),
                });
            }
        }
    }
    Ok(())
}

fn check_numeric_range(key: &str, n: f64, schema: &ParamSchema) -> Result<(), ArgError> {
    if let Some(min) = schema.min
        && n < min
    {
        return Err(ArgError::BadValue {
            key: key.into(),
            message: format!("{n} is below the minimum {min}"),
        });
    }
    if let Some(max) = schema.max
        && n > max
    {
        return Err(ArgError::BadValue {
            key: key.into(),
            message: format!("{n} is above the maximum {max}"),
        });
    }
    Ok(())
}

/// Parse + validate a single manifest file. `dir` is the subdirectory
/// holding `manifest.toml` + `workflow.json`; the workflow path is resolved
/// relative to it.
pub fn load(dir: &std::path::Path) -> Result<WorkflowManifest, ManifestError> {
    let manifest_path = dir.join("manifest.toml");
    let body = std::fs::read_to_string(&manifest_path).map_err(|source| ManifestError::Read {
        path: manifest_path.clone(),
        source,
    })?;
    let raw: RawManifest = toml::from_str(&body).map_err(|source| ManifestError::Parse {
        path: manifest_path.clone(),
        source,
    })?;

    if raw.id.is_empty() {
        return Err(ManifestError::Invalid {
            path: manifest_path,
            message: "`id` must not be empty".into(),
        });
    }
    if !is_tool_id(&raw.id) {
        return Err(ManifestError::Invalid {
            path: manifest_path,
            message: format!(
                "`id = \"{}\"` is not a valid tool id (a-z, 0-9, _ only); becomes `comfyui_<id>`",
                raw.id
            ),
        });
    }
    if raw.title.is_empty() {
        return Err(ManifestError::Invalid {
            path: manifest_path,
            message: "`title` must not be empty".into(),
        });
    }
    if raw.description.is_empty() {
        return Err(ManifestError::Invalid {
            path: manifest_path,
            message: "`description` must not be empty".into(),
        });
    }
    if raw.params.is_empty() {
        return Err(ManifestError::Invalid {
            path: manifest_path,
            message: "at least one [[params]] entry is required".into(),
        });
    }
    for p in &raw.params {
        if p.key.is_empty() || p.node_id.is_empty() || p.input_key.is_empty() {
            return Err(ManifestError::Invalid {
                path: manifest_path.clone(),
                message: format!(
                    "param `{}`: `key`, `node_id`, `input_key` must all be non-empty",
                    p.key
                ),
            });
        }
        if p.description.is_empty() {
            return Err(ManifestError::Invalid {
                path: manifest_path.clone(),
                message: format!(
                    "param `{}`: `description` must not be empty — the model reads it verbatim",
                    p.key
                ),
            });
        }
    }

    let workflow_path = dir.join("workflow.json");
    if !workflow_path.exists() {
        return Err(ManifestError::Invalid {
            path: manifest_path,
            message: format!(
                "sibling `workflow.json` not found at {}",
                workflow_path.display()
            ),
        });
    }

    let duplicate_keys = find_duplicates(raw.params.iter().map(|p| p.key.as_str()));
    if !duplicate_keys.is_empty() {
        return Err(ManifestError::Invalid {
            path: manifest_path,
            message: format!(
                "duplicate `key` values in [[params]]: {}",
                duplicate_keys.join(", ")
            ),
        });
    }

    // Pre-parse workflow.json so the runner doesn't re-read the file on
    // every tool call. The parsed value is immutable and shared via Arc.
    let workflow_body =
        std::fs::read_to_string(&workflow_path).map_err(|source| ManifestError::Read {
            path: workflow_path.clone(),
            source,
        })?;
    let workflow_json: Value =
        serde_json::from_str(&workflow_body).map_err(|source| ManifestError::WorkflowParse {
            path: workflow_path.clone(),
            source,
        })?;

    Ok(WorkflowManifest {
        id: raw.id,
        title: raw.title,
        description: raw.description,
        output_kind: raw.output_kind,
        output_node_id: raw.output_node_id,
        output_filename_prefix: raw.output_filename_prefix,
        params: raw.params,
        workflow_json: Arc::new(workflow_json),
    })
}

/// Same rule as the OpenAI function-name regex (`^[a-zA-Z0-9_-]+$`) minus
/// the leading-trailing-dash ban, since `comfyui_<id>` always prefixes.
fn is_tool_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 60
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn find_duplicates<'a, I: Iterator<Item = &'a str>>(items: I) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut dups = Vec::new();
    for item in items {
        if !seen.insert(item.to_string()) {
            dups.push(item.to_string());
        }
    }
    dups
}

#[derive(Debug, Deserialize)]
struct RawManifest {
    id: String,
    title: String,
    description: String,
    output_kind: OutputKind,
    output_node_id: String,
    #[serde(default)]
    output_filename_prefix: String,
    #[serde(default)]
    params: Vec<Param>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest(id: &str, params: Vec<Param>) -> WorkflowManifest {
        WorkflowManifest {
            id: id.into(),
            title: "T".into(),
            description: "D".into(),
            output_kind: OutputKind::Image,
            output_node_id: "9".into(),
            output_filename_prefix: "comfyui".into(),
            params,
            workflow_json: Arc::new(json!({})),
        }
    }

    fn str_param(key: &str, required: bool) -> Param {
        Param {
            key: key.into(),
            node_id: "1".into(),
            input_key: "v".into(),
            description: "d".into(),
            default: None,
            required,
            schema: ParamSchema {
                ty: ParamType::String,
                min: None,
                max: None,
                enum_values: None,
                max_length: Some(100),
                randomize_on_sentinel: false,
            },
        }
    }

    fn int_param(key: &str, min: Option<f64>, max: Option<f64>) -> Param {
        Param {
            key: key.into(),
            node_id: "1".into(),
            input_key: "v".into(),
            description: "d".into(),
            default: None,
            required: true,
            schema: ParamSchema {
                ty: ParamType::Integer,
                min,
                max,
                enum_values: None,
                max_length: None,
                randomize_on_sentinel: false,
            },
        }
    }

    fn enum_param(key: &str, values: &[&str]) -> Param {
        Param {
            key: key.into(),
            node_id: "1".into(),
            input_key: "v".into(),
            description: "d".into(),
            default: None,
            required: true,
            schema: ParamSchema {
                ty: ParamType::String,
                min: None,
                max: None,
                enum_values: Some(values.iter().map(|s| s.to_string()).collect()),
                max_length: None,
                randomize_on_sentinel: false,
            },
        }
    }

    #[test]
    fn schema_property_for_string_includes_description_and_maxlength() {
        let p = str_param("prompt", true);
        let prop = p.schema_property();
        assert_eq!(prop["type"], "string");
        assert_eq!(prop["description"], "d");
        assert_eq!(prop["maxLength"], 100);
    }

    #[test]
    fn schema_property_for_int_includes_min_max() {
        let p = int_param("width", Some(256.0), Some(2048.0));
        let prop = p.schema_property();
        assert_eq!(prop["type"], "integer");
        assert_eq!(prop["minimum"], 256);
        assert_eq!(prop["maximum"], 2048);
    }

    #[test]
    fn schema_property_for_enum_includes_choices() {
        let p = enum_param("sampler", &["euler", "dpmpp_2m"]);
        let prop = p.schema_property();
        assert_eq!(prop["enum"], json!(["euler", "dpmpp_2m"]));
    }

    #[test]
    fn resolve_args_applies_defaults_for_missing_optionals() {
        let mut p = str_param("negative_prompt", false);
        p.default = Some(json!(""));
        let m = manifest("text_to_image", vec![p]);
        let resolved = m.resolve_args(&json!({})).expect("ok");
        assert_eq!(resolved["negative_prompt"], json!(""));
    }

    #[test]
    fn resolve_args_errors_on_missing_required() {
        let m = manifest("text_to_image", vec![str_param("prompt", true)]);
        let err = m.resolve_args(&json!({})).unwrap_err();
        assert!(matches!(err, ArgError::MissingRequired(_)));
    }

    #[test]
    fn resolve_args_enforces_int_range() {
        let m = manifest("x", vec![int_param("width", Some(256.0), Some(2048.0))]);
        let err = m.resolve_args(&json!({ "width": 100 })).unwrap_err();
        assert!(matches!(err, ArgError::BadValue { .. }));
        assert!(format!("{err}").contains("below the minimum"));
    }

    #[test]
    fn resolve_args_enforces_enum() {
        let m = manifest("x", vec![enum_param("sampler", &["euler", "dpmpp_2m"])]);
        let err = m.resolve_args(&json!({ "sampler": "lala" })).unwrap_err();
        assert!(matches!(err, ArgError::BadValue { .. }));
        assert!(format!("{err}").contains("must be one of"));
    }

    #[test]
    fn resolve_args_rejects_wrong_type() {
        let m = manifest("x", vec![int_param("width", None, None)]);
        let err = m.resolve_args(&json!({ "width": "lots" })).unwrap_err();
        assert!(matches!(err, ArgError::BadValue { .. }));
        assert!(format!("{err}").contains("expected an integer"));
    }

    #[test]
    fn is_tool_id_rejects_uppercase_and_dashes() {
        assert!(is_tool_id("text_to_image"));
        assert!(!is_tool_id("text-to-image"));
        assert!(!is_tool_id("TextToImage"));
    }

    #[test]
    fn load_rejects_empty_id() {
        let dir = tempdir_with_manifest(
            "id = ''\ntitle = \"T\"\ndescription = \"D\"\noutput_kind = \"image\"\noutput_node_id = \"9\"\n",
        );
        let err = load(&dir).unwrap_err();
        assert!(matches!(err, ManifestError::Invalid { .. }));
        assert!(format!("{err}").contains("`id` must not be empty"));
    }

    #[test]
    fn load_rejects_param_with_empty_description() {
        let toml = r#"
id = "x"
title = "T"
description = "D"
output_kind = "image"
output_node_id = "9"

[[params]]
key = "prompt"
node_id = "6"
input_key = "text"
required = true
description = ""

[params.schema]
type = "string"
"#;
        let dir = tempdir_with_manifest(toml);
        let err = load(&dir).unwrap_err();
        assert!(matches!(err, ManifestError::Invalid { .. }));
        assert!(format!("{err}").contains("description"));
    }

    #[test]
    fn load_rejects_missing_workflow_json() {
        let toml = r#"
id = "x"
title = "T"
description = "D"
output_kind = "image"
output_node_id = "9"

[[params]]
key = "prompt"
node_id = "6"
input_key = "text"
required = true
description = "what to draw"

[params.schema]
type = "string"
"#;
        let dir = tempdir_with_manifest(toml);
        let err = load(&dir).unwrap_err();
        assert!(matches!(err, ManifestError::Invalid { .. }));
        assert!(format!("{err}").contains("workflow.json"));
    }

    #[test]
    fn load_rejects_duplicate_param_keys() {
        let toml = r#"
id = "x"
title = "T"
description = "D"
output_kind = "image"
output_node_id = "9"

[[params]]
key = "prompt"
node_id = "6"
input_key = "a"
required = true
description = "d"

[params.schema]
type = "string"

[[params]]
key = "prompt"
node_id = "6"
input_key = "b"
required = true
description = "d"

[params.schema]
type = "string"
"#;
        let dir = tempdir_with_manifest(toml);
        // Workflow.json exists this time so we get past that check.
        std::fs::write(dir.join("workflow.json"), "{}").unwrap();
        let err = load(&dir).unwrap_err();
        assert!(matches!(err, ManifestError::Invalid { .. }));
        assert!(format!("{err}").contains("duplicate"));
    }

    fn tempdir_with_manifest(toml: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("comfyui-manifest-test-{n}-{}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).ok();
        }
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.toml"), toml).unwrap();
        dir
    }
}
