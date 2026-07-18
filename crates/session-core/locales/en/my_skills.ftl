# Strings owned by `gateway/src/rama_server/pages/skills_user.rs` — the
# per-user `/skills` page (a user's own private Agent Skills: upload,
# inline editor, delete, download).

my-skills-page-title = My Skills — LLM Gateway
my-skills-heading = My Skills
my-skills-intro = Your own private skills — guidance the chat model loads on demand in your conversations. Only you can see and use them. Upload a .skill archive, or write one inline.

my-skills-new-button = New skill
my-skills-upload-heading = Upload a skill
my-skills-upload-button = Upload .skill
my-skills-loaded-heading = Your skills
my-skills-none-yet = None yet
my-skills-empty-loaded = No skills yet. Create one, or upload a .skill archive.
my-skills-empty-not-configured = Skills aren't enabled on this gateway.

my-skills-edit-button = Edit
my-skills-download-button = Download
my-skills-download-title = Download this skill as a .skill archive
my-skills-delete-button = Delete
my-skills-delete-title = Delete this skill
my-skills-description-heading = Description
my-skills-files-count = { $count } bundled files

my-skills-new-heading = New skill
my-skills-edit-heading = Edit skill
my-skills-editor-hint = A skill is a SKILL.md file: YAML frontmatter (name, description) followed by the instructions. Edit it directly below.
my-skills-save-button = Save
my-skills-cancel-button = Cancel

my-skills-error-not-configured = Skills aren't enabled on this gateway.
my-skills-error-no-file = No file was uploaded — pick a .skill archive.
my-skills-error-no-name = Add a `name:` line to the SKILL.md frontmatter.
my-skills-error-install-failed = Could not install skill: { $error }
my-skills-error-save-failed = Could not save skill: { $error }
