# Refactoring- & Dev-Speed-Plan

Ergebnis eines Codebase-weiten Sweeps (Ziel: Komplexität senken, Wartbarkeit
erhöhen, Dev-Builds beschleunigen — **ohne Funktionsänderung**).

## Ausgangslage (gemessen)

- `gateway`-Crate = **108k Zeilen in EINEM Crate** (196 Dateien) → dominiert jede Rebuild-Zeit
- **36 Dateien > 900 Zeilen** (viele davon aber test-lastig; realer Code oft ~halb so groß)
- **22 Page-Module = 20k Zeilen** HTML-Rendering
- **37 Integrations-Test-Binaries**, jede linkt den kompletten Crate (~6,4 s Link jeweils)
- Edit-Rebuild-Loop `gateway`: **~13 s** (4,8 s Frontend + ~8,4 s Codegen/Link)
- `target/` = 307 GB, davon **63 GB veraltetes `incremental/`** (tot seit `incremental=false`)
- 3208 `unwrap/expect`, 1368 `.clone()`, kein `[workspace.lints]`

### Was NICHT angefasst wird (verifiziert gesund)
Backend-Abstraktion ist bereits sauber table-driven — kein `if qwen … else if anthropic`.
- `reasoning.rs`, `model_defaults.rs`, `feature_defaults.rs` — saubere Enum/Tabellen-Designs, voll getestet
- `upstreams/registry.rs` `Backend`/`Pool`-Modell — bestfaktoriert von allen großen Dateien
- `incremental=false` + sccache — **gemessen schneller** als incremental für diesen Monolithen; nicht umschalten
- Linker (Apple `ld-prime`) — bereits optimal auf Apple Silicon
- Die 3208 unwrap/expect — Großteil in Tests/Mutex/infallible-serialize; Request-Pfade sind sauber (nur 23 in `pages/`, alle unkritisch). **Kein Massen-Rewrite.**

---

## P0 — Quick Wins (klein, risikoarm, sofort)

| # | Task | Aufwand | Wirkung |
|---|------|---------|---------|
| 0.1 | `cargo clean` des toten 63 GB `incremental/`-Verzeichnisses; ggf. `target/` aufräumen | S | 63 GB Disk (kein Speed) |
| 0.2 | `mise run test` auf **`cargo nextest run`** umstellen (bereits installiert) | S | Test-Runtime-Parallelität |
| 0.3 | `[workspace.lints]` mit `clippy::unwrap_used`/`expect_used` = warn, `#[allow]` auf Test-Modulen | S | Verhindert Regression des guten Ist-Zustands |
| 0.4 | `ToolContext::for_proxy(state, user, ip)`-Konstruktor — ersetzt 2× 35-Zeilen-Literal in proxy.rs (`:418`, `:2017`) + Drift-Gefahr | S | Entfernt Foot-Gun |
| 0.5 | `ToolCallAcc` + `StreamToolCallAcc` (identische Structs) zu einem Typ + `absorb(&Value)` mergen | S | Vorstufe für 1.1 |
| 0.6 | `parse_ts`/`parse_optional_ts` in `server/db/mod.rs` (analog session-core `db.rs:219`) — ersetzt 15× inline `DbError::Decode`-Pattern + eigene Kopie in `tokens.rs:51` | S | Mechanisch, verhaltensgleich |
| 0.7 | Sandbox-Tool-Beschreibung als `const SANDBOX_TOOL_DESC`/`include_str!` (sandbox.rs `:903`) | S | `schema` schrumpft auf Struktur |
| 0.8 | `ToolContext::for_test(db)`-Builder — ~25 Tool-Test-Module setzen aktuell alle 13 Felder manuell | S | Nur Testcode, aber große Menge |

---

## P1 — Hoher Impact, mittlerer Aufwand

| # | Task | Aufwand | Wirkung |
|---|------|---------|---------|
| 1.1 | **Gemeinsames `sse`-Modul**: SSE-Frame-Decoder + `ChatDelta`-View + geteilter `ToolCallAcc`. Ersetzt die **3× von Hand geschriebene identische SSE-Parse-Schleife** (openai_driver `:618`, proxy `:1881` + `:2359`) inkl. `reasoning_content`→`reasoning`-Fallback (3×). Baut auf 0.5 auf. | M | **Größter Wartbarkeitsgewinn** — SSE-Bugfixes künftig an 1 Stelle |
| 1.2 | **`PageCtx::from_req(&req)`** (theme/lang/nav/datastar + chrome-Felder) — ersetzt das 4-Zeilen-Preamble (26×) und schrumpft den **13-Argument-`nav_or_html_page`**-Aufruf (pages/mod.rs `:780`) auf ~5 Args; entfernt `#[allow(too_many_arguments)]` | M | Größte Lesbarkeits-Steuer in `pages/` weg |
| 1.3 | `proxy.rs::chat_completions` (330 Z., `:250`) aufteilen: gemeinsame Prolog-Extraktion → `chat_bytedumb`/`chat_buffered_tools`/`chat_streaming_tools`; Runner-Closure `:485` als benannte `async fn forward_one_round` | M | God-Function entzerrt |
| 1.4 | `typst_render.rs::render_and_attach` (316 Z., `:334`): `export_pptx`/`export_docx` je Format extrahieren + `compile_with_at_fixup` (Auto-Escape-Retry) isolieren; `require_chat_ctx` für alle 4 Typst-Tools | M | Trickigste Logik isoliert |
| 1.5 | `sandbox.rs` → Modul `sandbox/` (client / run / documents / render / read), reine Moves + Re-Export | M | 10 Tools + Client entflochten |
| 1.6 | Tool-Boilerplate: Default-Methode `parse_args::<T>()` (killt 29× `map_err(InvalidArgs)`) — optional später `define_tool!`-Makro für die 23 `impl Tool` | S→M | Weniger Zeremonie pro Tool |
| 1.7 | Session-Gate (`match require_session_or_redirect … return resp`, **86×**) via `let-else`/`try_session!`-Makro oder `Result<Response, Response>` | M | −3 Zeilen × 86 |
| 1.8 | `push_tool_round_messages(...)` — assistant/tool-Replay ist near-verbatim dupliziert (openai_driver `:987` vs proxy `:2498`) | S | Dedup |

---

## P2 — Groß / strukturell (gestaffelt, größter Langzeit-Payoff)

| # | Task | Aufwand | Wirkung |
|---|------|---------|---------|
| 2.1 | **`gateway`-Crate in Layer-Sub-Crates splitten** — der **einzige** Hebel, der die 13-s-Edit-Loop senkt (→ ~5–7 s in einem Leaf-Crate). Reihenfolge: **`gateway-core` zuerst** (db/ + config + state/AppState + crypto + rbac — hoher fan-in), dann `gateway-tools`, `gateway-web` (pages/), `gateway-drivers` (openai_driver + upstreams), `gateway` (bin: router/proxy/api/main). Achtung: `crate::server` wird von tools/ 182× und pages/ 129× referenziert — Hub muss zuerst entflochten werden. Inkrementell machen. | L | Edit-Loop **13 s → 5–7 s** |
| 2.2 | **37 Test-Binaries zu 1 Harness** (`tests/main.rs` mit `mod proxy; mod rag; …`, geteiltes `tests/common/`) — 37 Links → 1. Risiko: prozess-globaler State (env `set_var`, DB-Pfade, Ports) auf Kollision prüfen; genuin isolierte (OIDC, sandbox-live) separat lassen | M | **1–3 min** weniger `mise run test`/CI |
| 2.3 | `run_one_turn` (openai_driver `:242`, **~810 Z.**, Verschachtelung ~7): `run_round` (SSE, auf 1.1) + `classify_and_dispatch_tool_calls` (`:836`, testbare State-Machine) + `build_round_request` (`:441`) extrahieren | L | Größte God-Function reviewbar |
| 2.4 | `session-core/db.rs` (3037) → Submodule (sessions/turns/tool_calls/search/fork/recovery); `fork_session`/`search_sessions` in Helfer zerlegen; `str_enum!`-Makro für die 3 identischen Enum-`as_str`/`parse` | M | Navigation + Dedup |
| 2.5 | `session-core/render.rs` (2539) → Modul `render/` (markdown/turns/attachments/composer/thinking/tool_calls/canvas), reine Moves | M | Navigations-Gewinn |
| 2.6 | DB `try_get`-Mapping (243× in 15 `map_row`-Fns) schrittweise auf `#[derive(FromRow)]` migrieren — nur wo kein Custom-Decode nötig, mit Tests | L | −hunderte Zeilen (Spalten-Drift-Risiko, vorsichtig) |

---

## Empfohlene Reihenfolge

1. **P0 komplett** (ein Nachmittag, alles risikoarm)
2. **1.1 (SSE-Merge)** und **1.2 (PageCtx)** — die zwei größten Wartbarkeits-Hebel
3. **2.2 (Test-Harness)** — billigster großer Dev-Speed-Gewinn
4. Rest P1 nach Bedarf
5. **2.1 (Crate-Split)** als eigenes, gestaffeltes Projekt (`gateway-core` zuerst) — sobald die Edit-Loop wirklich stört
