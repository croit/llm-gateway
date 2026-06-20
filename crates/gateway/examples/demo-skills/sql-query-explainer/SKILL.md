---
name: sql-query-explainer
description: Explains what a SQL query does in plain language and flags likely performance pitfalls. Use when the user pastes a SQL statement and asks "what does this do", "is this slow", or wants an index suggestion.
---

# SQL Query Explainer

Make a SQL statement understandable and spot the obvious traps.

## What to produce

1. A one-paragraph plain-language summary of what the query returns.
2. A step-by-step read of the joins and filters, in execution-ish order.
3. Performance notes: missing indexes, `SELECT *`, non-sargable predicates, accidental cross joins.

## Rules

- Never invent table columns that aren't in the query.
- Call out `LIKE '%x'` and functions wrapped around indexed columns as index-defeating.
- Suggest at most the two highest-impact indexes; explain why each helps.
