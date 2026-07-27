// One-click copy for a fenced code block in a reply.
//
// The button markup is server-rendered (`add_code_copy_buttons` in
// session-core's markdown renderer) inside a `.md-code` wrapper that also
// holds the `<pre>`. This file only supplies the click behaviour.
//
// Two deliberate choices:
//
//   - **Delegation on `document`**, not a per-button listener. Datastar
//     morphs assistant bubbles on every streaming tick, so code blocks
//     appear, grow and get replaced continuously; a listener bound per
//     button would need re-binding after each patch (and would leak on the
//     nodes morph discards). One document-level listener never has to know
//     the DOM changed.
//   - **Read the text out of the sibling `<pre>`** instead of carrying it in
//     a `data-*` attribute. The code would otherwise be inlined a second
//     time in every re-render of the turn — for a large JSON payload that
//     doubles the bytes of every SSE tick, which is exactly the payload
//     size this button exists for.
//
// Labels come from `data-copy-label` / `data-copied-label` (Fluent-resolved
// server-side), so nothing here needs to know the user's language.

const RESET_AFTER_MS = 1600;

/** Timers per button, so a rapid second click doesn't reset early. */
const pending = new WeakMap<HTMLElement, number>();

/** Swap the button into its "copied" state, then back after a moment. */
const flashCopied = (btn: HTMLElement): void => {
    const copied = btn.dataset.copiedLabel;
    const idle = btn.dataset.copyLabel;
    btn.classList.add('is-copied');
    if (copied) {
        btn.title = copied;
        btn.setAttribute('aria-label', copied);
    }
    const prev = pending.get(btn);
    if (prev) window.clearTimeout(prev);
    pending.set(
        btn,
        window.setTimeout(() => {
            btn.classList.remove('is-copied');
            if (idle) {
                btn.title = idle;
                btn.setAttribute('aria-label', idle);
            }
            pending.delete(btn);
        }, RESET_AFTER_MS),
    );
};

document.addEventListener('click', (ev) => {
    const target = ev.target;
    if (!(target instanceof Element)) return;
    const btn = target.closest('[data-md-copy]');
    if (!(btn instanceof HTMLElement)) return;
    // Inside a `<form>` in the canvas panel a bare button would submit.
    ev.preventDefault();
    const pre = btn.closest('.md-code')?.querySelector('pre');
    // `innerText` (not `textContent`): the highlighter wraps every token in
    // its own `<span>`, and only `innerText` reproduces the rendered line
    // breaks — `textContent` would hand back one run-together line.
    const text = pre instanceof HTMLElement ? pre.innerText : '';
    if (!text) return;
    navigator.clipboard.writeText(text).then(
        () => flashCopied(btn),
        (err) => window.pushToast('error', `Couldn't copy: ${err}`),
    );
});
