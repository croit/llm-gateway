// Croit LLM Gateway service worker.
//
// Primary goal: installability + standalone app window, NOT offline
// chat (an LLM gateway is inherently online). The SW provides:
//   - cache-first for the immutable, content-hashed /assets/* bundles
//     (the ?v=<hash> query makes the full URL a per-content cache key)
//   - network-first for PWA metadata + icons (manifest, favicon,
//     /icons/*), so their short, non-immutable server cache headers
//     govern freshness and a rebrand / manifest change rolls out; the
//     SW cache is only a last-resort offline fallback and only ever
//     stores OK responses
//   - network-first for full-page navigations, falling back to a
//     minimal offline page. Authed HTML is deliberately NEVER cached:
//     pages are per-user (a shared cache with no logout invalidation
//     would leak one user's page to the next), the server may answer
//     with a 303 login redirect, and Datastar SPA navigations return
//     text/event-stream payloads — caching and re-serving any of these
//     offline would show the wrong user's content, a stray login
//     screen, or raw SSE text
//   - network-only passthrough for everything else dynamic: /v1/*,
//     /api/v0/*, all SSE/Datastar streams, and all non-GET requests.
//     Buffering or caching streaming responses would break chat.
//
// `skipWaiting()` + `clients.claim()` ensure new SW versions activate
// immediately so clients aren't stuck behind a stale SW under the
// Datastar SPA morph.
//
// This file is served at /sw.js (root scope) with a short max-age —
// never the `immutable` cache header, otherwise SW updates never ship.

const CACHE = 'gateway-shell-v1';

// Offline-page copy per UI language, mirroring the `lang` cookie the
// rest of the UI honors (see session-core i18n). Falls back to English
// for any locale not listed here.
const OFFLINE_COPY = {
  en: { title: 'Offline', heading: 'You are offline', body: 'The LLM Gateway needs a network connection to function. Chat cannot work offline.' },
  de: { title: 'Offline', heading: 'Sie sind offline', body: 'Das LLM Gateway benötigt eine Netzwerkverbindung. Chat funktioniert offline nicht.' },
  fr: { title: 'Hors ligne', heading: 'Vous êtes hors ligne', body: 'La passerelle LLM nécessite une connexion réseau pour fonctionner. Le chat ne fonctionne pas hors ligne.' },
  es: { title: 'Sin conexión', heading: 'Estás sin conexión', body: 'La pasarela LLM necesita una conexión de red para funcionar. El chat no funciona sin conexión.' },
  ru: { title: 'Нет сети', heading: 'Вы не в сети', body: 'Для работы шлюзу LLM требуется сетевое подключение. Чат не работает офлайн.' },
  zh: { title: '离线', heading: '您已离线', body: 'LLM 网关需要网络连接才能运行。聊天无法在离线状态下使用。' },
};

// Paths that must NEVER be intercepted — always passthrough to network.
function isPassthrough(url) {
  // All non-GET requests (POST, PUT, DELETE, PATCH) — including the
  // chat composer submits, token CRUD, and all mutations.
  // API + proxy surface — /v1/* and /api/v0/*.
  if (url.pathname.startsWith('/v1/') || url.pathname.startsWith('/api/v0/')) {
    return true;
  }
  // SSE / Datastar streams — identified by the Accept header or the
  // datastar-patch-elements event type. Also catch /chat/* tail and
  // any path ending in /tail (the SSE log stream).
  if (url.pathname.startsWith('/chat/') && url.pathname.endsWith('/tail')) {
    return true;
  }
  // OIDC auth flow — never cache.
  if (url.pathname.startsWith('/auth/')) {
    return true;
  }
  // Webhook triggers — authenticated by secret, must hit the server.
  if (url.pathname.startsWith('/hooks/')) {
    return true;
  }
  return false;
}

// Install: nothing to precache. The /assets/* bundles carry a
// ?v=<hash> query the SW can't know ahead of time, so a static
// precache list would never match the real requests (they key on the
// full URL including the query) — they're cached on first fetch
// instead. skipWaiting so the new SW takes over immediately.
self.addEventListener('install', (event) => {
  event.waitUntil(self.skipWaiting());
});

// Activate: clean up old caches, claim existing clients.
self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

// Fetch strategy router.
self.addEventListener('fetch', (event) => {
  const req = event.request;
  const url = new URL(req.url);

  // Only handle same-origin GET. Everything else (cross-origin, non-GET)
  // goes straight to the network.
  if (url.origin !== self.location.origin || req.method !== 'GET') {
    return;
  }

  // Never intercept dynamic/streaming/API paths.
  if (isPassthrough(url)) {
    return;
  }

  // Immutable, content-hashed /assets/* bundles — cache-first. The
  // ?v=<hash> query makes the full URL a per-content key, so a cached
  // entry is always correct and a new deploy misses the cache and
  // re-fetches. Only OK responses are cached.
  if (url.pathname.startsWith('/assets/')) {
    event.respondWith(
      caches.match(req).then((cached) => cached || fetch(req).then((resp) => {
        if (resp.ok) {
          const copy = resp.clone();
          caches.open(CACHE).then((cache) => cache.put(req, copy));
        }
        return resp;
      })),
    );
    return;
  }

  // PWA metadata + icons — network-first so the short, non-immutable
  // server cache headers govern freshness (a manifest / rebrand change
  // rolls out instead of being pinned by the SW). The SW cache is only
  // consulted when the network is unavailable, and only OK responses
  // are stored so a transient 404/502 during a deploy isn't pinned.
  if (
    url.pathname === '/manifest.webmanifest' ||
    url.pathname === '/favicon.ico' ||
    url.pathname.startsWith('/icons/')
  ) {
    event.respondWith(
      fetch(req)
        .then((resp) => {
          if (resp.ok) {
            const copy = resp.clone();
            caches.open(CACHE).then((cache) => cache.put(req, copy));
          }
          return resp;
        })
        .catch(() => caches.match(req)),
    );
    return;
  }

  // Full-page navigations — network-first with an offline fallback.
  // Authed HTML is NEVER cached (see the file header): per-user pages,
  // 303 login redirects, and text/event-stream SPA payloads would all
  // be mis-served if cached and replayed offline.
  if (req.mode === 'navigate') {
    event.respondWith(fetch(req).catch(() => offlineFallback()));
    return;
  }

  // Any other same-origin GET (Datastar SPA navigations that return
  // SSE, XHR, …) — passthrough to the network, never intercepted or
  // cached.
});

// ---- Web Push -----------------------------------------------------------
//
// A `push` event fires when the gateway sends a turn-complete notification
// (see `server::push`). The payload is the JSON `PushMessage`
// {title, body, url, tag}. We show a notification UNLESS a focused, visible
// window already has that conversation open — the server always sends; "am I
// looking at it?" is decided here.
self.addEventListener('push', (event) => {
  let data = {};
  try {
    data = event.data ? event.data.json() : {};
  } catch (_) {
    data = {};
  }
  const title = data.title || 'croit LLM Gateway';
  const url = typeof data.url === 'string' ? data.url : '/';
  const options = {
    body: data.body || '',
    tag: data.tag || 'gateway-turn',
    icon: '/icons/icon-192.png',
    badge: '/icons/icon-192.png',
    data: { url },
  };
  event.waitUntil(
    (async () => {
      const wins = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
      const lookingAtIt = wins.some(
        (c) => c.focused && c.visibilityState === 'visible' && samePath(c.url, url),
      );
      // Suppressing here (not showing a notification) is technically a "silent
      // push", which spends the origin's push budget. That's an accepted
      // trade-off: turn completions are human-paced, and we only suppress the
      // rare case where the user is *actively viewing that exact conversation*,
      // so the shown-vs-silent ratio stays high enough to keep the budget
      // healthy. The alternative — always notifying — defeats the whole point
      // ("only when you're away").
      if (lookingAtIt) return;
      await self.registration.showNotification(title, options);
    })(),
  );
});

// Clicking the notification focuses an existing tab on that conversation, or
// navigates an open tab to it, or opens a new window.
self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const url = (event.notification.data && event.notification.data.url) || '/';
  event.waitUntil(
    (async () => {
      const wins = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
      for (const c of wins) {
        if (samePath(c.url, url)) {
          await c.focus();
          return;
        }
      }
      if (wins.length && 'navigate' in wins[0]) {
        await wins[0].focus();
        try {
          await wins[0].navigate(url);
        } catch (_) {
          /* cross-origin or nav blocked — fall through to openWindow below */
        }
        return;
      }
      await self.clients.openWindow(url);
    })(),
  );
});

// True when a client window's URL is the same-origin path `path`.
function samePath(clientUrl, path) {
  try {
    const u = new URL(clientUrl);
    return u.origin === self.location.origin && u.pathname === path;
  } catch (_) {
    return false;
  }
}

// Pick the offline-page language. A service worker can't read the `Cookie`
// header off a fetch request (cookies aren't exposed to the SW), so the app's
// `lang` cookie is unavailable here — fall back to the browser's own UI
// languages (`navigator.languages`), best-effort, then English.
function offlineLang() {
  const langs = (self.navigator && self.navigator.languages) || [];
  for (const l of langs) {
    const code = (l || '').toLowerCase().split('-')[0];
    if (OFFLINE_COPY[code]) return code;
  }
  return 'en';
}

function offlineFallback() {
  const lang = offlineLang();
  const t = OFFLINE_COPY[lang];
  const html =
    `<!doctype html><html lang="${lang}" data-theme="dark"><head>` +
    '<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">' +
    `<title>${t.title}</title></head>` +
    '<body style="display:flex;align-items:center;justify-content:center;min-height:100vh;margin:0;background:#1d1d1b;color:#fff;font-family:system-ui,sans-serif">' +
    `<div style="text-align:center"><h1>${t.heading}</h1><p>${t.body}</p></div></body></html>`;
  return new Response(html, {
    headers: { 'content-type': 'text/html; charset=utf-8' },
  });
}
