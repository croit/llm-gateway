// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use std::collections::HashMap;

use thiserror::Error;

use super::config::{RbacConfig, RoleConfig};
use crate::server::tools::ToolRegistry;

/// Runtime view of `[rbac]` + `[[roles]]`. Built once at startup.
#[derive(Debug, Clone)]
pub struct Resolver {
    rbac: RbacConfig,
    roles: HashMap<String, RoleConfig>,
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

        Ok(Self { rbac, roles: map })
    }

    pub fn empty() -> Self {
        Self {
            rbac: RbacConfig::default(),
            roles: HashMap::new(),
        }
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
            tools: tools.iter().map(|s| (*s).to_string()).collect(),
            models: models.iter().map(|s| (*s).to_string()).collect(),
        }
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
