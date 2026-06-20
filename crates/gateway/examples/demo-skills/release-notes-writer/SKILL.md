---
name: release-notes-writer
description: Turns a list of merged changes into user-facing release notes grouped by Added / Changed / Fixed. Use when the user asks to "write release notes", "draft a changelog", or summarize a milestone for end users.
---

# Release Notes Writer

Produce release notes your users actually want to read.

## Structure

- A one-line headline describing the release theme.
- Sections in this order, omitting any that are empty: **Added**, **Changed**, **Fixed**, **Deprecated**.
- Each bullet is user-facing: describe the benefit, not the implementation.

## Style

- Lead with the verb ("Add dark-mode toggle", "Fix crash when…").
- Keep each bullet to one sentence.
- Link the PR or issue number in parentheses when provided.
- No internal ticket IDs, no author names.

See `references/example.md` for a worked example.
