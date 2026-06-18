// Browser geolocation sharing. The user opts in *explicitly* — from the
// "share precise location" button on `/tools`, or (Phase D) the in-chat
// prompt — and we POST the coordinates to the gateway, which stores them
// on the user row for the `get_user_location` tool to read. Nothing here
// runs on its own: `navigator.geolocation` always requires a user
// gesture plus the browser's permission grant, which is exactly why this
// is a button rather than an automatic post like the timezone one.

const LOCATION_URL = '/api/v0/me/location';

interface ShareResult {
    ok: boolean;
    /** Short human-readable reason when `ok` is false. */
    reason?: string;
}

// Promise wrapper around the callback-based geolocation API. Rejects
// (rather than hanging) on unsupported / denied / timeout so callers can
// branch and the UI never sticks in a "requesting…" state.
function getPosition(): Promise<GeolocationPosition> {
    return new Promise((resolve, reject) => {
        if (!('geolocation' in navigator)) {
            reject(new Error('this browser has no geolocation support'));
            return;
        }
        // Browsers gate geolocation behind a secure context: HTTPS, or a
        // localhost origin. On plain HTTP (e.g. http://<lan-ip>:8080) the
        // call is denied *without* a prompt — pre-empt it with a message
        // that says why, instead of a bare "permission denied".
        if (!window.isSecureContext) {
            reject(new Error(
                'location needs a secure context — open the gateway over HTTPS, or via http://localhost',
            ));
            return;
        }
        navigator.geolocation.getCurrentPosition(resolve, reject, {
            enableHighAccuracy: true,
            timeout: 15000,
            maximumAge: 60000,
        });
    });
}

// Map a GeolocationPositionError (which arrives as the promise rejection
// value) to a terse message. Detected structurally — `GeolocationPosition\
// Error` isn't reliably available as a value to `instanceof`.
function reason(err: unknown): string {
    if (err && typeof err === 'object' && 'code' in err) {
        const e = err as GeolocationPositionError;
        if (e.code === e.PERMISSION_DENIED) {
            // No prompt + denied usually means an insecure origin; if the
            // context IS secure, the site permission is set to "block".
            return window.isSecureContext
                ? 'permission denied — allow location for this site in your browser settings'
                : 'blocked: location needs HTTPS or a localhost origin';
        }
        if (e.code === e.POSITION_UNAVAILABLE) return 'position unavailable';
        if (e.code === e.TIMEOUT) return 'timed out';
    }
    if (err instanceof Error) return err.message;
    return String(err);
}

// The {lat, lon, accuracy} payload both POST paths send. Accuracy is
// dropped when the browser reports a non-finite value.
function extractCoords(pos: GeolocationPosition) {
    return {
        lat: pos.coords.latitude,
        lon: pos.coords.longitude,
        accuracy: Number.isFinite(pos.coords.accuracy) ? pos.coords.accuracy : undefined,
    };
}

// POST a freshly-read position to the gateway. Returns whether it landed
// so the caller can decide what to render next.
async function postCurrentPosition(): Promise<ShareResult> {
    const pos = await getPosition();
    const resp = await fetch(LOCATION_URL, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(extractCoords(pos)),
    });
    if (!resp.ok) throw new Error(`server returned ${resp.status}`);
    return { ok: true };
}

// /tools button: request + store the position, toasting the outcome and
// updating an optional `[data-geo-status]` label near the button.
async function share(btn?: HTMLElement): Promise<ShareResult> {
    const status = document.querySelector('[data-geo-status]');
    const setStatus = (t: string): void => {
        if (status) status.textContent = t;
    };
    if (btn instanceof HTMLButtonElement) btn.disabled = true;
    setStatus('Requesting location…');
    try {
        await postCurrentPosition();
        window.pushToast('success', 'Location shared with the assistant.');
        setStatus('Location shared.');
        return { ok: true };
    } catch (err) {
        const why = reason(err);
        window.pushToast('error', `Couldn't share location: ${why}`);
        setStatus('Not shared.');
        return { ok: false, reason: why };
    } finally {
        if (btn instanceof HTMLButtonElement) btn.disabled = false;
    }
}

// /tools "stop sharing" affordance: forget the stored position.
async function forget(btn?: HTMLElement): Promise<void> {
    const status = document.querySelector('[data-geo-status]');
    if (btn instanceof HTMLButtonElement) btn.disabled = true;
    try {
        const resp = await fetch(LOCATION_URL, { method: 'DELETE' });
        if (!resp.ok) throw new Error(`server returned ${resp.status}`);
        window.pushToast('success', 'Stored location cleared.');
        if (status) status.textContent = 'Not shared.';
    } catch (err) {
        window.pushToast('error', `Couldn't clear location: ${reason(err)}`);
    } finally {
        if (btn instanceof HTMLButtonElement) btn.disabled = false;
    }
}

// ---- Chat feedback loop ----------------------------------------------
//
// When the assistant's `get_user_location` tool needs a precise position
// mid-turn, the server injects a prompt (`#geo-prompt-{turnId}`) over the
// SSE stream and parks the tool. These handlers deliver the user's answer
// to `POST /api/v0/me/location/feedback/{turnId}`, which un-parks the tool
// so the assistant resumes — with the position, or knowing it was declined.

function feedbackUrl(turnId: string): string {
    return `/api/v0/me/location/feedback/${encodeURIComponent(turnId)}`;
}

function removePrompt(turnId: string): void {
    document.getElementById(`geo-prompt-${turnId}`)?.remove();
}

function postFeedback(turnId: string, body: unknown): Promise<Response> {
    return fetch(feedbackUrl(turnId), {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(body),
    });
}

// Prompt "Share location": read the position and hand it to the parked
// tool. On any failure we still tell the tool (as a decline) so it stops
// waiting and falls back to its approximate IP location.
async function shareForTurn(turnId: string): Promise<void> {
    removePrompt(turnId);
    try {
        const pos = await getPosition();
        await postFeedback(turnId, extractCoords(pos));
        window.pushToast('success', 'Location shared with the assistant.');
    } catch (err) {
        await postFeedback(turnId, { denied: true }).catch(() => {});
        window.pushToast('error', `Couldn't share location: ${reason(err)}`);
    }
}

// Prompt "Not now": tell the parked tool to stop waiting.
async function declineForTurn(turnId: string): Promise<void> {
    removePrompt(turnId);
    await postFeedback(turnId, { denied: true }).catch(() => {});
}

window.geo = { share, forget, shareForTurn, declineForTurn };
