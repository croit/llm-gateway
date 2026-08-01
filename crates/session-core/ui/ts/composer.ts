// Chat composer client behaviour.
//
// The chat form drives behaviour through Datastar attributes — see
// `render_composer` in `crates/gateway/src/rama_server/pages/chat/
// render.rs`:
//   * `data-signals="{chatStreaming: false}"` on the form establishes
//     the streaming flag. Datastar binds it into the expression scope
//     as `$chatStreaming`.
//   * `data-class="{'chat-composer--streaming': $chatStreaming}"` flips
//     the send/stop button overlay via the existing CSS rules.
//   * `data-on:submit__prevent="window.chatComposer.onSubmit(evt) &&
//     ($chatStreaming = true, @post('/chat/{id}/messages', …))"` runs
//     the empty-guard here, flips the signal, then hands off to
//     Datastar's SSE-aware POST.
//   * `data-on:keydown="window.chatComposer.onKeydown(evt)"` on the
//     textarea handles desktop Enter-to-submit.
//   * `data-on:click="@post('/chat/{id}/cancel'); $chatStreaming = false"`
//     on the stop button does both the optimistic UI flip and the
//     server round-trip.
//
// The server owns both ends of the lifecycle via `datastar-patch-signals`
// SSE events — no JS callback needed for either transition:
//   * `chatStreaming: true` rides on the opening frame of a turn's stream,
//     so "a turn started" and "Stop is showing" are the same event. The
//     client-side flips above are only an optimistic head start.
//   * `chatStreaming: false` arrives at end-of-stream.
// Retry/edit (`render::action_submit`) go through the same pair — they spawn
// a real worker too. They used to set neither, so a regenerated turn streamed
// behind a composer that still read "ready": no Stop button, and the
// Enter-guard below let a second message fire into a busy worker.
//
// History reconstruction used to live here (`collectHistory()` walked
// `#conversation` and JSON-encoded the prior turns into a hidden
// field) but the gateway now persists every turn server-side and
// rebuilds the upstream message list from SQLite. The composer's job
// shrunk to: validate non-empty, flip the streaming signal, and clear
// the textarea once the server's initial SSE event lands.

import {
    type AttachmentEls,
    addFiles,
    currentFiles,
    filesFromClipboard,
    filesFromDataTransfer,
    refreshChips,
    setFiles,
} from './attachments.js';

// `pendingClear` is set the moment a non-empty submit fires and
// cleared by the first conversation mutation that arrives after that
// submit (= the server's SSE response landing). The autoscroll
// observer in `chat/scroll.ts` pings `notifyConversationMutated()` on
// each mutation; we drain the flag inside that callback so the input
// only empties once the message is definitely accepted.
let pendingClear = false;

const isPointerFineDesktop = (): boolean =>
    window.matchMedia('(pointer: fine)').matches;

const getMessageInput = (): HTMLTextAreaElement | null =>
    document.getElementById('message') as HTMLTextAreaElement | null;

const getFileInput = (): HTMLInputElement | null =>
    document.getElementById('chat-attachments-input') as HTMLInputElement | null;

const getChipStrip = (): HTMLElement | null =>
    document.getElementById('chat-attachments-chips');

/** The composer's singleton `{input, strip}` pair, or null if either is
 *  absent (e.g. a page without a live composer). All attachment logic
 *  lives in the shared `attachments.ts`; this just resolves the two
 *  elements the composer owns. */
const els = (): AttachmentEls | null => {
    const input = getFileInput();
    const strip = getChipStrip();
    return input && strip ? { input, strip } : null;
};

const composerFiles = (): File[] => {
    const input = getFileInput();
    return input ? currentFiles(input) : [];
};

const clearAttachments = (): void => {
    const e = els();
    if (e) setFiles(e, []);
};

const openFilePicker = (): void => {
    getFileInput()?.click();
};

const onFilesPicked = (evt: Event): void => {
    const input = evt.target as HTMLInputElement | null;
    if (!input || !input.files) return;
    // Picker assigns its own FileList directly; we just need to
    // re-paint the chip strip.
    const strip = getChipStrip();
    if (strip) refreshChips({ input, strip });
};

const onDragOver = (evt: DragEvent): void => {
    const form = evt.currentTarget as HTMLElement | null;
    form?.classList.add('chat-composer--drag');
};
const onDragLeave = (evt: DragEvent): void => {
    const form = evt.currentTarget as HTMLElement | null;
    form?.classList.remove('chat-composer--drag');
};
const onDrop = (evt: DragEvent): void => {
    const form = evt.currentTarget as HTMLElement | null;
    form?.classList.remove('chat-composer--drag');
    const e = els();
    if (!e) return;
    const files = filesFromDataTransfer(evt.dataTransfer);
    if (files.length > 0) addFiles(e, files);
};
const onPaste = (evt: ClipboardEvent): void => {
    const e = els();
    if (!e) return;
    const files = filesFromClipboard(evt.clipboardData);
    if (files.length > 0) {
        evt.preventDefault();
        addFiles(e, files);
    }
};

const onSubmit = (_evt: Event): boolean => {
    const msg = getMessageInput();
    const hasFiles = composerFiles().length > 0;
    const text = msg?.value.trim() ?? '';
    // Allow attachment-only submits (e.g. drop a screenshot, hit
    // send): the backend still expects a message field but accepts
    // an empty string when at least one attachment is present.
    if (!hasFiles && !text) return false;
    pendingClear = true;
    // Arm the scroll module so the message we're about to send scrolls
    // to the top of the viewport once the server appends it. Send path
    // only — retry/edit have their own submit directives.
    window.chatScroll?.onUserSend?.();
    return true;
};

const onKeydown = (evt: KeyboardEvent): void => {
    if (!isPointerFineDesktop()) return;
    if (evt.key !== 'Enter') return;
    if (evt.shiftKey || evt.ctrlKey || evt.metaKey || evt.altKey) return;
    const target = evt.target as HTMLElement | null;
    const form = target?.closest('form');
    if (!(form instanceof HTMLFormElement)) return;
    // The form's data-class binding keeps `chat-composer--streaming`
    // in sync with `$chatStreaming`; reading the class is the simplest
    // cross-module check.
    if (form.classList.contains('chat-composer--streaming')) {
        evt.preventDefault();
        return;
    }
    evt.preventDefault();
    if (typeof form.requestSubmit === 'function') form.requestSubmit();
    else form.dispatchEvent(new Event('submit', { cancelable: true, bubbles: true }));
};

const notifyConversationMutated = (): void => {
    if (!pendingClear) return;
    const messageInput = getMessageInput();
    if (!messageInput) return;
    pendingClear = false;
    messageInput.value = '';
    // `field-sizing: content` reflows the textarea automatically;
    // dispatching `input` also flips the `:placeholder-shown`-driven
    // mic ↔ send overlay back to the empty state.
    messageInput.dispatchEvent(new Event('input', { bubbles: true }));
    // Attached files were consumed by the server response — drop
    // them from the file input + clear the chip strip.
    clearAttachments();
};

window.chatComposer = {
    onSubmit,
    onKeydown,
    notifyConversationMutated,
    openFilePicker,
    onFilesPicked,
    onDragOver,
    onDragLeave,
    onDrop,
    onPaste,
};
