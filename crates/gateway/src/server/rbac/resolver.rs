// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use thiserror::Error;

use super::config::{RbacConfig, RoleConfig};
use crate::server::tools::ToolRegistry;

/// Runtime view of `[rbac]` + `[[roles]]`. The static config (`rbac`,
/// `roles`) is built once at startup; the skill-grant overlay is mutable so
/// admin edits on `/admin/skills` take effect live.
#[derive(Debug, Clone)]
pub struct Resolver {
    rbac: RbacConfig,
    roles: HashMap<String, RoleConfig>,
    /// UI-managed skill→role grants, layered *on top of* each role's static
    /// `skills` config (see `server::db::skill_grants`). Keyed by skill name →
    /// the role ids granted it. Interior mutability behind an `Arc` so the one
    /// resolver shared by `AppState` and the `read_skill` tool can be updated
    /// without a rebuild; seeded from the DB at startup and replaced wholesale
    /// after each edit.
    skill_overlay: Arc<RwLock<HashMap<String, Vec<String>>>>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ResolveError {
    #[error("duplicate role id `{0}`")]
    DuplicateRole(String),
    #[error("mapping references unknown role `{0}`")]
    UnknownRoleInMapping(String),
    #[error("default_role `{0}` is not a defined role")]
    UnknownDefaultRole(String),
}

impl Resolver {
    pub fn build(rbac: RbacConfig, roles: Vec<RoleConfig>) -> Result<Self, ResolveError> {
        let mut map: HashMap<String, RoleConfig> = HashMap::new();
        for role in roles {
            if map.contains_key(&role.id) {
                return Err(ResolveError::DuplicateRole(role.id));
            }
            map.insert(role.id.clone(), role);
        }

        if let Some(default) = rbac.default_role.as_deref()
            && !map.contains_key(default)
        {
            return Err(ResolveError::UnknownDefaultRole(default.into()));
        }
        for m in &rbac.mappings {
            if !map.contains_key(&m.role) {
                return Err(ResolveError::UnknownRoleInMapping(m.role.clone()));
            }
        }

        Ok(Self {
            rbac,
            roles: map,
            skill_overlay: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    pub fn empty() -> Self {
        Self {
            rbac: RbacConfig::default(),
            roles: HashMap::new(),
            skill_overlay: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Replace the dynamic skill-grant overlay from flat `(skill, role)` pairs
    /// (the shape stored in `skill_role_grants`). Called once at startup and
    /// after every admin edit. The map is tiny, so a full rebuild + swap is
    /// simpler — and races no reader — than an incremental update.
    pub fn set_skill_grant_overlay(&self, grants: Vec<(String, String)>) {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for (skill, role) in grants {
            let roles = map.entry(skill).or_default();
            if !roles.iter().any(|r| r == &role) {
                roles.push(role);
            }
        }
        if let Ok(mut guard) = self.skill_overlay.write() {
            *guard = map;
        }
    }

    /// Overlay role ids granting `skill` (UI grants only — config grants live
    /// in `[[roles]].skills`). Powers the admin page's "Granted to" display and
    /// pre-checks the grant dialog.
    pub fn overlay_roles_for_skill(&self, skill: &str) -> Vec<String> {
        self.skill_overlay
            .read()
            .ok()
            .and_then(|g| g.get(skill).cloned())
            .unwrap_or_default()
    }

    /// Resolves a user's raw OIDC claim values (the strings stored on
    /// `User.roles`) to the set of internal role IDs they hold. The result is
    /// stable-ordered: default role first (if any), then mappings in
    /// declaration order, deduplicated.
    pub fn role_ids_for(&self, oidc_values: &[String]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        fn push(id: &str, out: &mut Vec<String>) {
            if !out.iter().any(|s| s == id) {
                out.push(id.to_string());
            }
        }
        if let Some(default) = &self.rbac.default_role {
            push(default, &mut out);
        }
        for m in &self.rbac.mappings {
            if oidc_values.iter().any(|v| v == &m.oidc_value) {
                push(&m.role, &mut out);
            }
        }
        out
    }

    /// True if any of the given role IDs is flagged `admin = true` in config.
    /// The single source of truth for admin-UI / admin-action gating; replaces
    /// the former check that matched a role literally named `"admin"`. Unknown
    /// role IDs are ignored.
    pub fn is_admin(&self, role_ids: &[String]) -> bool {
        role_ids
            .iter()
            .any(|id| self.roles.get(id).is_some_and(|r| r.admin))
    }

    /// Union of tool IDs granted by any of the user's roles, filtered to
    /// tools that are actually registered. `"*"` in a role's `tools` list
    /// expands to every registered tool.
    pub fn allowed_tools(&self, role_ids: &[String], registry: &ToolRegistry) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for role_id in role_ids {
            let Some(role) = self.roles.get(role_id) else {
                continue;
            };
            for tool in &role.tools {
                if tool == "*" {
                    for id in registry.ids() {
                        if !out.iter().any(|s| s == id) {
                            out.push(id.to_string());
                        }
                    }
                } else if registry.contains(tool) && !out.iter().any(|s| s == tool) {
                    out.push(tool.clone());
                }
            }
        }
        out
    }

    /// Union of skill names granted by any of the user's roles, filtered to
    /// skills that are actually loaded. `"*"` in a role's `skills` list
    /// expands to every loaded skill. Mirrors [`Self::allowed_tools`] — the
    /// outer authorization bound for both the system-message skill listing
    /// and the `read_skill` tool.
    pub fn allowed_skills(
        &self,
        role_ids: &[String],
        registry: &crate::server::skills::SkillRegistry,
    ) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for role_id in role_ids {
            let Some(role) = self.roles.get(role_id) else {
                continue;
            };
            for skill in &role.skills {
                if skill == "*" {
                    for name in registry.names() {
                        if !out.iter().any(|s| s == name) {
                            out.push(name.to_string());
                        }
                    }
                } else if registry.get(skill).is_some() && !out.iter().any(|s| s == skill) {
                    out.push(skill.clone());
                }
            }
        }
        // UI-managed overlay grants, additive on top of config. Same rules: the
        // skill must be loaded, and we dedupe against what config already
        // granted above.
        if let Ok(overlay) = self.skill_overlay.read() {
            for (skill, granted_roles) in overlay.iter() {
                if registry.get(skill).is_none() || out.iter().any(|s| s == skill) {
                    continue;
                }
                if granted_roles
                    .iter()
                    .any(|gr| role_ids.iter().any(|rid| rid == gr))
                {
                    out.push(skill.clone());
                }
            }
        }
        out
    }

    /// True if any of the user's roles permits the given model. `"*"` matches
    /// anything; a trailing `*` is a prefix match.
    pub fn model_allowed(&self, role_ids: &[String], model: &str) -> bool {
        for role_id in role_ids {
            let Some(role) = self.roles.get(role_id) else {
                continue;
            };
            for pattern in &role.models {
                if pattern_matches(pattern, model) {
                    return true;
                }
            }
        }
        false
    }
}

fn pattern_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" || pattern == value {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*')
        && value.starts_with(prefix)
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::super::config::{RbacConfig, RoleConfig, RoleMapping};
    use super::*;
    use crate::server::tools::{ToolRegistry, echo::Echo, time::CurrentTimestamp};

    fn role(id: &str, tools: &[&str], models: &[&str]) -> RoleConfig {
        RoleConfig {
            id: id.into(),
            admin: false,
            tools: tools.iter().map(|s| (*s).to_string()).collect(),
            models: models.iter().map(|s| (*s).to_string()).collect(),
            skills: Vec::new(),
        }
    }

    fn admin_role(id: &str) -> RoleConfig {
        RoleConfig {
            id: id.into(),
            admin: true,
            tools: Vec::new(),
            models: Vec::new(),
            skills: Vec::new(),
        }
    }

    fn role_with_skills(id: &str, skills: &[&str]) -> RoleConfig {
        RoleConfig {
            id: id.into(),
            admin: false,
            tools: Vec::new(),
            models: Vec::new(),
            skills: skills.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    /// A registry of empty-bodied skills with the given names, for RBAC
    /// tests (which only care about name membership, not file contents).
    fn skill_registry(names: &[&str]) -> crate::server::skills::SkillRegistry {
        use crate::server::skills::Skill;
        crate::server::skills::SkillRegistry::new(names.iter().map(|n| Skill {
            name: (*n).to_string(),
            title: (*n).to_string(),
            description: "d".into(),
            root: std::path::PathBuf::from("/nonexistent").join(n),
        }))
    }

    fn mapping(claim: &str, value: &str, role: &str) -> RoleMapping {
        RoleMapping {
            oidc_claim: claim.into(),
            oidc_value: value.into(),
            role: role.into(),
        }
    }

    #[test]
    fn build_rejects_duplicate_roles() {
        let err = Resolver::build(
            RbacConfig::default(),
            vec![role("x", &[], &[]), role("x", &[], &[])],
        )
        .unwrap_err();
        assert_eq!(err, ResolveError::DuplicateRole("x".into()));
    }

    #[test]
    fn build_rejects_unknown_default_role() {
        let rbac = RbacConfig {
            default_role: Some("ghost".into()),
            mappings: vec![],
        };
        let err = Resolver::build(rbac, vec![role("user", &[], &[])]).unwrap_err();
        assert_eq!(err, ResolveError::UnknownDefaultRole("ghost".into()));
    }

    #[test]
    fn build_rejects_mapping_to_unknown_role() {
        let rbac = RbacConfig {
            default_role: None,
            mappings: vec![mapping("groups", "engineering", "engineering")],
        };
        let err = Resolver::build(rbac, vec![role("user", &[], &[])]).unwrap_err();
        assert_eq!(
            err,
            ResolveError::UnknownRoleInMapping("engineering".into())
        );
    }

    #[test]
    fn role_ids_starts_with_default() {
        let rbac = RbacConfig {
            default_role: Some("user".into()),
            mappings: vec![],
        };
        let r = Resolver::build(rbac, vec![role("user", &[], &[])]).unwrap();
        assert_eq!(r.role_ids_for(&[]), vec!["user".to_string()]);
    }

    #[test]
    fn role_ids_adds_mapped_roles() {
        let rbac = RbacConfig {
            default_role: Some("user".into()),
            mappings: vec![
                mapping("groups", "engineering", "engineering"),
                mapping("groups", "admin", "admin"),
            ],
        };
        let r = Resolver::build(
            rbac,
            vec![
                role("user", &[], &[]),
                role("engineering", &[], &[]),
                role("admin", &[], &[]),
            ],
        )
        .unwrap();
        let ids = r.role_ids_for(&["engineering".into(), "qa".into()]);
        assert_eq!(ids, vec!["user".to_string(), "engineering".to_string()]);
    }

    #[test]
    fn role_ids_dedupes_when_multiple_values_map_to_same_role() {
        let rbac = RbacConfig {
            default_role: None,
            mappings: vec![
                mapping("groups", "eng-team-a", "engineering"),
                mapping("groups", "eng-team-b", "engineering"),
            ],
        };
        let r = Resolver::build(rbac, vec![role("engineering", &[], &[])]).unwrap();
        let ids = r.role_ids_for(&["eng-team-a".into(), "eng-team-b".into()]);
        assert_eq!(ids, vec!["engineering".to_string()]);
    }

    #[test]
    fn is_admin_true_only_for_flagged_roles() {
        let r = Resolver::build(
            RbacConfig::default(),
            vec![role("user", &[], &[]), admin_role("platform-admin")],
        )
        .unwrap();
        // The capability rides on the flag, not the role name: a role named
        // "platform-admin" grants admin, while "user" does not.
        assert!(r.is_admin(&["platform-admin".into()]));
        assert!(r.is_admin(&["user".into(), "platform-admin".into()]));
        assert!(!r.is_admin(&["user".into()]));
    }

    #[test]
    fn is_admin_false_for_unflagged_role_named_admin() {
        // The old code keyed off the literal name "admin"; now a role *named*
        // admin but without the flag must NOT grant admin access.
        let r =
            Resolver::build(RbacConfig::default(), vec![role("admin", &["*"], &["*"])]).unwrap();
        assert!(!r.is_admin(&["admin".into()]));
    }

    #[test]
    fn is_admin_ignores_unknown_role_ids() {
        let r = Resolver::build(RbacConfig::default(), vec![admin_role("ops")]).unwrap();
        assert!(!r.is_admin(&["ghost".into()]));
        assert!(!r.is_admin(&[]));
    }

    #[test]
    fn allowed_tools_unions_across_roles() {
        let reg = ToolRegistry::new().with(Echo).with(CurrentTimestamp);
        let r = Resolver::build(
            RbacConfig::default(),
            vec![
                role("user", &["company_echo"], &[]),
                role("engineering", &["get_current_timestamp"], &[]),
            ],
        )
        .unwrap();
        let tools = r.allowed_tools(&["user".into(), "engineering".into()], &reg);
        assert!(tools.contains(&"company_echo".to_string()));
        assert!(tools.contains(&"get_current_timestamp".to_string()));
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn allowed_tools_wildcard_expands_to_all_registered() {
        let reg = ToolRegistry::new().with(Echo).with(CurrentTimestamp);
        let r = Resolver::build(RbacConfig::default(), vec![role("admin", &["*"], &[])]).unwrap();
        let tools = r.allowed_tools(&["admin".into()], &reg);
        assert_eq!(tools.len(), 2);
        assert!(tools.contains(&"company_echo".to_string()));
        assert!(tools.contains(&"get_current_timestamp".to_string()));
    }

    #[test]
    fn allowed_tools_skips_unregistered_ids_silently() {
        let reg = ToolRegistry::new().with(Echo);
        let r = Resolver::build(
            RbacConfig::default(),
            vec![role(
                "user",
                &["company_echo", "company.does.not.exist"],
                &[],
            )],
        )
        .unwrap();
        let tools = r.allowed_tools(&["user".into()], &reg);
        assert_eq!(tools, vec!["company_echo".to_string()]);
    }

    #[test]
    fn allowed_tools_ignores_unknown_role_ids() {
        let reg = ToolRegistry::new().with(Echo);
        let r = Resolver::build(
            RbacConfig::default(),
            vec![role("user", &["company_echo"], &[])],
        )
        .unwrap();
        let tools = r.allowed_tools(&["nobody".into()], &reg);
        assert!(tools.is_empty());
    }

    #[test]
    fn allowed_skills_unions_and_filters_to_loaded() {
        let reg = skill_registry(&["brand", "legal"]);
        let r = Resolver::build(
            RbacConfig::default(),
            vec![
                role_with_skills("user", &["brand", "ghost"]),
                role_with_skills("eng", &["legal"]),
            ],
        )
        .unwrap();
        let skills = r.allowed_skills(&["user".into(), "eng".into()], &reg);
        // `ghost` isn't loaded → filtered out; `brand` + `legal` survive.
        assert!(skills.contains(&"brand".to_string()));
        assert!(skills.contains(&"legal".to_string()));
        assert_eq!(skills.len(), 2);
    }

    #[test]
    fn allowed_skills_wildcard_expands_to_all_loaded() {
        let reg = skill_registry(&["brand", "legal"]);
        let r = Resolver::build(
            RbacConfig::default(),
            vec![role_with_skills("admin", &["*"])],
        )
        .unwrap();
        let skills = r.allowed_skills(&["admin".into()], &reg);
        assert_eq!(skills.len(), 2);
        assert!(skills.contains(&"brand".to_string()));
        assert!(skills.contains(&"legal".to_string()));
    }

    #[test]
    fn allowed_skills_overlay_unions_with_config() {
        // Config grants `brand` to `user`; the UI overlay additionally grants
        // `legal` to `user`. The caller sees both.
        let reg = skill_registry(&["brand", "legal"]);
        let r = Resolver::build(
            RbacConfig::default(),
            vec![role_with_skills("user", &["brand"])],
        )
        .unwrap();
        r.set_skill_grant_overlay(vec![("legal".into(), "user".into())]);
        let skills = r.allowed_skills(&["user".into()], &reg);
        assert!(skills.contains(&"brand".to_string()));
        assert!(skills.contains(&"legal".to_string()));
        assert_eq!(skills.len(), 2);
    }

    #[test]
    fn allowed_skills_overlay_grants_to_a_role_with_no_config_skills() {
        // A role that grants no skills in config still sees a skill the UI
        // overlay grants it — this is the whole point of the editable grant.
        let reg = skill_registry(&["brand"]);
        let r = Resolver::build(RbacConfig::default(), vec![role("user", &[], &[])]).unwrap();
        assert!(r.allowed_skills(&["user".into()], &reg).is_empty());
        r.set_skill_grant_overlay(vec![("brand".into(), "user".into())]);
        assert_eq!(
            r.allowed_skills(&["user".into()], &reg),
            vec!["brand".to_string()]
        );
    }

    #[test]
    fn allowed_skills_overlay_filters_to_loaded_and_role() {
        let reg = skill_registry(&["brand"]);
        let r = Resolver::build(RbacConfig::default(), vec![role("user", &[], &[])]).unwrap();
        // `ghost` isn't loaded → filtered; `brand` granted to `eng`, not `user`.
        r.set_skill_grant_overlay(vec![
            ("ghost".into(), "user".into()),
            ("brand".into(), "eng".into()),
        ]);
        assert!(r.allowed_skills(&["user".into()], &reg).is_empty());
        assert_eq!(
            r.allowed_skills(&["eng".into()], &reg),
            vec!["brand".to_string()]
        );
    }

    #[test]
    fn overlay_roles_for_skill_reports_grants_and_dedupes() {
        let r = Resolver::empty();
        r.set_skill_grant_overlay(vec![
            ("brand".into(), "eng".into()),
            ("brand".into(), "eng".into()),
            ("brand".into(), "qa".into()),
        ]);
        let mut roles = r.overlay_roles_for_skill("brand");
        roles.sort();
        assert_eq!(roles, vec!["eng".to_string(), "qa".to_string()]);
        assert!(r.overlay_roles_for_skill("missing").is_empty());
    }

    #[test]
    fn set_skill_grant_overlay_replaces_wholesale() {
        let r = Resolver::empty();
        r.set_skill_grant_overlay(vec![("brand".into(), "eng".into())]);
        r.set_skill_grant_overlay(vec![("brand".into(), "qa".into())]);
        assert_eq!(r.overlay_roles_for_skill("brand"), vec!["qa".to_string()]);
    }

    #[test]
    fn allowed_skills_empty_grant_yields_nothing() {
        // A role that grants no skills sees none, even with skills loaded —
        // adding `[skills]` must not silently expose them.
        let reg = skill_registry(&["brand"]);
        let r = Resolver::build(RbacConfig::default(), vec![role("user", &["*"], &["*"])]).unwrap();
        assert!(r.allowed_skills(&["user".into()], &reg).is_empty());
    }

    #[test]
    fn model_allowed_exact() {
        let r = Resolver::build(
            RbacConfig::default(),
            vec![role("user", &[], &["gpt-4o", "llama-3.1-70b"])],
        )
        .unwrap();
        assert!(r.model_allowed(&["user".into()], "gpt-4o"));
        assert!(!r.model_allowed(&["user".into()], "gpt-4o-mini"));
    }

    #[test]
    fn model_allowed_wildcard_and_prefix() {
        let r = Resolver::build(
            RbacConfig::default(),
            vec![
                role("admin", &[], &["*"]),
                role("user", &[], &["llama-3.1-*"]),
            ],
        )
        .unwrap();
        assert!(r.model_allowed(&["admin".into()], "anything"));
        assert!(r.model_allowed(&["user".into()], "llama-3.1-8b"));
        assert!(!r.model_allowed(&["user".into()], "gpt-4o"));
    }
}
