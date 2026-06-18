// Conversation scroll behaviour.
//
// The `#conversation` section is `data-init`'d to `window.chatScroll.
// init(el)`. Datastar fires `data-init` whenever the attribute is
// initialized — including the first render *and* every time the
// server SSE-swaps `<main>` in place — so this self-binds on every
// mount. The observer is element-bound, not document-bound, so the
// previous one is naturally orphaned when the conversation node is
// detached.
//
// Behaviour (deliberately NOT sticky-to-bottom autoscroll):
//
//   * On send, and only on send, we animate-scroll so the message the
//     user just submitted lands at the TOP of the viewport, with the
//     reply streaming in below it. This is the one and only scroll we
//     ever drive — `window.chatScroll.onUserSend()` (called from the
//     composer's submit handler) arms it, and the first conversation
//     mutation that carries the new user bubble fires it.
//   * While the model streams its reply, we do NOT scroll at all. The
//     user reads top-down at their own pace; tokens land below them.
//     They can scroll freely the whole time. The next send re-anchors
//     to the new message.
//
// Two things make the one scroll robust against the storm of mutations
// that streaming produces:
//
//   1. We run our OWN rAF scroll animation (writing `scrollTop` each
//      frame) instead of CSS `scroll-behavior: smooth`. A native smooth
//      scroll gets cancelled the moment a streaming mutation reflows
//      the container — that was the "scrolls up, then jumps back" bug.
//      Ours re-asserts the target every frame and ignores the reflow.
//   2. To let a freshly-sent message actually reach the top even when
//      the reply is short, we reserve trailing space via
//      `padding-bottom` on the scroll container (so `scrollHeight` is
//      tall enough). Crucially we track that reserve in a variable and
//      NEVER collapse it to re-measure — collapsing it mid-stream
//      clamped the scroll position back down. The reserve shrinks to
//      nothing as the reply grows past one viewport, never below the
//      container's authored `padding-bottom` (the composer clearance).
//
// Each mutation also pings `window.chatComposer.notifyConversation
// Mutated()` so the composer can drain its `pendingClear` flag (=
// the input empties on the first server response after a submit).

const initialised = new WeakSet<Element>();

// Set by `onUserSend` (a stable window surface, no element handle) and
// read+cleared by the live conversation's observer on the next
// mutation. Module-scoped because only one `#conversation` is mounted
// at a time — nav patches detach the old node, orphaning its observer.
let armed = false;

const prefersReducedMotion = (): boolean =>
    window.matchMedia('(prefers-reduced-motion: reduce)').matches;

const SCROLL_MS = 350;
const easeOutCubic = (x: number): number => 1 - Math.pow(1 - x, 3);

const init = (conversation: HTMLElement): void => {
    // `data-init` re-fires if the attribute is mutated; idempotent
    // guard so we never bind two MutationObservers to the same node.
    if (initialised.has(conversation)) return;
    initialised.add(conversation);

    // Authored padding captured once, before we ever set it inline.
    // `padding-bottom` is the floating-composer clearance we must
    // never reserve *less* than; `padding-top` is the floating
    // drawer-button clearance on phone — we offset the anchor by it
    // so a scrolled-to-top message doesn't hide under that button.
    const cs = getComputedStyle(conversation);
    const basePadBottom = parseFloat(cs.paddingBottom) || 0;
    const topInset = Math.max(parseFloat(cs.paddingTop) || 0, 8);

    // The latest user bubble we anchored to the top for this turn.
    // Null until the first send (or after a patch removes it).
    let anchor: HTMLElement | null = null;
    // Extra space we've added on top of `basePadBottom`, tracked so
    // `reserveTailSpace` can re-measure without collapsing the padding.
    let reservePx = 0;
    let padScheduled = false;
    let animId = 0;

    // Offset of `anchor`'s top within the scroll content, independent
    // of the current scroll position (content above the anchor never
    // changes mid-stream, so this is stable for the whole turn).
    const anchorOffset = (): number =>
        anchor!.getBoundingClientRect().top
        - conversation.getBoundingClientRect().top
        + conversation.scrollTop;

    // Reserve just enough trailing space that `anchor` can sit at the
    // top of the viewport (offset by `topInset`). Collapses toward the
    // authored padding as the reply grows past one viewport. Measures
    // via the tracked `reservePx` — it never zeroes the inline padding
    // to measure, which would clamp the scroll position.
    const reserveTailSpace = (): void => {
        if (!anchor || !conversation.contains(anchor)) {
            reservePx = 0;
            conversation.style.paddingBottom = '';
            return;
        }
        // Height of real content (incl. base padding) below the anchor,
        // backing out whatever reserve is currently applied.
        const belowAnchor = conversation.scrollHeight - reservePx - anchorOffset();
        const need = conversation.clientHeight - topInset - belowAnchor;
        reservePx = Math.max(0, need);
        conversation.style.paddingBottom =
            reservePx > 0 ? `${basePadBottom + reservePx}px` : '';
    };

    const cancelAnim = (): void => {
        if (animId) {
            cancelAnimationFrame(animId);
            animId = 0;
        }
    };

    // Drive scrollTop to `target` ourselves over SCROLL_MS, writing the
    // position each frame so streaming reflows can't cancel us. We
    // re-clamp to `target` (recomputed from the stable anchor offset)
    // every frame; the reserve keeps it reachable.
    const animateAnchorToTop = (): void => {
        if (!anchor) return;
        reserveTailSpace();
        const target = Math.max(0, anchorOffset() - topInset);
        const start = conversation.scrollTop;
        cancelAnim();
        if (prefersReducedMotion() || Math.abs(target - start) < 1) {
            conversation.scrollTop = target;
            return;
        }
        const t0 = performance.now();
        const step = (now: number): void => {
            const p = Math.min(1, (now - t0) / SCROLL_MS);
            conversation.scrollTop = start + (target - start) * easeOutCubic(p);
            animId = p < 1 ? requestAnimationFrame(step) : 0;
        };
        animId = requestAnimationFrame(step);
    };

    // A deliberate user scroll mid-animation wins — abort and leave
    // them where they put themselves.
    for (const evt of ['wheel', 'touchmove']) {
        conversation.addEventListener(evt, cancelAnim, { passive: true });
    }

    const observer = new MutationObserver(() => {
        // First mutation after submit = server's SSE response
        // arrived; tell the composer it can drain `pendingClear`.
        window.chatComposer.notifyConversationMutated();

        if (armed) {
            // The send patch appends the user bubble + the empty
            // assistant skeleton together; grab the now-newest user
            // bubble as our anchor and scroll it to the top once.
            const users = conversation.querySelectorAll<HTMLElement>(
                ':scope > .chat-msg--user',
            );
            const last = users[users.length - 1];
            if (last) {
                armed = false;
                anchor = last;
                reservePx = 0;
                requestAnimationFrame(animateAnchorToTop);
                return;
            }
        }

        // Streaming tokens (and any other mutation): never scroll, but
        // keep the reserved tail space honest as the reply grows.
        if (anchor && !padScheduled) {
            padScheduled = true;
            requestAnimationFrame(() => {
                padScheduled = false;
                reserveTailSpace();
            });
        }
    });
    observer.observe(conversation, {
        childList: true,
        subtree: true,
        characterData: true,
    });

    // Keep the reserve viewport-accurate across resizes (orientation
    // change, devtools, window drag) without touching scroll position.
    // Self-removes once this conversation detaches on a nav patch — the
    // observer/element listeners are GC'd with the node, but a `window`
    // listener would otherwise pin every detached conversation alive.
    const onResize = (): void => {
        if (!conversation.isConnected) {
            window.removeEventListener('resize', onResize);
            return;
        }
        if (anchor) reserveTailSpace();
    };
    window.addEventListener('resize', onResize);

    // On a fresh mount with existing content (page load / nav-land /
    // attaching to an in-flight tail), sit at the bottom so the latest
    // reply is in view. We only take over the scroll on the user's
    // next send.
    if (conversation.firstElementChild) {
        conversation.scrollTop = conversation.scrollHeight;
    }
};

// Arm the scroll-to-top for the message about to be appended. Called
// from the composer's submit handler so it fires for genuine sends
// only — retry/edit regeneration goes through its own directives and
// deliberately leaves the scroll where it is.
const onUserSend = (): void => {
    armed = true;
};

window.chatScroll = { init, onUserSend };
