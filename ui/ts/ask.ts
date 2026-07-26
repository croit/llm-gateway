// Mid-turn question prompts. When the assistant's `ask_user` tool needs a
// decision it can't guess, the server injects a card (`#ask-prompt-{turnId}`)
// onto the live SSE stream and parks the tool. These handlers deliver the
// user's answer to `POST /api/v0/me/ask/feedback/{turnId}`, which un-parks the
// tool so the turn continues — with the answer, or knowing it was skipped.
//
// Mirrors the geolocation feedback loop in `geo.ts`; the difference is that the
// answer comes from the DOM (picked options + a text field) rather than from a
// browser API.

function feedbackUrl(turnId: string): string {
    return `/api/v0/me/ask/feedback/${encodeURIComponent(turnId)}`;
}

function card(turnId: string): HTMLElement | null {
    return document.getElementById(`ask-prompt-${turnId}`);
}

function textInput(turnId: string): HTMLInputElement | null {
    const el = document.getElementById(`ask-text-${turnId}`);
    return el instanceof HTMLInputElement ? el : null;
}

function removePrompt(turnId: string): void {
    card(turnId)?.remove();
}

async function post(turnId: string, body: unknown): Promise<void> {
    try {
        const resp = await fetch(feedbackUrl(turnId), {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify(body),
        });
        if (!resp.ok) throw new Error(`server returned ${resp.status}`);
    } catch (err) {
        // The tool is parked with a timeout, so a lost answer degrades to
        // "nobody responded" rather than a stuck turn — but say so, otherwise
        // the user waits for a reply that reflects an answer we never sent.
        window.pushToast('error', `Couldn't send your answer: ${String(err)}`);
    }
}

/** Labels of the options currently selected in this card. */
function pickedLabels(turnId: string): string[] {
    const root = card(turnId);
    if (!root) return [];
    return Array.from(root.querySelectorAll('[data-ask-option][data-picked="1"]'))
        .map((el) => el.getAttribute('data-ask-option') ?? '')
        .filter((l) => l.length > 0);
}

// Option click. Single-select submits immediately — the extra "Send" click
// would be pure friction when the user has already expressed the whole answer.
// Multi-select toggles and waits for Send, since the answer isn't complete yet.
function pick(turnId: string, btn: HTMLElement, multi: boolean): void {
    if (!multi) {
        btn.setAttribute('data-picked', '1');
        void submit(turnId);
        return;
    }
    const picked = btn.getAttribute('data-picked') === '1';
    if (picked) {
        btn.removeAttribute('data-picked');
        btn.classList.remove('btn-primary');
        btn.classList.add('btn-outline');
    } else {
        btn.setAttribute('data-picked', '1');
        btn.classList.add('btn-primary');
        btn.classList.remove('btn-outline');
    }
}

// Send whatever the card holds: picked options, typed text, or both. An empty
// submission is treated as a skip by the server, so pressing Send with nothing
// entered can't leave the tool waiting.
async function submit(turnId: string): Promise<void> {
    const choices = pickedLabels(turnId);
    const text = textInput(turnId)?.value.trim() ?? '';
    removePrompt(turnId);
    await post(turnId, { choices, text: text.length > 0 ? text : undefined });
}

// "Skip": tell the parked tool to stop waiting and proceed on an assumption.
async function dismiss(turnId: string): Promise<void> {
    removePrompt(turnId);
    await post(turnId, { dismissed: true });
}

window.ask = { pick, submit, dismiss };

// Module scope, not the shared global one: `geo.ts` and `push.ts` are
// scripts, so their top-level helpers are globals and a same-named local here
// (`feedbackUrl`, `removePrompt`, `post`) would be a duplicate declaration.
// The side-effect import in `app.ts` works either way.
export {};
