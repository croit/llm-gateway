-- UI-managed skill→role grants — the editable overlay behind the
-- `/admin/skills` "Granted to" control.
--
-- Skills are uploaded and deleted live from the admin UI, so their access
-- grants are managed there too rather than only in the static
-- `[[roles]].skills` config. These rows are an *additive overlay*: the RBAC
-- resolver unions them with the config grants when deciding which skills a
-- caller's roles permit. Config stays authoritative and read-only (a role
-- with `skills = ["*"]` keeps every skill regardless of this table); the UI
-- only ever adds or removes rows here.
--
-- Filtering to currently-loaded skills happens in the resolver, so a row left
-- behind for a deleted skill is harmless (it's cleaned up on delete anyway).
CREATE TABLE skill_role_grants (
    skill_name TEXT NOT NULL,
    role_id    TEXT NOT NULL,
    granted_at TEXT NOT NULL,
    PRIMARY KEY (skill_name, role_id)
);
