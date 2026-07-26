// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `read_skill` — the model's entry point into an Agent Skill.
//!
//! The chat system message advertises every skill the caller's roles grant,
//! as `name: description` (see `openai_driver::build_request_context`). That
//! listing is cheap metadata; the actual instructions live in the bundle.
//! This tool is how the model pulls them, progressive-disclosure style:
//!
//!   - `read_skill(name)` → the `SKILL.md` body (the instructions) plus a
//!     listing of the bundle's other files, so the model knows what it can
//!     read next.
//!   - `read_skill(name, path)` → one referenced file (`references/…`,
//!     `assets/…`) — e.g. the SVG logo the model inlines into an HTML answer.
//!
//! RBAC is enforced here, not in the schema: the tool holds the same
//! `Resolver` the rest of the gateway uses and narrows the loaded
//! [`SkillRegistry`] to what the caller's roles permit. Asking for a skill
//! that isn't loaded *or* isn't permitted gets the same "unknown skill"
//! answer — we don't leak which skills exist to roles that can't use them.
//!
//! Unlike `enable_tools`, this needs no chat session: it only reads files,
//! so it works on the proxy paths too (where it's merged into the tools
//! list like any other tool).

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use shared::api::ToolDef;

use super::{Tool, ToolContext, ToolError, ToolFuture};
use crate::server::rbac::Resolver;
use crate::server::skills::{SkillRegistry, SkillStore, UserSkillStore, combined_registry};

/// Tool id — also the OpenAI function name and the `/tools` toggle key.
pub const READ_SKILL_ID: &str = "read_skill";

pub struct ReadSkill {
    store: Arc<SkillStore>,
    user_store: Arc<UserSkillStore>,
    rbac: Arc<Resolver>,
}

impl ReadSkill {
    pub fn new(
        store: Arc<SkillStore>,
        user_store: Arc<UserSkillStore>,
        rbac: Arc<Resolver>,
    ) -> Self {
        Self {
            store,
            user_store,
            rbac,
        }
    }

    /// The registry to resolve a requested skill name against: the caller's
    /// private skills overlaid on the global operator ones (private shadows
    /// global). Read live so an upload/edit is usable without a restart.
    fn registry_for(&self, ctx: &ToolContext) -> SkillRegistry {
        let private = self.user_store.registry_for(&ctx.user_id);
        combined_registry(&self.store.current(), &private)
    }

    /// Skill names the caller may load: their roles' global grants, plus every
    /// private skill they own (ownership is the grant). The single
    /// authorization gate for this tool — a name absent here is refused with
    /// the same "unknown" answer as a non-existent one.
    fn allowed_for(&self, ctx: &ToolContext) -> Vec<String> {
        let role_ids = self.rbac.role_ids_for(&ctx.roles);
        let mut allowed = self.rbac.allowed_skills(&role_ids, &self.store.current());
        for name in self.user_store.registry_for(&ctx.user_id).names() {
            if !allowed.iter().any(|n| n == name) {
                allowed.push(name.to_string());
            }
        }
        allowed
    }
}

#[derive(Deserialize)]
struct ReadArgs {
    name: String,
    /// Bundle-relative file to read instead of the `SKILL.md` body.
    #[serde(default)]
    path: Option<String>,
}

impl Tool for ReadSkill {
    fn id(&self) -> &str {
        READ_SKILL_ID
    }

    fn schema(&self) -> ToolDef {
        // The model already has the name → description listing in its system
        // context, so the schema stays short and stable (no per-user skill
        // list baked in — that would churn the prefix cache, same reasoning
        // as `enable_tools`).
        ToolDef::function(
            self.id(),
            "Load an installed skill's full guidance before producing related output. \
             Call with just `name` to get the skill's instructions (and a list of its \
             reference/asset files); call again with `name` + `path` to read one of those \
             files (e.g. an SVG asset to inline). The available skills — by name and what \
             each is for — are listed in your system context."
                .to_string(),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["name"],
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The skill's name, exactly as listed in your system context."
                    },
                    "path": {
                        "type": "string",
                        "description": "Optional. A bundle-relative file to read \
                                        (e.g. \"references/visual-specs.md\" or \
                                        \"assets/logo.svg\"); omit to get the skill's \
                                        instructions. Get valid paths from the `files` \
                                        list a no-`path` call returns."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: ReadArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!("expected {{name: string, path?: string}}: {e}"))
            })?;

            // Authorize before we reveal anything: a not-permitted skill and a
            // non-existent one get the identical answer. Snapshot the live
            // combined registry (global + this user's private) once so it stays
            // alive for the borrows below.
            let allowed = self.allowed_for(&ctx);
            let registry = self.registry_for(&ctx);
            let skill = match registry.get(&args.name) {
                Some(s) if allowed.iter().any(|n| n == &args.name) => s,
                _ => {
                    return Err(ToolError::Failed(format!(
                        "no skill named `{}` is available to you",
                        args.name
                    )));
                }
            };

            match args.path.as_deref() {
                Some(path) => {
                    let content = skill
                        .read_file(path)
                        .map_err(|e| ToolError::Failed(e.to_string()))?;
                    Ok(json!({
                        "name": skill.name,
                        "path": path,
                        "content": content,
                    }))
                }
                None => {
                    let body = skill.body().map_err(|e| {
                        ToolError::Failed(format!("reading skill `{}`: {e}", skill.name))
                    })?;
                    // Stickiness: remember this skill is loaded for the
                    // conversation so `build_request_context` keeps re-injecting
                    // its guidance on later turns without the model re-reading.
                    // Chat path only (proxy has no session); best-effort —
                    // a write hiccup just means the model reloads next turn.
                    if let Some(session_id) = ctx.session_id.as_deref()
                        && let Err(err) = crate::server::db::chat_session_skills::record(
                            &ctx.db,
                            session_id,
                            &skill.name,
                        )
                        .await
                    {
                        tracing::warn!(
                            error = %err,
                            session = %session_id,
                            skill = %skill.name,
                            "read_skill: could not persist skill load (will reload next turn)"
                        );
                    }
                    Ok(json!({
                        "name": skill.name,
                        "description": skill.description,
                        "body": body,
                        "files": skill.files(),
                    }))
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::rbac::config::{RbacConfig, RoleConfig};
    use crate::server::skills::{SkillStore, UserSkillStore};
    use std::path::{Path, PathBuf};

    fn write_skill(parent: &Path, name: &str) -> PathBuf {
        let dir = parent.join(name);
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: d\n---\n\nDo the {name} thing.\n"),
        )
        .unwrap();
        std::fs::write(dir.join("assets/logo.svg"), "<svg/>").unwrap();
        dir
    }

    fn store(parent: &Path, names: &[&str]) -> Arc<SkillStore> {
        for n in names {
            write_skill(parent, n);
        }
        Arc::new(SkillStore::load(parent.to_path_buf()))
    }

    /// An empty per-user store rooted under the same tempdir the global store
    /// uses (`.users/` subdir — a level deeper than the global scanner reaches,
    /// so the two never cross). Most tests exercise global skills; a couple
    /// seed private ones via the returned store.
    fn user_store_in(parent: &Path) -> Arc<UserSkillStore> {
        Arc::new(UserSkillStore::new(parent.join(".users")))
    }

    fn rbac_granting(skills: &[&str]) -> Arc<Resolver> {
        let role = RoleConfig {
            id: "user".into(),
            admin: false,
            models: vec![],
            tools: vec![],
            skills: skills.iter().map(|s| (*s).to_string()).collect(),
        };
        let rbac = RbacConfig {
            default_role: Some("user".into()),
            mappings: vec![],
        };
        Arc::new(Resolver::build(rbac, vec![role]).unwrap())
    }

    async fn ctx() -> ToolContext {
        let pool = crate::server::db::open(Path::new(":memory:"))
            .await
            .unwrap();
        ctx_with(pool, None)
    }

    fn ctx_with(pool: crate::server::db::Pool, session_id: Option<String>) -> ToolContext {
        ToolContext {
            user_id: "u1".into(),
            roles: vec!["user".into()],
            db: pool,
            s3: None,
            assistant_turn_id: None,
            session_id,
            client_ip: None,
            geoip: None,
            chat_feedback: None,
            attachment_reservations: None,
            indexer: None,
            image_gen: None,
            sandbox_lease: None,
            crypto: None,
        }
    }

    async fn seed_session(pool: &crate::server::db::Pool, id: &str) {
        sqlx::query(
            "INSERT INTO users (id, email, created_at, updated_at) \
             VALUES ('u1', 'u1@example.com', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z') \
             ON CONFLICT(id) DO NOTHING",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO chat_sessions (id, user_id, created_at, updated_at) \
             VALUES (?, 'u1', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn body_call_returns_instructions_and_file_list() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ReadSkill::new(
            store(dir.path(), &["brand"]),
            user_store_in(dir.path()),
            rbac_granting(&["brand"]),
        );
        let out = tool
            .run(ctx().await, json!({"name": "brand"}))
            .await
            .unwrap();
        assert_eq!(out["name"], "brand");
        assert_eq!(out["body"].as_str().unwrap().trim(), "Do the brand thing.");
        assert_eq!(out["files"], json!(["assets/logo.svg"]));
    }

    #[tokio::test]
    async fn body_call_records_stickiness_for_the_session() {
        // Loading a skill's body in a chat session must persist it, so the
        // next turn's build_request_context re-injects its guidance.
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::server::db::open(Path::new(":memory:"))
            .await
            .unwrap();
        seed_session(&pool, "s1").await;
        let tool = ReadSkill::new(
            store(dir.path(), &["brand"]),
            user_store_in(dir.path()),
            rbac_granting(&["brand"]),
        );
        tool.run(
            ctx_with(pool.clone(), Some("s1".into())),
            json!({"name": "brand"}),
        )
        .await
        .unwrap();
        let loaded = crate::server::db::chat_session_skills::loaded_for_session(&pool, "s1")
            .await
            .unwrap();
        assert_eq!(loaded, vec!["brand".to_string()]);
    }

    #[tokio::test]
    async fn reading_a_file_does_not_record_stickiness() {
        // Only a body load (`read_skill(name)`) marks the skill active; a
        // file pull doesn't, so a bare asset fetch can't pin a skill.
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::server::db::open(Path::new(":memory:"))
            .await
            .unwrap();
        seed_session(&pool, "s1").await;
        let tool = ReadSkill::new(
            store(dir.path(), &["brand"]),
            user_store_in(dir.path()),
            rbac_granting(&["brand"]),
        );
        tool.run(
            ctx_with(pool.clone(), Some("s1".into())),
            json!({"name": "brand", "path": "assets/logo.svg"}),
        )
        .await
        .unwrap();
        assert!(
            crate::server::db::chat_session_skills::loaded_for_session(&pool, "s1")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn path_call_returns_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ReadSkill::new(
            store(dir.path(), &["brand"]),
            user_store_in(dir.path()),
            rbac_granting(&["brand"]),
        );
        let out = tool
            .run(
                ctx().await,
                json!({"name": "brand", "path": "assets/logo.svg"}),
            )
            .await
            .unwrap();
        assert_eq!(out["content"], "<svg/>");
    }

    #[tokio::test]
    async fn rejects_skill_not_granted_to_role() {
        // `legal` is loaded but the role only grants `brand`: same "unknown"
        // answer as a skill that doesn't exist, so we don't leak its existence.
        let dir = tempfile::tempdir().unwrap();
        let tool = ReadSkill::new(
            store(dir.path(), &["brand", "legal"]),
            user_store_in(dir.path()),
            rbac_granting(&["brand"]),
        );
        let err = tool
            .run(ctx().await, json!({"name": "legal"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed(_)), "{err:?}");
    }

    #[tokio::test]
    async fn rejects_unknown_skill() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ReadSkill::new(
            store(dir.path(), &["brand"]),
            user_store_in(dir.path()),
            rbac_granting(&["*"]),
        );
        let err = tool
            .run(ctx().await, json!({"name": "ghost"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed(_)), "{err:?}");
    }

    #[tokio::test]
    async fn path_traversal_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("secret.txt"), "TOPSECRET").unwrap();
        let tool = ReadSkill::new(
            store(dir.path(), &["brand"]),
            user_store_in(dir.path()),
            rbac_granting(&["brand"]),
        );
        let err = tool
            .run(
                ctx().await,
                json!({"name": "brand", "path": "../../secret.txt"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed(_)), "{err:?}");
    }

    #[test]
    fn id_matches_schema_name() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ReadSkill::new(
            store(dir.path(), &[]),
            user_store_in(dir.path()),
            rbac_granting(&[]),
        );
        assert_eq!(tool.id(), tool.schema().function.name);
        assert_eq!(tool.id(), READ_SKILL_ID);
    }

    #[tokio::test]
    async fn private_skill_loads_without_any_role_grant() {
        // Ownership is the grant: a user's own private skill is loadable even
        // though no role grants it. Global skills the role doesn't grant stay
        // invisible.
        let dir = tempfile::tempdir().unwrap();
        let user_store = user_store_in(dir.path());
        user_store
            .save_manifest(
                "u1",
                "myown",
                "---\nname: myown\ndescription: mine.\n---\n\nDo my private thing.\n",
            )
            .unwrap();
        // Global `secret` exists but the role grants nothing.
        let tool = ReadSkill::new(
            store(dir.path(), &["secret"]),
            user_store,
            rbac_granting(&[]),
        );

        // The private skill loads.
        let out = tool
            .run(ctx().await, json!({"name": "myown"}))
            .await
            .unwrap();
        assert_eq!(out["body"].as_str().unwrap().trim(), "Do my private thing.");

        // The ungranted global skill is still refused.
        assert!(
            tool.run(ctx().await, json!({"name": "secret"}))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn another_users_private_skill_is_invisible() {
        // u1 owns `mine`; the tool runs as u1 (ctx()), so a *different* owner's
        // skill must not be reachable. Seed under "u2" and confirm u1 can't see it.
        let dir = tempfile::tempdir().unwrap();
        let user_store = user_store_in(dir.path());
        user_store
            .save_manifest(
                "u2",
                "mine",
                "---\nname: mine\ndescription: u2's.\n---\nbody",
            )
            .unwrap();
        let tool = ReadSkill::new(store(dir.path(), &[]), user_store, rbac_granting(&["*"]));
        // ctx() is user u1 — u2's private skill is not available.
        assert!(
            tool.run(ctx().await, json!({"name": "mine"}))
                .await
                .is_err()
        );
    }
}
