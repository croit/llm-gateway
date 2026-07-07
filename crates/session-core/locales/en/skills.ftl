# Strings owned by `gateway/src/rama_server/pages/skills.rs` — the
# `/admin/skills` viewer + manager (upload, delete, grants).
#
# A few keys (the `-part1`/`-part2`/`-part3` suffixed ones) are sentence
# fragments split around an inline `<code>`/`<span>` element in the
# template; the Rust call site re-joins them with explicit space
# literals, so these values intentionally carry no leading/trailing
# whitespace of their own (Fluent's parser isn't guaranteed to preserve
# it on a single-line value).

skills-error-not-configured = Skills aren't configured ([skills] dir is unset).
skills-error-no-file = No file was uploaded — pick a .skill archive.
skills-error-install-failed = Could not install skill: { $error }
skills-error-bad-delete-request = Bad delete request: { $error }
skills-error-delete-failed = Could not delete skill: { $error }
skills-page-title = Skills — LLM Gateway

skills-heading = Skills
skills-intro-part1 = Operator-installed guidance the chat model loads on demand via the
skills-intro-part2 = tool. Upload a
skills-intro-part3 = archive below — it's available immediately, no restart.
skills-empty-loaded = No skills loaded yet. Upload a .skill archive to add one.
skills-empty-not-configured = Skills aren't configured. Set [skills] dir in the gateway config and restart to enable them.

skills-upload-heading = Add a skill
skills-upload-button = Upload .skill
skills-loaded-heading = Loaded skills
skills-none-yet = None yet
skills-source-prefix = Source:

skills-download-title = Download this skill as a .skill archive
skills-download-button = Download
skills-delete-title = Remove this skill
skills-delete-button = Delete
skills-granted-to-heading = Granted to
skills-granted-config-title = Granted in the gateway config ([[roles]].skills)
skills-choose-access-title = Choose which roles can use this skill
skills-no-grants-warning = no role grants this — set access
skills-edit-access-title = Edit which roles can use this skill
skills-edit-access-button = Edit access
skills-files-heading = Files
skills-files-count = { $count } bundled
skills-description-heading = Description

skills-grant-dialog-heading = Who can use this skill?
skills-grant-dialog-desc-part1 = Pick the roles allowed to load
skills-grant-dialog-desc-part2 = . Everyone with a selected role gets it.
skills-grant-dialog-no-roles-part1 = No roles are defined in the gateway config. Add
skills-grant-dialog-no-roles-part2 = entries before you can grant access.
skills-cancel-button = Cancel
skills-save-access-button = Save access

skills-from-config-badge = from config
