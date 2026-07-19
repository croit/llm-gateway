// Live "Thinking… (X.Ys)" timer — ticked entirely client-side.
//
// This used to be server-driven: the worker rewrote `reasoning_elapsed_ms`
// on every reasoning chunk (throttled to 100 ms) and re-rendered the whole
// assistant bubble per tick just to advance a number. It froze at 0.0s on
// backends that flush their reasoning in a single burst (elapsed ≈ 0 at every
// write, then frozen on the first content delta), and cost a DB write + full
// bubble re-render + SSE morph per 100 ms besides.
//
// Now the server renders, once, a
//   <thinking-timer data-elapsed-ms="…" data-label-template="Thinking… ({secs}s)">…</thinking-timer>
// and this element counts up locally, independent of upstream chunk cadence.
//
// Morph-safety: Datastar re-renders and morphs `#turn-<id>` on every stream
// tick (reasoning text keeps growing). We render the live label into a shadow
// root, which idiomorph never reconciles, so the ticking text survives each
// morph untouched. The host keeps a stable id, so morph reuses the same node
// and `connectedCallback` fires once — the interval is never restarted and the
// count never jumps. The host's light-DOM text is a server-rendered static
// fallback (correct at render instant); the shadow root hides it once the
// element upgrades, so it only shows pre-upgrade or with JS disabled.
//
// `data-elapsed-ms` is the elapsed-so-far the server measured — non-zero when
// a page is (re)loaded mid-reasoning, so the count resumes at the right offset
// instead of restarting at 0. We read it once on connect and thereafter track
// time with the monotonic `performance.now()` clock; later attribute rewrites
// from morph are ignored to keep the tick smooth.

class ThinkingTimer extends HTMLElement {
    private timer: number | null = null;
    private baseMs = 0;
    private startPerf = 0;
    private out: HTMLSpanElement | null = null;

    connectedCallback(): void {
        this.baseMs = Math.max(0, Number(this.dataset.elapsedMs) || 0);
        this.startPerf = performance.now();
        const root = this.shadowRoot ?? this.attachShadow({ mode: 'open' });
        // A bare span; font/colour inherit across the shadow boundary from
        // the host's `.thinking-block__label`, so no styles need copying in.
        root.innerHTML = '<span></span>';
        this.out = root.querySelector('span');
        this.tick();
        // 100 ms matches the one-decimal display granularity.
        this.timer = window.setInterval(() => this.tick(), 100);
    }

    disconnectedCallback(): void {
        if (this.timer !== null) {
            window.clearInterval(this.timer);
            this.timer = null;
        }
    }

    private tick(): void {
        if (!this.out) return;
        const elapsedMs = this.baseMs + (performance.now() - this.startPerf);
        const secs = (elapsedMs / 1000).toFixed(1);
        const template = this.dataset.labelTemplate ?? '{secs}s';
        this.out.textContent = template.replace('{secs}', secs);
    }
}

if (!customElements.get('thinking-timer')) {
    customElements.define('thinking-timer', ThinkingTimer);
}
