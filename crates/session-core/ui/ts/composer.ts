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
// The server flips `$chatStreaming` back to `false` at end-of-stream
// via a `datastar-patch-signals` SSE event — no JS callback needed
// for the lifecycle transition.
//
// History reconstruction used to live here (`collectHistory()` walked
// `#conversation` and JSON-encoded the prior turns into a hidden
// field) but the gateway now persists every turn server-side and
// rebuilds the upstream message list from SQLite. The composer's job
// shrunk to: validate non-empty, flip the streaming signal, and clear
// the textarea once the server's initial SSE event lands.

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

/** True iff `name` looks like a file we'd inline as text. Mirrors
 *  the backend's `chat_attachments::is_inline_text` heuristic so the
 *  chip can show a slightly different label for code-ish files. */
const looksLikeText = (mime: string, name: string): boolean => {
    if (mime.startsWith('text/')) return true;
    const ext = name.split('.').pop()?.toLowerCase() ?? '';
    return [
        'csv','tsv','json','jsonl','ndjson','yaml','yml','toml','xml',
        'md','markdown','rst','txt','log','sql',
        'sh','bash','zsh','py','rs','ts','tsx','js','jsx','go','java',
        'kt','swift','rb','php','c','h','cpp','cc','hpp','css','html','htm',
        'ini','cfg','conf',
    ].includes(ext);
};

const formatBytes = (n: number): string => {
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
    return `${(n / (1024 * 1024)).toFixed(1)} MB`;
};

const refreshChips = (): void => {
    const input = getFileInput();
    const strip = getChipStrip();
    if (!input || !strip) return;
    const files = input.files ? Array.from(input.files) : [];
    strip.innerHTML = '';
    files.forEach((f, idx) => {
        const chip = document.createElement('span');
        chip.className = 'chat-composer__chip';
        chip.title = `${f.name} (${f.type || 'unknown'}, ${formatBytes(f.size)})`;
        const label = document.createElement('span');
        label.className = 'chat-composer__chip-label';
        const kind = f.type.startsWith('image/')
            ? '🖼'
            : looksLikeText(f.type, f.name)
              ? '📄'
              : '📦';
        label.textContent = `${kind} ${f.name}`;
        chip.appendChild(label);
        const meta = document.createElement('span');
        meta.className = 'chat-composer__chip-size';
        meta.textContent = formatBytes(f.size);
        chip.appendChild(meta);
        const close = document.createElement('button');
        close.type = 'button';
        close.className = 'chat-composer__chip-remove';
        close.setAttribute('aria-label', `Remove ${f.name}`);
        close.textContent = '×';
        close.addEventListener('click', (e) => {
            e.preventDefault();
            removeAttachmentAt(idx);
        });
        chip.appendChild(close);
        strip.appendChild(chip);
    });
};

/** Replace the file input's `.files` with a new FileList built from
 *  `files`. The DataTransfer trick is the only cross-browser way to
 *  programmatically assign a FileList — direct construction isn't
 *  allowed. */
const setFiles = (files: File[]): void => {
    const input = getFileInput();
    if (!input) return;
    const dt = new DataTransfer();
    files.forEach((f) => dt.items.add(f));
    input.files = dt.files;
    refreshChips();
};

const currentFiles = (): File[] => {
    const input = getFileInput();
    if (!input || !input.files) return [];
    return Array.from(input.files);
};

const addFiles = (incoming: File[]): void => {
    if (incoming.length === 0) return;
    const existing = currentFiles();
    const dedupKey = (f: File) => `${f.name}/${f.size}/${f.lastModified}`;
    const seen = new Set(existing.map(dedupKey));
    const merged = [...existing];
    incoming.forEach((f) => {
        const k = dedupKey(f);
        if (!seen.has(k)) {
            seen.add(k);
            merged.push(f);
        }
    });
    setFiles(merged);
};

const removeAttachmentAt = (idx: number): void => {
    const next = currentFiles().filter((_, i) => i !== idx);
    setFiles(next);
};

const clearAttachments = (): void => {
    setFiles([]);
};

const openFilePicker = (): void => {
    getFileInput()?.click();
};

const onFilesPicked = (evt: Event): void => {
    const input = evt.target as HTMLInputElement | null;
    if (!input || !input.files) return;
    // Picker assigns its own FileList directly; we just need to
    // re-paint the chip strip.
    refreshChips();
};

const filesFromDataTransfer = (dt: DataTransfer | null): File[] => {
    if (!dt) return [];
    return Array.from(dt.files);
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
    const files = filesFromDataTransfer(evt.dataTransfer);
    if (files.length > 0) addFiles(files);
};
const onPaste = (evt: ClipboardEvent): void => {
    const data = evt.clipboardData;
    if (!data) return;
    const files: File[] = [];
    // `clipboardData.items` is the modern path — it surfaces every
    // pasted entry, including image bytes copied with the OS's
    // screenshot shortcut. Each item has either `kind: 'file'` or
    // `kind: 'string'`; we keep only files.
    for (let i = 0; i < data.items.length; i++) {
        const item = data.items[i];
        if (item.kind === 'file') {
            const f = item.getAsFile();
            if (f) files.push(f);
        }
    }
    if (files.length > 0) {
        evt.preventDefault();
        addFiles(files);
    }
};

const onSubmit = (_evt: Event): boolean => {
    const msg = getMessageInput();
    const hasFiles = currentFiles().length > 0;
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
