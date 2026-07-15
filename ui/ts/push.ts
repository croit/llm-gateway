// Web Push opt-in for turn-complete notifications.
//
// The `/tokens` page renders a "Notifications" card carrying every
// user-visible string as `data-msg-*` attributes (so all copy stays
// server-localized — no English lives here) plus enable/disable buttons that
// call `window.gatewayPush.*`. This module:
//   - reflects the *device-local* subscription state into the card (is this
//     browser subscribed? was permission denied? is push even supported?), and
//   - drives subscribe/unsubscribe: request permission, talk to the browser's
//     PushManager with the gateway's VAPID public key, and register/forget the
//     resulting subscription with `/api/v0/push/*`.
//
// State here is per-browser, not server state, so this is plain JS wiring
// rather than a datastar signal. The card is re-initialized whenever it
// (re)enters the DOM — full load or SPA morph — via a MutationObserver.

interface PushConfig {
    enabled: boolean;
    publicKey: string | null;
}

function supported(): boolean {
    return (
        'serviceWorker' in navigator &&
        'PushManager' in window &&
        'Notification' in window
    );
}

// Decode a base64url VAPID key into the Uint8Array `pushManager.subscribe`
// wants as `applicationServerKey`.
function urlB64ToUint8Array(base64: string): Uint8Array<ArrayBuffer> {
    const padding = '='.repeat((4 - (base64.length % 4)) % 4);
    const b64 = (base64 + padding).replace(/-/g, '+').replace(/_/g, '/');
    const raw = atob(b64);
    // Back it with a concrete ArrayBuffer (not the default ArrayBufferLike) so
    // it satisfies the `applicationServerKey: BufferSource` type.
    const out = new Uint8Array(new ArrayBuffer(raw.length));
    for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i);
    return out;
}

function card(): HTMLElement | null {
    return document.querySelector<HTMLElement>('[data-push-card]');
}

function msg(el: HTMLElement, key: string): string {
    return el.dataset[key] ?? '';
}

function toast(kind: ToastKind, message: string) {
    if (message) window.pushToast(kind, message);
}

async function fetchConfig(): Promise<PushConfig> {
    const r = await fetch('/api/v0/push/config', { headers: { accept: 'application/json' } });
    if (!r.ok) throw new Error(`config ${r.status}`);
    return (await r.json()) as PushConfig;
}

async function currentSubscription(): Promise<PushSubscription | null> {
    if (!supported()) return null;
    const reg = await navigator.serviceWorker.ready;
    return reg.pushManager.getSubscription();
}

// Paint the card to match this browser's actual state. Idempotent; guarded so
// the mutations it makes don't re-trigger the observer.
async function refreshCard(): Promise<void> {
    const el = card();
    if (!el) return;
    el.dataset.pushReady = '1';
    el.hidden = false;

    const status = el.querySelector<HTMLElement>('[data-push-status]');
    const enableBtn = el.querySelector<HTMLElement>('[data-push-enable]');
    const disableBtn = el.querySelector<HTMLElement>('[data-push-disable]');
    const setButtons = (showEnable: boolean, showDisable: boolean) => {
        if (enableBtn) enableBtn.hidden = !showEnable;
        if (disableBtn) disableBtn.hidden = !showDisable;
    };

    if (!supported()) {
        if (status) status.textContent = msg(el, 'msgUnsupported');
        setButtons(false, false);
        return;
    }
    if (Notification.permission === 'denied') {
        if (status) status.textContent = msg(el, 'msgDenied');
        setButtons(false, false);
        return;
    }
    let sub: PushSubscription | null = null;
    try {
        sub = await currentSubscription();
    } catch (_) {
        /* SW not ready yet — treat as not subscribed */
    }
    if (sub) {
        if (status) status.textContent = msg(el, 'msgOn');
        setButtons(false, true);
    } else {
        if (status) status.textContent = msg(el, 'msgOff');
        setButtons(true, false);
    }
}

async function enable(btn?: HTMLElement): Promise<void> {
    const el = card();
    if (!supported()) {
        if (el) toast('info', msg(el, 'msgUnsupported'));
        return;
    }
    if (btn instanceof HTMLButtonElement) btn.disabled = true;
    try {
        const cfg = await fetchConfig();
        if (!cfg.enabled || !cfg.publicKey) {
            if (el) toast('info', msg(el, 'msgUnsupported'));
            return;
        }
        const permission = await Notification.requestPermission();
        if (permission !== 'granted') {
            if (el) toast('info', msg(el, 'msgDenied'));
            return;
        }
        const reg = await navigator.serviceWorker.ready;
        let sub = await reg.pushManager.getSubscription();
        if (!sub) {
            sub = await reg.pushManager.subscribe({
                userVisibleOnly: true,
                applicationServerKey: urlB64ToUint8Array(cfg.publicKey),
            });
        }
        const res = await fetch('/api/v0/push/subscribe', {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify(sub.toJSON()),
        });
        if (!res.ok) throw new Error(`subscribe ${res.status}`);
        if (el) toast('success', msg(el, 'msgEnabled'));
    } catch (_) {
        if (el) toast('error', msg(el, 'msgError'));
    } finally {
        if (btn instanceof HTMLButtonElement) btn.disabled = false;
        await refreshCard();
    }
}

async function disable(btn?: HTMLElement): Promise<void> {
    const el = card();
    if (!supported()) return;
    if (btn instanceof HTMLButtonElement) btn.disabled = true;
    try {
        const sub = await currentSubscription();
        if (sub) {
            const endpoint = sub.endpoint;
            await sub.unsubscribe();
            await fetch('/api/v0/push/unsubscribe', {
                method: 'POST',
                headers: { 'content-type': 'application/json' },
                body: JSON.stringify({ endpoint }),
            });
        }
        if (el) toast('info', msg(el, 'msgDisabled'));
    } catch (_) {
        if (el) toast('error', msg(el, 'msgError'));
    } finally {
        if (btn instanceof HTMLButtonElement) btn.disabled = false;
        await refreshCard();
    }
}

window.gatewayPush = { enable, disable };

// Initialize on first paint and whenever the card (re)enters the DOM after a
// SPA morph. The `data-push-ready` marker keeps refreshCard's own DOM edits
// from re-triggering us.
function maybeInit() {
    const el = card();
    if (el && !el.dataset.pushReady) void refreshCard();
}
if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', maybeInit);
} else {
    maybeInit();
}
new MutationObserver(maybeInit).observe(document.documentElement, {
    childList: true,
    subtree: true,
});
