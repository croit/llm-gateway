---
name: take-screenshots
description: Take clean PNG screenshots of the LLM Gateway web UI (for README / docs) without rediscovering the tooling. Use whenever asked to screenshot a gateway page (/, /chat, /tokens, /admin/*, /rag, /admin/skills, login), refresh a docs/img/*.png, or capture an authed/admin page. Covers the Playwright + dev-ui-seed-cookie flow, the exact resolution that matches existing images, and the gotchas (cached-Chromium path, macOS file-input locale, stale dev-ui port).
---

# Take screenshots of the gateway UI

The README images in `docs/img/*.png` are **2800×1900** (viewport 1400×950 at
`deviceScaleFactor: 2`, dark theme). Match that. The reliable path is
Playwright (already installed via mise as `npm:@playwright/cli`) driving a
seeded session — **not** the `Codex-in-chrome` MCP (its Chrome is a separate
instance whose screenshots don't land on a reachable filesystem, and OS
screenshot tools can't target it).

`screenshot.mjs` (next to this file) self-discovers the Playwright lib and the
cached Chromium binary, so you don't hunt for paths.

## Recipe

1. **Start a seeded gateway.** For docs/README shots prefer the **dev-ui stub**
   — it has a generic `dev@example.com` user, an `admin` role, empty
   conversations, and it loads `data/skills` — so screenshots carry **no real
   user data** (important for the public README):

   ```bash
   pkill -f "target/debug/examples/dev_ui"; sleep 1     # avoid a stale instance on :8080
   cargo run --example dev_ui -p gateway > /tmp/devui.log 2>&1 &
   until lsof -nP -iTCP:8080 -sTCP:LISTEN >/dev/null 2>&1; do sleep 2; done; sleep 1
   COOKIE=$(grep -E '    id=' /tmp/devui.log | tail -1 | sed -E 's/.*id=//')
   ```

   - dev-ui binds `127.0.0.1:8080` and prints `id=<signed cookie>`. It's a
     debug-only harness (wiremock upstream); real LLM calls won't work, but
     every UI page renders with the real code path.
   - To shoot the **real** gateway instead (real data, real models), it must
     have a valid session — that's OIDC-only; there's no `/__dev/seed-session`
     anymore. Use dev-ui unless you specifically need real content.

2. **Validate the cookie** (catches a stale-instance/port mismatch before you
   waste a screenshot):

   ```bash
   curl -s -o /dev/null -w '%{http_code}\n' -H "Cookie: id=$COOKIE" \
     http://localhost:8080/admin/skills        # expect 200 (303 = login = bad cookie)
   ```

3. **Shoot:**

   ```bash
   node .Codex/skills/take-screenshots/screenshot.mjs \
     --url http://localhost:8080/admin/skills \
     --out docs/img/skills.png \
     --cookie "id=$COOKIE" \
     --wait "text=Loaded skills" \
     --strip-file-input
   ```

   Then verify with the Read tool (it renders PNGs) and check
   `sips -g pixelWidth -g pixelHeight docs/img/skills.png` is 2800×1900.

## screenshot.mjs flags

- `--url` page to shoot · `--out` PNG path · `--cookie "id=<v>"` (omit for
  anonymous pages like `/login`).
- `--wait "<selector>"` — wait for an element so the shot isn't mid-render
  (e.g. `"text=Loaded skills"`, `"table.tokens-table tbody tr"`).
- `--strip-file-input` — replace native `<input type=file>` controls with an
  English "Choose File" stand-in. **Use this on any page with a file upload**
  (e.g. `/admin/skills`): macOS renders the native button text in the system
  UI locale (German here), which `page.locale` can't override.
- `--light` for light theme · `--width/--height/--scale` to override the
  defaults (don't, unless matching a different image set).
- Env overrides if discovery ever fails: `PLAYWRIGHT_DIR`, `CHROME_EXE`.

## Gotchas (the things that cost time last round)

- **Stale dev-ui** still on :8080 → the cookie you grabbed belongs to a
  different process and every request 303s to `/login`. Always `pkill` first
  and grab the cookie from the log of the instance that actually bound the port.
- **`playwright install` error on launch** → the bundled Playwright wants a
  browser revision that isn't cached. The script avoids this by launching the
  newest cached `chromium-<rev>` via `executablePath`; if it still fails, set
  `CHROME_EXE`.
- **German / non-English native controls** → only the `<input type=file>`
  button; fixed by `--strip-file-input`. Everything else is app-rendered and
  already English.
- **Generic vs real data** → dev-ui = generic (safe for public README); the
  real gateway = Martin's email + real conversations (do **not** put in the
  public README).
- **`docs/images/take-screenshots.mjs`** in the repo is the older canonical
  script but references the removed `/__dev/seed-session` endpoint — it's stale;
  use this skill's flow instead.
