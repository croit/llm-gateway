# Docs

This directory holds the design docs for the LLM gateway. The agent-facing entry point lives at [`/AGENTS.md`](../AGENTS.md); start there. These files go deeper on specific subsystems.

## Index

| Doc | What it covers |
|---|---|
| [`architecture.md`](architecture.md) | High-level system diagram, request flow, crate boundaries |
| [`dev-workflow.md`](dev-workflow.md) | mise tasks, two-terminal dev loop (cargo + tailwind --watch) |
| [`dependencies.md`](dependencies.md) | Dep policy + the current allowed list and rationale |
| [`auth.md`](auth.md) | OIDC discovery, CLI loopback handoff, gateway-minted tokens, sessions |
| [`gateway-api.md`](gateway-api.md) | OpenAI-compatible HTTP API, streaming, transcription |
| [`upstreams.md`](upstreams.md) | Provider config, model→backend routing, load balancing, health |
| [`tools-rbac.md`](tools-rbac.md) | Tool registry, role→tool mapping, server-side execution loop |
| [`cli.md`](cli.md) | `gw` CLI commands, UX, on-disk config |
| [`ui.md`](ui.md) | Server-rendered HTML with plait + daisyUI + datastar (SSE-patch CRUD pattern) |
| [`testing.md`](testing.md) | Test layers, mocking strategy, coverage targets |
| [`errors.md`](errors.md) | Error type tiers, message anatomy, OpenAI mapping, CLI formatting |
| [`roadmap.md`](roadmap.md) | Phased delivery plan + the current phase |

## Editing rules

- One topic per file. If a doc grows past ~400 lines or starts to cover two distinct subjects, split it.
- Code samples must compile (or be marked `// pseudocode`). If something is aspirational, say so.
- Update docs in the same commit as the code change. If you change the auth flow, update `auth.md`.
- Cross-link freely — a reader landing on any one doc should be able to find the related ones in two clicks.
