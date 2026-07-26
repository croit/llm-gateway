-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH
--
-- Scope tool-call identity to its owning turn.
--
-- A tool call is a child entity of the assistant-turn aggregate: it is
-- identified by (turn_id, id), not by a global id. The original schema made
-- the model's tool_call_id the entire primary key, so a backend that recycles
-- ids per request (qwen / vLLM emit `call_0`, `call_1`, … reset every
-- response) produced the same id again in a later turn — or in another
-- session — and the insert aborted the turn with
-- `UNIQUE constraint failed: chat_tool_calls.id`.
--
-- Within a single turn the driver already guarantees the ids are unique (the
-- OpenAI tool-call protocol and the per-turn DOM require it). The only wrong
-- assumption was global scope. Move the primary key to (turn_id, id) so
-- recycled ids are only ever compared inside the turn that produced them.
--
-- SQLite can't alter a primary key in place, so rebuild the table. Existing
-- rows all carry globally-unique ids (the old PK enforced it), so the copy is
-- lossless. Nothing references chat_tool_calls as a foreign-key parent, so the
-- drop-and-rename is safe.
CREATE TABLE chat_tool_calls_new (
    id              TEXT NOT NULL,                     -- the model's tool_call_id (unique within its turn)
    turn_id         TEXT NOT NULL,
    seq             INTEGER NOT NULL,                  -- order within turn, 0-based
    name            TEXT NOT NULL,
    arguments_json  TEXT NOT NULL,                     -- raw arguments JSON string
    output_json     TEXT,                              -- nullable until tool finishes
    status          TEXT NOT NULL,                     -- 'running'|'completed'|'errored'
    created_at      TEXT NOT NULL,
    completed_at    TEXT,
    PRIMARY KEY (turn_id, id),
    FOREIGN KEY (turn_id) REFERENCES chat_turns(id) ON DELETE CASCADE,
    UNIQUE (turn_id, seq)
);

INSERT INTO chat_tool_calls_new
    (id, turn_id, seq, name, arguments_json, output_json, status, created_at, completed_at)
SELECT id, turn_id, seq, name, arguments_json, output_json, status, created_at, completed_at
FROM chat_tool_calls;

DROP TABLE chat_tool_calls;
ALTER TABLE chat_tool_calls_new RENAME TO chat_tool_calls;

CREATE INDEX chat_tool_calls_turn_seq ON chat_tool_calls(turn_id, seq);
