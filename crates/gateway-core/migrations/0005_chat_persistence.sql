-- Persisted chat conversations.
--
-- A `chat_session` is one ChatGPT-style conversation thread. Multiple
-- per user, listed in the sidebar. A `chat_turn` is one message in the
-- thread — role 'user' carries the prompt, role 'assistant' carries
-- the streamed reply (content + reasoning) plus any tool calls the
-- model made. `chat_tool_calls` is a side table because one assistant
-- turn can produce many tool invocations across multiple round-trips
-- with the model.
--
-- Worker writes happen incrementally: a new assistant turn lands as
-- `status='in_progress'` with empty `content`/`reasoning`, then each
-- delta appends to the strings (debounced) and tool-call rows insert
-- as the model emits them. The worker flips `status` to
-- 'completed'/'cancelled'/'errored' on finalize and writes
-- `completed_at`.
--
-- Reconnect protocol: any in-flight assistant turn rendered on
-- `GET /chat/{id}` carries a `data-on-load` that re-subscribes to the
-- live broadcast — so backgrounding the page mid-stream no longer
-- loses progress.

CREATE TABLE chat_sessions (
    id          TEXT PRIMARY KEY NOT NULL,            -- UUID v4
    user_id     TEXT NOT NULL,
    title       TEXT,                                  -- nullable; auto-titled later
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,                         -- bumped on every new turn
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

CREATE INDEX chat_sessions_user_updated
    ON chat_sessions(user_id, updated_at DESC);

CREATE TABLE chat_turns (
    id                    TEXT PRIMARY KEY NOT NULL,   -- UUID; doubles as DOM id suffix
    session_id            TEXT NOT NULL,
    seq                   INTEGER NOT NULL,            -- order within session, 0-based
    role                  TEXT NOT NULL,               -- 'user' | 'assistant'
    -- user-turn payload
    user_content          TEXT,                         -- iff role='user'
    -- assistant-turn payload (all nullable so the row exists from the
    -- moment the worker starts, with status='in_progress')
    model                 TEXT,                         -- iff role='assistant'
    content               TEXT,                         -- accumulated markdown
    reasoning             TEXT,                         -- accumulated reasoning
    reasoning_elapsed_ms  INTEGER,                      -- frozen when content starts
    status                TEXT NOT NULL,                -- 'completed' for user;
                                                        -- 'in_progress'|'completed'
                                                        -- |'cancelled'|'errored' for assistant
    error_message         TEXT,                         -- iff status='errored'
    created_at            TEXT NOT NULL,
    completed_at          TEXT,                         -- nullable until finalize
    FOREIGN KEY (session_id) REFERENCES chat_sessions(id) ON DELETE CASCADE,
    UNIQUE (session_id, seq)
);

CREATE INDEX chat_turns_session_seq ON chat_turns(session_id, seq);

CREATE TABLE chat_tool_calls (
    id              TEXT PRIMARY KEY NOT NULL,         -- the model's tool_call_id
    turn_id         TEXT NOT NULL,
    seq             INTEGER NOT NULL,                  -- order within turn, 0-based
    name            TEXT NOT NULL,
    arguments_json  TEXT NOT NULL,                     -- raw arguments JSON string
    output_json     TEXT,                              -- nullable until tool finishes
    status          TEXT NOT NULL,                     -- 'running'|'completed'|'errored'
    created_at      TEXT NOT NULL,
    completed_at    TEXT,
    FOREIGN KEY (turn_id) REFERENCES chat_turns(id) ON DELETE CASCADE,
    UNIQUE (turn_id, seq)
);

CREATE INDEX chat_tool_calls_turn_seq ON chat_tool_calls(turn_id, seq);
