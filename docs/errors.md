# Error handling

We treat error messages as a product surface — users hit them when something goes wrong, and a confusing message wastes someone's afternoon. This doc codifies how we structure errors so they stay useful.

## Three tiers of error types

| Tier | Where | Crate to use | Why |
|---|---|---|---|
| **Domain errors** that cross an API boundary (HTTP response, server-fn return, CLI exit code) | `shared`, public modules of `gateway` and `cli` | `thiserror` | Stable variant tags so callers can match. Each variant maps to a documented OpenAI-style `error.code`. |
| **Internal errors** that bubble up through a binary | `gateway` server pipeline, `cli` command handlers | `anyhow` + `.context(...)` | Cheap to add context, preserves a chain of *what was being attempted*. |
| **`panic!`** | Unreachable code only | — | A panic means a bug. Don't use panics for expected failure modes. |

The boundary rule: **errors that an external observer sees** (HTTP body, CLI stderr, audit log) must be `thiserror`-typed. Inside a function, use `anyhow` freely.

## Anatomy of a good error message

Three things, in this order:

1. **What was happening** — the operation, in plain language.
2. **What went wrong** — the specific cause.
3. **What to do about it** (when there's a non-trivial answer).

Bad:
```
Error: forbidden
```

Good:
```
Error: cannot call /v1/chat/completions
Caused by: model `gpt-4o` is not granted to your role `finance`.
Help: ask an admin to add the model to your role, or list available models with `gw models`.
```

In code that uses `anyhow`, build it with `.context()`:

```rust
let upstream = pool
    .pick()
    .ok_or_else(|| anyhow!("no healthy backend in pool `{}`", pool.name))
    .with_context(|| format!("routing model `{}`", req.model))?;
```

`thiserror` variants carry structured fields; their `Display` impl produces the short form. The "help" line is added at the boundary where the error becomes user-facing — see `gateway::server::api::error::IntoResponse`.

## Mapping to the OpenAI error shape

The HTTP boundary in the gateway converts the internal error tree to:

```json
{ "error": { "message": "...", "type": "...", "code": "..." } }
```

- `message` — user-facing prose. May be multi-sentence. Includes the "help" line.
- `type` — coarse class: `invalid_request_error`, `permission_denied`, `upstream_error`, `internal_error`.
- `code` — stable machine-readable id matched 1:1 with a `thiserror` variant.

The mapping lives in **one** place: `gateway::server::api::error`. Don't sprinkle `IntoResponse` impls across handlers.

## CLI errors

The `gw` CLI prints errors to stderr in this format:

```
gw auth login: failed
  caused by: gateway returned 502 Bad Gateway
  caused by: connection refused (tcp 127.0.0.1:8080)

Help: is the gateway running? Try `mise run dev` or set --gateway <url>.
```

Implemented via `anyhow::Error`'s `{:#}` formatter plus a tail "Help:" line built per command. Exit codes per `docs/cli.md`.

## Logging vs returning

Don't log inside library code. Log at the boundary:
- Gateway: the request middleware logs every `5xx` with the full error chain at WARN.
- CLI: the top-level `main()` prints to stderr.

Re-logging the same error at every level of a call stack produces noise. One log per error chain.

## Sensitive data

Errors never include secrets — tokens, passwords, raw OIDC client secret. Use the `Redacted<T>` newtype in `shared` (`Debug` and `Display` print `***`) for any field that holds a secret. PII (emails, user ids) is fine; secrets are not.

## Testing errors

Every `thiserror` variant gets at least one test that:
- Provokes it through the normal pipeline.
- Asserts the resulting `error.code` and `error.type`.

This prevents drift — if the variant goes away or the code changes, tests break loudly.
