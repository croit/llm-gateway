-- ComfyUI async job tracking.
--
-- When a chat tool submits a ComfyUI workflow, the prompt_id lands here
-- so the scheduler background worker can poll ComfyUI's /history and
-- re-host the produced asset when the job finishes. The row survives
-- gateway restarts — the scheduler reads pending jobs on boot and
-- resumes polling.

CREATE TABLE comfyui_jobs (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    prompt_id       TEXT NOT NULL UNIQUE,           -- ComfyUI's prompt UUID
    session_id      TEXT NOT NULL,                  -- chat session the request came from
    turn_id         TEXT NOT NULL,                  -- assistant turn that submitted the job
    user_id         TEXT NOT NULL,                  -- who requested it
    workflow_id     TEXT NOT NULL,                  -- manifest id (e.g. "text_to_image")
    output_kind     TEXT NOT NULL,                  -- "image" | "video" | "audio" | "json"
    output_node_id  TEXT NOT NULL,                  -- which ComfyUI node holds the result
    filename_prefix TEXT NOT NULL DEFAULT '',       -- manifest's output_filename_prefix
    status          TEXT NOT NULL DEFAULT 'pending', -- pending | completed | failed | timeout
    error_message   TEXT,
    output_filename TEXT,                           -- S3 attachment name once completed
    output_mime     TEXT,
    created_at      TEXT NOT NULL,
    completed_at    TEXT
);

CREATE INDEX comfyui_jobs_status ON comfyui_jobs(status);
CREATE INDEX comfyui_jobs_session ON comfyui_jobs(session_id);
CREATE INDEX comfyui_jobs_user ON comfyui_jobs(user_id);
