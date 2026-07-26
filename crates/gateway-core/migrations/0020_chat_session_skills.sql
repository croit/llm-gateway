-- Per-conversation skill stickiness (Agent Skills, Phase 2).
--
-- The chat system message advertises the skills a caller's roles permit
-- (name + description); the model loads one by calling `read_skill(name)`.
-- Without persistence a loaded skill would evaporate on the next turn — the
-- chat driver rebuilds the upstream message list from DB history each turn
-- and drops tool-call results (see `openai_driver::run_one_turn`), so the
-- model would have to re-read the skill every turn to keep applying it.
--
-- A row here records that a skill was loaded in a conversation. On every
-- later turn `build_request_context` re-injects the loaded skills' SKILL.md
-- bodies into the system message, so operator guidance (brand voice, house
-- style) keeps applying for the whole conversation without re-reading. Sticky
-- by design, mirroring `chat_session_tools`: once loaded, a skill stays
-- active for the conversation.
--
-- `skill_name` is the skill's frontmatter `name` (its lookup key). RBAC is
-- still applied at render time — a skill later revoked from the caller's
-- roles is filtered out even if a stale row remains — so this table only ever
-- *remembers* intent, never widens access.
CREATE TABLE chat_session_skills (
    session_id TEXT NOT NULL,
    skill_name TEXT NOT NULL,
    loaded_at  TEXT NOT NULL,
    PRIMARY KEY (session_id, skill_name),
    FOREIGN KEY (session_id) REFERENCES chat_sessions(id) ON DELETE CASCADE
);

-- The hot path reads "which skills are loaded in this conversation" on every
-- turn; index the lookup.
CREATE INDEX idx_chat_session_skills_session ON chat_session_skills (session_id);
