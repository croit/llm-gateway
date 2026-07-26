// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use thiserror::Error;

use super::config::{RbacConfig, RoleConfig};
use crate::server::db::gateway_groups::GroupSnapshot;

/// Synthetic group that `[gateway].bootstrap_admin_groups` resolve to. It is
/// injected on every build/reload so a break-glass admin works regardless of
/// what's in (or missing from) the DB group tables — the anti-lockout anchor.
const BOOTSTRAP_ADMIN_GROUP: &str = "__bootstrap_admin__";

/// A group's grants, minus skills (those come via the [`Self::skill_overlay`]).
/// `is_admin` grants the admin UI + resource-restriction bypass; `is_default`
/// makes the group apply to every authenticated user.
#[derive(Debug, Clone, Default)]
struct GroupDef {
    is_admin: bool,
    is_default: bool,
    tools: Vec<String>,
    /// Only populated on the config/test build path; the DB reload path leaves
    /// this empty and sources skills entirely from the overlay.
    skills: Vec<String>,
}

/// The whole resolvable state, swapped atomically on reload.
#[derive(Debug, Clone, Default)]
struct Snapshot {
    /// `(oidc_value, group)` mapping rows.
    mappings: Vec<(String, String)>,
    groups: HashMap<String, GroupDef>,
}

/// Runtime view of the RBAC tables (`gateway_groups`, `oidc_group_mappings`,
/// `group_tool_grants`) plus the skill-grant overlay (`skill_role_grants`).
///
/// A "gateway group" and an internal "role id" are the same thing in the same
/// namespace. The DB is the source of truth; this snapshot is rebuilt from it
/// at startup and after every admin edit ([`Self::reload`]). Tests and the
/// first-boot seed still construct it straight from config ([`Self::build`]).
#[derive(Debug, Clone)]
pub struct Resolver {
    inner: Arc<RwLock<Snapshot>>,
    /// UI-managed skill→group grants (`skill_role_grants`), keyed by skill name
    /// → the groups granted it. `*` as a skill name expands to every loaded
    /// skill. Interior mutability so the resolver shared by `AppState` and the
    /// `read_skill` tool updates without a rebuild.
    skill_overlay: Arc<RwLock<HashMap<String, Vec<String>>>>,
    /// Raw OIDC claim values that always resolve to admin, from
    /// `[gateway].bootstrap_admin_groups`. Static (config-only) — re-applied on
    /// every reload so a botched DB mapping can't lock the operator out.
    bootstrap_admin_groups: Vec<String>,
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

/// The registry surface RBAC needs in order to resolve a grant: enumerate every
/// registered id (for a `*` grant) and check membership (for an explicit one).
///
/// A trait so the resolver can sit *below* the registries it filters against —
/// `ToolRegistry` lives in `gateway-runtime` and `SkillRegistry` in
/// `gateway-features`, both above this crate. It's used through generics, not
/// `dyn`, so there's no vtable on the request path.
pub trait GrantableSet {
    /// Every registered id, in any order.
    fn ids(&self) -> impl Iterator<Item = &str>;
    /// Whether `id` is registered.
    fn has(&self, id: &str) -> bool;
}

/// So call sites can pass the `Arc`-shared registries straight from `AppState`
/// without dereferencing at every call.
impl<T: GrantableSet + ?Sized> GrantableSet for std::sync::Arc<T> {
    fn ids(&self) -> impl Iterator<Item = &str> {
        (**self).ids()
    }

    fn has(&self, id: &str) -> bool {
        (**self).has(id)
    }
}

impl Resolver {
    /// Build straight from `[rbac]` + `[[roles]]` config. Used by tests and by
    /// the first-boot seed's validation; production reloads from the DB via
    /// [`Self::from_snapshot`] / [`Self::reload`]. `models` on each role is
    /// intentionally ignored — model access is governed per-pool now.
    pub fn build(rbac: RbacConfig, roles: Vec<RoleConfig>) -> Result<Self, ResolveError> {
        Self::build_with_bootstrap(rbac, roles, Vec::new())
    }

    pub fn build_with_bootstrap(
        rbac: RbacConfig,
        roles: Vec<RoleConfig>,
        bootstrap_admin_groups: Vec<String>,
    ) -> Result<Self, ResolveError> {
        let mut groups: HashMap<String, GroupDef> = HashMap::new();
        for role in roles {
            if groups.contains_key(&role.id) {
                return Err(ResolveError::DuplicateRole(role.id));
            }
            let is_default = rbac.default_role.as_deref() == Some(role.id.as_str());
            groups.insert(
                role.id.clone(),
                GroupDef {
                    is_admin: role.admin,
                    is_default,
                    tools: role.tools,
                    skills: role.skills,
                },
            );
        }
        if let Some(default) = rbac.default_role.as_deref()
            && !groups.contains_key(default)
        {
            return Err(ResolveError::UnknownDefaultRole(default.into()));
        }
        for m in &rbac.mappings {
            if !groups.contains_key(&m.role) {
                return Err(ResolveError::UnknownRoleInMapping(m.role.clone()));
            }
        }
        let mappings: Vec<(String, String)> = rbac
            .mappings
            .iter()
            .map(|m| (m.oidc_value.clone(), m.role.clone()))
            .collect();

        let mut snapshot = Snapshot { mappings, groups };
        apply_bootstrap(&mut snapshot, &bootstrap_admin_groups);
        Ok(Self {
            inner: Arc::new(RwLock::new(snapshot)),
            skill_overlay: Arc::new(RwLock::new(HashMap::new())),
            bootstrap_admin_groups,
        })
    }

    /// Build from a DB [`GroupSnapshot`] — the production startup path.
    pub fn from_snapshot(snap: GroupSnapshot, bootstrap_admin_groups: Vec<String>) -> Self {
        let snapshot = build_snapshot(&snap, &bootstrap_admin_groups);
        Self {
            inner: Arc::new(RwLock::new(snapshot)),
            skill_overlay: Arc::new(RwLock::new(HashMap::new())),
            bootstrap_admin_groups,
        }
    }

    /// Swap in a freshly-loaded DB snapshot after an admin edit. Bootstrap
    /// admins are re-applied. Skills are handled separately via
    /// [`Self::set_skill_grant_overlay`].
    pub fn reload(&self, snap: GroupSnapshot) {
        let snapshot = build_snapshot(&snap, &self.bootstrap_admin_groups);
        if let Ok(mut guard) = self.inner.write() {
            *guard = snapshot;
        }
    }

    pub fn empty() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Snapshot::default())),
            skill_overlay: Arc::new(RwLock::new(HashMap::new())),
            bootstrap_admin_groups: Vec::new(),
        }
    }

    /// Replace the dynamic skill-grant overlay from flat `(skill, group)` pairs
    /// (the shape stored in `skill_role_grants`). Called once at startup and
    /// after every admin edit.
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

    /// Overlay groups granting `skill` (`*` grants included verbatim). Powers
    /// the admin page's "Granted to" display.
    pub fn overlay_roles_for_skill(&self, skill: &str) -> Vec<String> {
        self.skill_overlay
            .read()
            .ok()
            .and_then(|g| g.get(skill).cloned())
            .unwrap_or_default()
    }

    /// Resolve a user's raw OIDC claim values to the set of group ids they hold:
    /// every default group first, then mapped groups in declaration order,
    /// deduplicated. This is the single "effective groups" seam every
    /// access check flows through.
    pub fn role_ids_for(&self, oidc_values: &[String]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        fn push(id: &str, out: &mut Vec<String>) {
            if !out.iter().any(|s| s == id) {
                out.push(id.to_string());
            }
        }
        let Ok(snap) = self.inner.read() else {
            return out;
        };
        // Default groups (stable order by name so the result is deterministic).
        let mut defaults: Vec<&String> = snap
            .groups
            .iter()
            .filter(|(_, g)| g.is_default)
            .map(|(name, _)| name)
            .collect();
        defaults.sort();
        for d in defaults {
            push(d, &mut out);
        }
        for (value, group) in &snap.mappings {
            if oidc_values.iter().any(|v| v == value) {
                push(group, &mut out);
            }
        }
        out
    }

    /// True if any of the given group ids is flagged `is_admin`.
    pub fn is_admin(&self, role_ids: &[String]) -> bool {
        let Ok(snap) = self.inner.read() else {
            return false;
        };
        role_ids
            .iter()
            .any(|id| snap.groups.get(id).is_some_and(|g| g.is_admin))
    }

    /// True if a resource restricted to `allowed_groups` is accessible to a user
    /// holding `role_ids`. The central enforcement primitive shared by pools,
    /// RAG collections, and MCP connectors:
    ///
    /// * empty `allowed_groups` → unrestricted (visible to all) — the opt-in
    ///   default that keeps existing setups unchanged;
    /// * admins bypass every restriction;
    /// * otherwise the user must hold at least one of the listed groups.
    pub fn resource_allowed(&self, role_ids: &[String], allowed_groups: &[String]) -> bool {
        if allowed_groups.is_empty() {
            return true;
        }
        if self.is_admin(role_ids) {
            return true;
        }
        allowed_groups
            .iter()
            .any(|g| role_ids.iter().any(|r| r == g))
    }

    /// Union of tool ids granted by any of the user's groups, filtered to
    /// registered tools. `*` expands to every registered tool.
    pub fn allowed_tools(&self, role_ids: &[String], registry: &impl GrantableSet) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let Ok(snap) = self.inner.read() else {
            return out;
        };
        for role_id in role_ids {
            let Some(group) = snap.groups.get(role_id) else {
                continue;
            };
            for tool in &group.tools {
                if tool == "*" {
                    for id in registry.ids() {
                        if !out.iter().any(|s| s == id) {
                            out.push(id.to_string());
                        }
                    }
                } else if registry.has(tool) && !out.iter().any(|s| s == tool) {
                    out.push(tool.clone());
                }
            }
        }
        out
    }

    /// Whether any of `role_ids`'s groups declares a tool grant that should
    /// also unlock the dynamically-loaded ComfyUI workflows (`comfyui_<id>`
    /// tools that aren't in the static registry). True when the group lists
    /// `*`, lists any `comfyui_*` id, or is admin-tier (admins bypass the
    /// tool gate entirely). The caller then expands the result of
    /// [`Self::allowed_tools`] with the live catalog's workflow ids.
    pub fn grants_comfyui_overlay(&self, role_ids: &[String]) -> ComfyuiGrant {
        let Ok(snap) = self.inner.read() else {
            return ComfyuiGrant::None;
        };
        let mut wildcard = false;
        let mut specific: Vec<String> = Vec::new();
        for role_id in role_ids {
            let Some(group) = snap.groups.get(role_id) else {
                continue;
            };
            for tool in &group.tools {
                if tool == "*" {
                    wildcard = true;
                } else if tool.starts_with(crate::server::tool_naming::COMFYUI_PREFIX)
                    && !specific.contains(tool)
                {
                    specific.push(tool.clone());
                }
            }
        }
        // Check admin using the snapshot we already hold — avoids
        // re-entrant read-lock on self.inner (which deadlocks against a
        // queued writer). Mirrors is_admin's logic but on `snap`.
        let is_admin = role_ids
            .iter()
            .any(|id| snap.groups.get(id).is_some_and(|g| g.is_admin));
        if is_admin {
            return ComfyuiGrant::Wildcard;
        }
        if wildcard {
            ComfyuiGrant::Wildcard
        } else if !specific.is_empty() {
            ComfyuiGrant::Specific(specific)
        } else {
            ComfyuiGrant::None
        }
    }

    /// Union of skill names granted by any of the user's groups, filtered to
    /// loaded skills. `*` expands to every loaded skill. Sources both the config
    /// build path's per-group `skills` and the DB `skill_role_grants` overlay.
    pub fn allowed_skills(&self, role_ids: &[String], registry: &impl GrantableSet) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if let Ok(snap) = self.inner.read() {
            for role_id in role_ids {
                let Some(group) = snap.groups.get(role_id) else {
                    continue;
                };
                for skill in &group.skills {
                    if skill == "*" {
                        for name in registry.ids() {
                            if !out.iter().any(|s| s == name) {
                                out.push(name.to_string());
                            }
                        }
                    } else if registry.has(skill) && !out.iter().any(|s| s == skill) {
                        out.push(skill.clone());
                    }
                }
            }
        }
        // DB overlay grants, additive. `*` as a skill name expands to every
        // loaded skill for the granted group.
        if let Ok(overlay) = self.skill_overlay.read() {
            for (skill, granted_roles) in overlay.iter() {
                let held = granted_roles
                    .iter()
                    .any(|gr| role_ids.iter().any(|rid| rid == gr));
                if !held {
                    continue;
                }
                if skill == "*" {
                    for name in registry.ids() {
                        if !out.iter().any(|s| s == name) {
                            out.push(name.to_string());
                        }
                    }
                } else if registry.has(skill) && !out.iter().any(|s| s == skill) {
                    out.push(skill.clone());
                }
            }
        }
        out
    }
}

/// Build a runtime [`Snapshot`] from a DB [`GroupSnapshot`], then apply the
/// bootstrap admins. Skills are left to the overlay (empty `GroupDef.skills`).
fn build_snapshot(snap: &GroupSnapshot, bootstrap: &[String]) -> Snapshot {
    let mut groups: HashMap<String, GroupDef> = HashMap::new();
    for g in &snap.groups {
        groups.insert(
            g.name.clone(),
            GroupDef {
                is_admin: g.is_admin,
                is_default: g.is_default,
                tools: Vec::new(),
                skills: Vec::new(),
            },
        );
    }
    for (group, tool) in &snap.tool_grants {
        groups
            .entry(group.clone())
            .or_default()
            .tools
            .push(tool.clone());
    }
    let mappings = snap.mappings.clone();
    let mut snapshot = Snapshot { mappings, groups };
    apply_bootstrap(&mut snapshot, bootstrap);
    snapshot
}

/// Result of [`Resolver::grants_comfyui_overlay`] — describes which
/// `comfyui_*` tool ids the caller may use, given their RBAC grants.
#[derive(Debug, Clone)]
pub enum ComfyuiGrant {
    /// No ComfyUI grant at all — caller may not use any `comfyui_*` tool.
    None,
    /// Wildcard grant — caller may use every currently-loaded workflow.
    Wildcard,
    /// Explicit list — caller may use only these `comfyui_<id>` ids.
    Specific(Vec<String>),
}

/// Inject the synthetic admin group + a mapping for each bootstrap claim value.
fn apply_bootstrap(snapshot: &mut Snapshot, bootstrap: &[String]) {
    if bootstrap.is_empty() {
        return;
    }
    snapshot.groups.insert(
        BOOTSTRAP_ADMIN_GROUP.to_string(),
        GroupDef {
            is_admin: true,
            is_default: false,
            tools: vec!["*".to_string()],
            skills: vec!["*".to_string()],
        },
    );
    for value in bootstrap {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let pair = (value.to_string(), BOOTSTRAP_ADMIN_GROUP.to_string());
        if !snapshot.mappings.contains(&pair) {
            snapshot.mappings.push(pair);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::config::{RbacConfig, RoleConfig, RoleMapping};
    use super::*;

    /// A bare set of registered ids. The resolver's contract is
    /// "grant × registered ids → allowed", so the registries themselves are
    /// irrelevant here — and they now live in crates *above* this one
    /// (`ToolRegistry` in `gateway-runtime`, `SkillRegistry` in
    /// `gateway-features`), which a unit test in this crate can't reach:
    /// a `cfg(test)` build is a separate crate instance whose types wouldn't
    /// unify with theirs. Testing against [`GrantableSet`] directly is both the
    /// only option and the more focused one.
    struct Registered(Vec<String>);

    impl Registered {
        fn of(ids: &[&str]) -> Self {
            Self(ids.iter().map(|s| (*s).to_string()).collect())
        }
    }

    impl GrantableSet for Registered {
        fn ids(&self) -> impl Iterator<Item = &str> {
            self.0.iter().map(String::as_str)
        }

        fn has(&self, id: &str) -> bool {
            self.0.iter().any(|i| i == id)
        }
    }

    fn role(id: &str, tools: &[&str]) -> RoleConfig {
        RoleConfig {
            id: id.into(),
            admin: false,
            tools: tools.iter().map(|s| (*s).to_string()).collect(),
            models: Vec::new(),
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

    fn skill_registry(names: &[&str]) -> Registered {
        Registered::of(names)
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
        let err = Resolver::build(RbacConfig::default(), vec![role("x", &[]), role("x", &[])])
            .unwrap_err();
        assert_eq!(err, ResolveError::DuplicateRole("x".into()));
    }

    #[test]
    fn build_rejects_unknown_default_role() {
        let rbac = RbacConfig {
            default_role: Some("ghost".into()),
            mappings: vec![],
        };
        let err = Resolver::build(rbac, vec![role("user", &[])]).unwrap_err();
        assert_eq!(err, ResolveError::UnknownDefaultRole("ghost".into()));
    }

    #[test]
    fn build_rejects_mapping_to_unknown_role() {
        let rbac = RbacConfig {
            default_role: None,
            mappings: vec![mapping("groups", "engineering", "engineering")],
        };
        let err = Resolver::build(rbac, vec![role("user", &[])]).unwrap_err();
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
        let r = Resolver::build(rbac, vec![role("user", &[])]).unwrap();
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
                role("user", &[]),
                role("engineering", &[]),
                role("admin", &[]),
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
        let r = Resolver::build(rbac, vec![role("engineering", &[])]).unwrap();
        let ids = r.role_ids_for(&["eng-team-a".into(), "eng-team-b".into()]);
        assert_eq!(ids, vec!["engineering".to_string()]);
    }

    #[test]
    fn is_admin_true_only_for_flagged_roles() {
        let r = Resolver::build(
            RbacConfig::default(),
            vec![role("user", &[]), admin_role("platform-admin")],
        )
        .unwrap();
        assert!(r.is_admin(&["platform-admin".into()]));
        assert!(r.is_admin(&["user".into(), "platform-admin".into()]));
        assert!(!r.is_admin(&["user".into()]));
    }

    #[test]
    fn is_admin_false_for_unflagged_role_named_admin() {
        let r = Resolver::build(RbacConfig::default(), vec![role("admin", &["*"])]).unwrap();
        assert!(!r.is_admin(&["admin".into()]));
    }

    #[test]
    fn is_admin_ignores_unknown_role_ids() {
        let r = Resolver::build(RbacConfig::default(), vec![admin_role("ops")]).unwrap();
        assert!(!r.is_admin(&["ghost".into()]));
        assert!(!r.is_admin(&[]));
    }

    #[test]
    fn bootstrap_admin_groups_resolve_to_admin() {
        // A raw OIDC claim value listed in bootstrap always resolves to admin,
        // even with no DB groups at all — the anti-lockout anchor.
        let r =
            Resolver::build_with_bootstrap(RbacConfig::default(), vec![], vec!["ldap-ops".into()])
                .unwrap();
        let ids = r.role_ids_for(&["ldap-ops".into()]);
        assert!(r.is_admin(&ids));
        // A user without the bootstrap claim is not admin.
        assert!(!r.is_admin(&r.role_ids_for(&["someone-else".into()])));
    }

    #[test]
    fn resource_allowed_opt_in_and_admin_bypass() {
        let r = Resolver::build(
            RbacConfig {
                default_role: None,
                mappings: vec![mapping("groups", "g-dev", "developers")],
            },
            vec![role("developers", &[]), admin_role("ops")],
        )
        .unwrap();
        // Unrestricted → everyone.
        assert!(r.resource_allowed(&[], &[]));
        // Restricted → only holders.
        assert!(r.resource_allowed(&["developers".into()], &["developers".into()]));
        assert!(!r.resource_allowed(&["other".into()], &["developers".into()]));
        // Admin bypass.
        assert!(r.resource_allowed(&["ops".into()], &["developers".into()]));
    }

    #[test]
    fn allowed_tools_unions_across_roles() {
        let reg = Registered::of(&["company_echo", "get_current_timestamp"]);
        let r = Resolver::build(
            RbacConfig::default(),
            vec![
                role("user", &["company_echo"]),
                role("engineering", &["get_current_timestamp"]),
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
        let reg = Registered::of(&["company_echo", "get_current_timestamp"]);
        let r = Resolver::build(RbacConfig::default(), vec![role("admin", &["*"])]).unwrap();
        let tools = r.allowed_tools(&["admin".into()], &reg);
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn allowed_tools_skips_unregistered_ids_silently() {
        let reg = Registered::of(&["company_echo"]);
        let r = Resolver::build(
            RbacConfig::default(),
            vec![role("user", &["company_echo", "company.does.not.exist"])],
        )
        .unwrap();
        let tools = r.allowed_tools(&["user".into()], &reg);
        assert_eq!(tools, vec!["company_echo".to_string()]);
    }

    #[test]
    fn allowed_tools_ignores_unknown_role_ids() {
        let reg = Registered::of(&["company_echo"]);
        let r =
            Resolver::build(RbacConfig::default(), vec![role("user", &["company_echo"])]).unwrap();
        assert!(r.allowed_tools(&["nobody".into()], &reg).is_empty());
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
    }

    #[test]
    fn allowed_skills_overlay_unions_with_config() {
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
    fn allowed_skills_overlay_wildcard_expands() {
        let reg = skill_registry(&["brand", "legal"]);
        let r = Resolver::build(RbacConfig::default(), vec![role("user", &[])]).unwrap();
        r.set_skill_grant_overlay(vec![("*".into(), "user".into())]);
        let skills = r.allowed_skills(&["user".into()], &reg);
        assert_eq!(skills.len(), 2);
    }

    #[test]
    fn allowed_skills_overlay_grants_to_a_role_with_no_config_skills() {
        let reg = skill_registry(&["brand"]);
        let r = Resolver::build(RbacConfig::default(), vec![role("user", &[])]).unwrap();
        assert!(r.allowed_skills(&["user".into()], &reg).is_empty());
        r.set_skill_grant_overlay(vec![("brand".into(), "user".into())]);
        assert_eq!(
            r.allowed_skills(&["user".into()], &reg),
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
    fn reload_swaps_snapshot() {
        use crate::server::db::gateway_groups::{GroupRow, GroupSnapshot};
        let r = Resolver::empty();
        assert!(r.role_ids_for(&["g-dev".into()]).is_empty());
        r.reload(GroupSnapshot {
            groups: vec![GroupRow {
                name: "developers".into(),
                description: String::new(),
                is_admin: false,
                is_default: false,
            }],
            mappings: vec![("g-dev".into(), "developers".into())],
            // `company_echo` rather than a "real" tool id: this asserts that a
            // DB tool_grant resolves against the registry at all, and `Echo` is
            // the canonical trivial tool that stays in this crate.
            tool_grants: vec![("developers".into(), "company_echo".into())],
        });
        assert_eq!(
            r.role_ids_for(&["g-dev".into()]),
            vec!["developers".to_string()]
        );
        let reg = Registered::of(&["company_echo"]);
        assert_eq!(
            r.allowed_tools(&["developers".into()], &reg),
            vec!["company_echo".to_string()]
        );
    }
}
