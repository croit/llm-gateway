// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The tool families whose membership depends on an operator setting.
//!
//! These live in the library rather than in `main.rs` so the boot path, the
//! settings reload and the test harness all install the *same* closure. Naming
//! the concrete tool types needs `gateway-tools`, which `gateway-runtime` (where
//! the state lives) deliberately does not depend on — hence a callback handed in
//! from this crate, which can see everything.

use gateway_runtime::server::state::ToolFamilyBuilder;

/// The typst tool family: one render, edit and read tool per discovered
/// template, plus a PPTX export for templates that opt into it.
///
/// Returned as a closure so the boot path and [`AppState::reload_settings`]
/// call the *same* code — the reason `typst.templates_dir` and
/// `typst.enabled` take effect without a restart. It lives here because
/// naming these tool types needs `gateway-tools`, which `gateway-runtime`
/// (where the state lives) deliberately does not depend on.
///
/// Discovery failures are a warning and an empty family: a broken templates
/// directory costs the typst tools, not the gateway.
pub fn typst() -> ToolFamilyBuilder {
    std::sync::Arc::new(|config, surface| {
        let Some(cfg) = config.typst.as_ref() else {
            return Vec::new();
        };
        let sandbox = surface.sandbox_client.clone();
        let templates =
            match gateway_features::server::typst::discover_templates(&cfg.templates_dir) {
                Ok(t) => t,
                Err(err) => {
                    tracing::warn!(
                        error = %err, dir = %cfg.templates_dir.display(),
                        "skipping typst tools — discovery failed"
                    );
                    return Vec::new();
                }
            };
        let mut family: Vec<std::sync::Arc<dyn gateway_runtime::server::tools::Tool>> = Vec::new();
        for t in templates {
            let t = std::sync::Arc::new(t);
            let pptx = t.pptx.is_some();
            family.push(std::sync::Arc::new(
                gateway_tools::typst_render::TypstRenderTool::new(t.clone(), sandbox.clone()),
            ));
            family.push(std::sync::Arc::new(
                gateway_tools::typst_render::TypstEditTool::new(t.clone(), sandbox.clone()),
            ));
            family.push(std::sync::Arc::new(
                gateway_tools::typst_render::TypstReadTool::new(t.clone()),
            ));
            // PPTX export renders through the sandbox, so it only exists when
            // the sandbox does — the same rule the boot path applied.
            if let (true, Some(sb)) = (pptx, sandbox.clone()) {
                family.push(std::sync::Arc::new(
                    gateway_tools::typst_render::TypstPptxTool::new(t.clone(), sb),
                ));
            }
        }
        tracing::info!(tools = family.len(), "built the typst tool family");
        family
    })
}
