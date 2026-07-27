// Unit tests for the conversation scroll/tail-space contract in `scroll.ts`.
//
// Run with `mise run test-js` (plain `node --test` — Node strips the types off
// the imported .ts module, no bundler involved). Written in .mjs so `tsc
// --noEmit` (which only includes `**/*.ts`) doesn't try to type the fake DOM.
//
// What's pinned here is the thing that kept regressing by hand: how much
// scrollable space sits below the last message, and where "scrolled to the
// end" is. The user-visible contract:
//
//   * A transcript longer than the viewport can always be scrolled until its
//     end sits in the TOP THIRD of the viewport, with empty space behind it.
//   * At rest (page load / nav-land) the end of the transcript sits just above
//     the floating composer — never behind it, and never a screen of blank.
//   * That stays true when the composer grows (tool chips wrapping onto more
//     rows), including when a patch REPLACES the composer node.

import { strict as assert } from 'node:assert';
import test from 'node:test';

// `scroll.ts` is a script, not a module with exports: importing it installs
// `window.chatScroll`, so a `window` has to exist first. Node caches the
// module (a `?cachebust` query doesn't defeat that for stripped .ts), hence one
// import here and a fresh fake conversation element per test — `init` is
// idempotent per element, so that's an honest fresh mount either way.
globalThis.window = {};
await import(new URL('./scroll.ts', import.meta.url).href);
const { chatScroll } = globalThis.window;

const VIEWPORT = 800;
const BASE_PAD = 144; // .chat-col > #conversation { padding-bottom: 9rem }

// Builds the minimal DOM surface `scroll.ts` touches and returns handles to
// drive it: the fake conversation, the current composer, and the callbacks the
// module registered with MutationObserver / ResizeObserver.
function harness({ content, composerHeight, viewport = VIEWPORT, padTop = 0 }) {
    const rafQueue = [];
    const mutationCbs = [];
    const resizeCbs = [];

    let composer = { isConnected: true, height: composerHeight };
    const makeComposerEl = (c) => ({
        get isConnected() {
            return c.isConnected;
        },
        getBoundingClientRect: () => ({ height: c.height, top: 0 }),
    });
    let composerEl = makeComposerEl(composer);

    const conversation = {
        style: { paddingBottom: '' },
        isConnected: true,
        scrollTop: 0,
        clientHeight: viewport,
        // Content plus whatever bottom padding is currently applied, floored at
        // the viewport — exactly how a real scroll container measures.
        get scrollHeight() {
            const pad = parseFloat(this.style.paddingBottom || `${BASE_PAD}`);
            return Math.max(this.clientHeight, content + padTop + pad);
        },
        firstElementChild: {},
        getBoundingClientRect: () => ({ top: 0 }),
        contains: () => true,
        querySelectorAll: () => [],
        addEventListener: () => {},
    };

    Object.assign(globalThis.window, {
        matchMedia: () => ({ matches: false }),
        addEventListener: () => {},
        removeEventListener: () => {},
        chatComposer: { notifyConversationMutated: () => {} },
    });
    globalThis.document = {
        querySelector: (sel) => (sel === '.chat-composer' ? composerEl : null),
    };
    globalThis.getComputedStyle = () => ({
        paddingBottom: `${BASE_PAD}px`,
        paddingTop: `${padTop}px`,
    });
    globalThis.requestAnimationFrame = (cb) => {
        rafQueue.push(cb);
        return rafQueue.length;
    };
    globalThis.cancelAnimationFrame = () => {};
    globalThis.MutationObserver = class {
        constructor(cb) {
            mutationCbs.push(cb);
        }
        observe() {}
        disconnect() {}
    };
    globalThis.ResizeObserver = class {
        constructor(cb) {
            resizeCbs.push(cb);
        }
        observe() {}
        unobserve() {}
        disconnect() {}
    };

    return {
        conversation,
        pad: () => parseFloat(conversation.style.paddingBottom),
        // Max reachable scroll position, and where the end of the transcript
        // lands (in viewport coordinates) once you're there.
        maxScroll: () => conversation.scrollHeight - conversation.clientHeight,
        endAt: (scrollTop) => content + padTop - scrollTop,
        setComposerHeight: (h) => {
            composer.height = h;
            resizeCbs.forEach((cb) => cb());
        },
        // A patch that swaps the composer node outright (not a morph in
        // place) — the old ResizeObserver target goes stale.
        replaceComposer: (h) => {
            composer.isConnected = false;
            composer = { isConnected: true, height: h };
            composerEl = makeComposerEl(composer);
        },
        mutate: () => {
            mutationCbs.forEach((cb) => cb());
            const queued = rafQueue.splice(0, rafQueue.length);
            queued.forEach((cb) => cb(0));
        },
    };
}

function mount(opts) {
    const h = harness(opts);
    chatScroll.init(h.conversation);
    return h;
}

test('a long transcript can be scrolled until its end is in the top third', () => {
    const h = mount({ content: 3000, composerHeight: 300 });
    const endWhenFullyScrolled = h.endAt(h.maxScroll());
    assert.ok(
        endWhenFullyScrolled <= VIEWPORT / 3 + 1,
        `end of chat should reach the top third (<= ${VIEWPORT / 3}px), got ${endWhenFullyScrolled}px`,
    );
});

test('at rest the end of the transcript sits clear of the composer, not on blank space', () => {
    const composerHeight = 300;
    const h = mount({ content: 3000, composerHeight });
    const end = h.endAt(h.conversation.scrollTop);
    // Above the composer's top edge (composer floats 16px off the bottom)...
    assert.ok(
        end < VIEWPORT - composerHeight - 16,
        `end of chat (${end}px) should sit above the composer top edge (${VIEWPORT - composerHeight - 16}px)`,
    );
    // ...and still in the lower half of the screen, i.e. we did NOT park the
    // view on the empty tail space.
    assert.ok(end > VIEWPORT / 2, `end of chat (${end}px) should still be low on screen at rest`);
});

test('a transcript that fits on screen gets no tail space at all', () => {
    const h = mount({ content: 400, composerHeight: 300 });
    assert.equal(h.maxScroll(), 0, 'short transcript must not become scrollable');
    assert.equal(h.conversation.scrollTop, 0);
});

test('growing the composer (chips wrapping) keeps the last message clear of it', () => {
    const h = mount({ content: 3000, composerHeight: 120 });
    h.setComposerHeight(500);
    assert.ok(
        h.pad() >= 500 + 48,
        `bottom padding (${h.pad()}px) must cover the taller composer plus margin`,
    );
});

test('a patch that replaces the composer node still updates the clearance', () => {
    const h = mount({ content: 3000, composerHeight: 120 });
    h.replaceComposer(500);
    // The stale ResizeObserver target can't report this; the conversation
    // mutation path has to re-resolve the composer.
    h.mutate();
    assert.ok(
        h.pad() >= 500 + 48,
        `bottom padding (${h.pad()}px) must cover the replaced composer plus margin`,
    );
});

test('the tail space never collapses below the composer clearance', () => {
    const h = mount({ content: 3000, composerHeight: 700 });
    assert.ok(
        h.pad() >= 700 + 48,
        `a composer taller than the tail fraction must still be cleared, got ${h.pad()}px`,
    );
});
