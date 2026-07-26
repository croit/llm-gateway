-- Store the backend API key value itself in the DB (AES-256-GCM sealed), not
-- just the env-var name. This makes the DB the sole source of upstream
-- configuration, so a new backend can be deployed at runtime ("Apply changes")
-- without a server restart to inject a new env var.
--
-- The pair mirrors the MCP secret store (see migration 0023 / server::crypto):
-- the DB layer keeps the opaque (nonce, ciphertext) blobs and never sees
-- plaintext. `api_key_env` stays as an optional fallback (resolved from the
-- environment only when no sealed value is set), so the TOML/env path keeps
-- working. On first-boot migration, seed_from_config resolves each referenced
-- env var once and seals its value here.
ALTER TABLE backends ADD COLUMN api_key_ct    BLOB;
ALTER TABLE backends ADD COLUMN api_key_nonce BLOB;
