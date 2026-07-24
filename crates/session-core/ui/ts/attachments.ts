// Shared attachment-strip behaviour for the chat surfaces that let you
// attach files: the main composer (`composer.ts`) and the inline
// message-edit form (`chat/actions.ts`).
//
// Both drive a hidden `<input type="file" name="attachment">` plus a
// chip strip, and both accept files from three sources — the native
// picker, drag-and-drop, and clipboard paste. The only thing that
// differs is *which* pair of elements a given form owns: the composer
// has one singleton pair; there is one edit form (and one pair) per
// user message on the page. So nothing here reaches for an element by
// id — every function takes the concrete `AttachmentEls`, and the same
// code serves the one composer and the N edit forms.

/** The hidden file input + its sibling chip strip for one form. */
export interface AttachmentEls {
    input: HTMLInputElement;
    strip: HTMLElement;
}

/** True iff `name` looks like a file we'd inline as text. Mirrors the
 *  backend's `chat_attachments::is_inline_text` heuristic so the chip
 *  can show a slightly different icon for code-ish files. */
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

/** The files currently held by a form's file input. */
export const currentFiles = (input: HTMLInputElement): File[] =>
    input.files ? Array.from(input.files) : [];

/** Replace the file input's `.files` with a new FileList built from
 *  `files`, then repaint the chip strip. The `DataTransfer` trick is the
 *  only cross-browser way to programmatically assign a FileList —
 *  direct construction isn't allowed. */
export const setFiles = (els: AttachmentEls, files: File[]): void => {
    const dt = new DataTransfer();
    files.forEach((f) => dt.items.add(f));
    els.input.files = dt.files;
    refreshChips(els);
};

const removeAt = (els: AttachmentEls, idx: number): void => {
    setFiles(
        els,
        currentFiles(els.input).filter((_, i) => i !== idx),
    );
};

/** Rebuild the chip strip from the input's current FileList. Each chip
 *  carries a remove button that drops just that file. */
export const refreshChips = (els: AttachmentEls): void => {
    const files = currentFiles(els.input);
    els.strip.innerHTML = '';
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
            removeAt(els, idx);
        });
        chip.appendChild(close);
        els.strip.appendChild(chip);
    });
};

/** Merge `incoming` into the input's existing files, de-duplicating on
 *  name/size/lastModified so a re-drop of the same file is a no-op. */
export const addFiles = (els: AttachmentEls, incoming: File[]): void => {
    if (incoming.length === 0) return;
    const existing = currentFiles(els.input);
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
    setFiles(els, merged);
};

/** File objects carried by a drag-and-drop `DataTransfer`. */
export const filesFromDataTransfer = (dt: DataTransfer | null): File[] =>
    dt ? Array.from(dt.files) : [];

/** File objects carried by a clipboard paste — `items` surfaces every
 *  pasted entry, including image bytes copied with the OS's screenshot
 *  shortcut. Each item is `kind: 'file'` or `kind: 'string'`; keep only
 *  files. */
export const filesFromClipboard = (data: DataTransfer | null): File[] => {
    if (!data) return [];
    const files: File[] = [];
    for (let i = 0; i < data.items.length; i++) {
        const item = data.items[i];
        if (item.kind === 'file') {
            const f = item.getAsFile();
            if (f) files.push(f);
        }
    }
    return files;
};
