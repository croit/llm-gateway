// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Drift guard for the whole example workflow catalog under
//! `examples/comfyui-workflows/`.
//!
//! Every workflow there is copied verbatim into an operator's `content_dir`,
//! and the failure mode is silent: a mismatch between `manifest.toml` and
//! `workflow.json` doesn't surface until the model calls the tool and ComfyUI
//! rejects the whole prompt — as a 400 with a validation blob, days later.
//! This has already happened once, when a workflow derived from another kept
//! the original's `{{image_a}}` placeholder while the manifest declared
//! `{{image}}`.
//!
//! So for each directory this asserts the two files agree:
//!
//!   * every `{{placeholder}}` in the graph is declared as a param,
//!   * every declared param's `node_id` / `input_key` exists in the graph and
//!     is the placeholder's actual home,
//!   * `output_node_id` names a real node,
//!   * resolving with defaults only leaves no `{{…}}` behind (the leak that
//!     reaches ComfyUI as a literal placeholder).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use gateway_features::server::comfyui::manifest;
use serde_json::Value;

fn catalog_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/comfyui-workflows")
}

/// Every `{{name}}` occurring anywhere in the graph.
fn placeholders(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::String(s) => {
            let mut rest = s.as_str();
            while let Some(start) = rest.find("{{") {
                let after = &rest[start + 2..];
                let Some(end) = after.find("}}") else { break };
                out.insert(after[..end].trim().to_string());
                rest = &after[end + 2..];
            }
        }
        Value::Array(items) => items.iter().for_each(|i| placeholders(i, out)),
        Value::Object(map) => map.values().for_each(|i| placeholders(i, out)),
        _ => {}
    }
}

/// Directories holding a `manifest.toml`, sorted for a stable failure order.
fn workflow_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(catalog_dir())
        .expect("examples/comfyui-workflows exists")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join("manifest.toml").is_file())
        .collect();
    dirs.sort();
    dirs
}

fn name(dir: &Path) -> String {
    dir.file_name().unwrap_or_default().to_string_lossy().into()
}

#[test]
fn every_example_workflow_loads() {
    let dirs = workflow_dirs();
    assert!(dirs.len() >= 8, "catalog looks truncated: {dirs:?}");
    for dir in dirs {
        manifest::load(&dir).unwrap_or_else(|e| panic!("{}: {e}", name(&dir)));
    }
}

#[test]
fn manifest_params_and_graph_placeholders_agree() {
    for dir in workflow_dirs() {
        let m = manifest::load(&dir).unwrap_or_else(|e| panic!("{}: {e}", name(&dir)));
        let id = name(&dir);

        let mut found = BTreeSet::new();
        placeholders(&m.workflow_json, &mut found);
        let declared: BTreeSet<String> = m.params.iter().map(|p| p.key.clone()).collect();

        let orphaned: Vec<&String> = found.difference(&declared).collect();
        assert!(
            orphaned.is_empty(),
            "{id}: workflow.json has placeholder(s) {orphaned:?} that manifest.toml \
             declares no param for — they would reach ComfyUI as literal text"
        );
        let unused: Vec<&String> = declared.difference(&found).collect();
        assert!(
            unused.is_empty(),
            "{id}: manifest.toml declares param(s) {unused:?} that appear nowhere in \
             workflow.json — the model can set them and nothing happens"
        );
    }
}

#[test]
fn every_param_targets_a_real_node_input() {
    for dir in workflow_dirs() {
        let m = manifest::load(&dir).unwrap_or_else(|e| panic!("{}: {e}", name(&dir)));
        let id = name(&dir);
        let graph = m.workflow_json.as_object().expect("graph is an object");

        for p in &m.params {
            let node = graph.get(&p.node_id).unwrap_or_else(|| {
                panic!(
                    "{id}: param `{}` targets node `{}` which does not exist",
                    p.key, p.node_id
                )
            });
            let inputs = node
                .get("inputs")
                .and_then(Value::as_object)
                .unwrap_or_else(|| panic!("{id}: node `{}` has no inputs", p.node_id));
            let slot = inputs.get(&p.input_key).unwrap_or_else(|| {
                panic!(
                    "{id}: param `{}` targets `{}`.inputs.{} which does not exist",
                    p.key, p.node_id, p.input_key
                )
            });
            // The targeted slot must be the placeholder's home, or the
            // injection lands somewhere the graph never reads.
            if let Some(text) = slot.as_str()
                && text.contains("{{")
            {
                assert!(
                    text.contains(&format!("{{{{{}}}}}", p.key)),
                    "{id}: param `{}` writes to `{}`.inputs.{} but that slot holds `{text}`",
                    p.key,
                    p.node_id,
                    p.input_key
                );
            }
        }

        assert!(
            graph.contains_key(&m.output_node_id),
            "{id}: output_node_id `{}` is not a node in workflow.json",
            m.output_node_id
        );
    }
}

#[test]
fn defaults_alone_leave_no_placeholder_behind() {
    // What a first, minimal tool call looks like: required params filled with
    // a plausible value, everything else left to its default. Nothing may
    // survive as `{{…}}` — ComfyUI rejects the whole prompt if it does, and
    // that is the single most common way one of these workflows breaks.
    for dir in workflow_dirs() {
        let m = manifest::load(&dir).unwrap_or_else(|e| panic!("{}: {e}", name(&dir)));
        let id = name(&dir);

        let mut args = serde_json::Map::new();
        for p in m.params.iter().filter(|p| p.required) {
            args.insert(p.key.clone(), sample_value(p));
        }
        let resolved = m
            .resolve_args(&Value::Object(args))
            .unwrap_or_else(|e| panic!("{id}: resolving defaults failed: {e}"));

        let mut missing = BTreeSet::new();
        placeholders(&m.workflow_json, &mut missing);
        for key in &missing {
            assert!(
                resolved.get(key).is_some(),
                "{id}: `{{{{{key}}}}}` is left unresolved by a defaults-only call — \
                 give the param a default or mark it required"
            );
        }
    }
}

/// A value that satisfies `p`'s schema, for the defaults-only smoke call.
fn sample_value(p: &manifest::Param) -> Value {
    use manifest::ParamType;
    match p.schema.ty {
        ParamType::Integer => Value::from(p.schema.min.unwrap_or(1.0).max(1.0) as i64),
        ParamType::Number => Value::from(p.schema.min.unwrap_or(1.0).max(1.0)),
        ParamType::Boolean => Value::Bool(true),
        ParamType::String => match &p.schema.enum_values {
            Some(values) => Value::from(values[0].clone()),
            None => Value::from("x"),
        },
        // Attachment ids are `<turn_id>/<filename>`-shaped strings.
        ParamType::ImageAttachment => Value::from("turn/pic.png"),
        ParamType::VideoAttachment => Value::from("turn/clip.mp4"),
        ParamType::AudioAttachment => Value::from("turn/voice.wav"),
    }
}
