---
name: commit-message-helper
description: Writes clear, Conventional-Commits messages from a diff or a description of the change. Use when the user asks to "write a commit message", "summarize this diff", or wants a Conventional Commits subject + body.
---

# Commit Message Helper

Turn a change into a clean, reviewable commit message.

## Format

- Subject line: `<type>(<scope>): <summary>`, imperative mood, ≤ 72 chars.
- Blank line, then a body explaining **what** and **why** (not how), wrapped at 72 columns.
- `type` is one of: feat, fix, refactor, docs, test, chore, perf.

## Rules

- One logical change per commit.
- Don't restate the diff line by line — explain intent.
- Reference an issue in the footer (`Refs #123`) only if the user gives one.
