// Per-message retry/edit affordances on the chat page.
//
// The bubbles (rendered server-side by session-core) carry the URLs +
// `confirm()` guards in their `data-on:*` directives; this module is the
// small imperative glue those directives call:
//   - fillModel: copy the current model-dropdown value into a retry/edit
//     form's hidden `model` input right before Datastar serialises and
//     POSTs it, so regeneration uses whatever model is selected now.
//   - editStart / editCancel: toggle the `.editing` class on a user
//     bubble so its inline edit form shows (the swap itself is CSS).
//   - editPaste / editDrop / editPickFiles / editFilesPicked: the same
//     attach-by-paste/drop/pick affordance the main composer has, but
//     scoped to whichever edit form fired the event — so you can paste a
//     screenshot into a message you're editing, exactly like a new one.
//     The shared, id-free attachment logic lives in `attachments.ts`.

import {
    type AttachmentEls,
    addFiles,
    filesFromClipboard,
    filesFromDataTransfer,
    refreshChips,
} from '../../../crates/session-core/ui/ts/attachments.js';

/** Resolve the `{input, strip}` pair an edit form owns. Both are
 *  scoped to the form (there is one edit form per user message), so we
 *  never touch a singleton id the way the composer does. */
const editEls = (form: HTMLFormElement): AttachmentEls | null => {
    const input = form.querySelector(
        'input[type="file"][name="attachment"]',
    ) as HTMLInputElement | null;
    const strip = form.querySelector(
        '.chat-msg__edit-chips',
    ) as HTMLElement | null;
    return input && strip ? { input, strip } : null;
};

const formOf = (evt: Event): HTMLFormElement | null => {
    const el = evt.currentTarget as HTMLElement | null;
    const form = el?.closest('form');
    return form instanceof HTMLFormElement ? form : null;
};

const modelValue = (): string => {
    const el = document.getElementById('model') as
        | HTMLInputElement
        | HTMLSelectElement
        | null;
    return el?.value ?? '';
};

const fillModel = (form: HTMLFormElement): boolean => {
    const input = form.querySelector(
        'input[name="model"]',
    ) as HTMLInputElement | null;
    if (input) input.value = modelValue();
    return true;
};

const editStart = (turnId: string): void => {
    const bubble = document.getElementById(`turn-${turnId}`);
    if (!bubble) return;
    bubble.classList.add('editing');
    const ta = bubble.querySelector(
        '.chat-msg__edit-textarea',
    ) as HTMLTextAreaElement | null;
    if (ta) {
        ta.focus();
        ta.setSelectionRange(ta.value.length, ta.value.length);
    }
};

const editCancel = (turnId: string): void => {
    document.getElementById(`turn-${turnId}`)?.classList.remove('editing');
};

/** Open the edit form's hidden file input from its attach button. */
const editPickFiles = (btn: HTMLElement): void => {
    const form = btn.closest('form');
    if (!(form instanceof HTMLFormElement)) return;
    editEls(form)?.input.click();
};

/** Change handler on the edit form's file input (native picker). */
const editFilesPicked = (evt: Event): void => {
    const form = formOf(evt);
    if (!form) return;
    const e = editEls(form);
    if (e) refreshChips(e);
};

const editDragOver = (evt: DragEvent): void => {
    formOf(evt)?.classList.add('chat-msg__edit--drag');
};
const editDragLeave = (evt: DragEvent): void => {
    formOf(evt)?.classList.remove('chat-msg__edit--drag');
};
const editDrop = (evt: DragEvent): void => {
    const form = formOf(evt);
    if (!form) return;
    form.classList.remove('chat-msg__edit--drag');
    const e = editEls(form);
    if (!e) return;
    const files = filesFromDataTransfer(evt.dataTransfer);
    if (files.length > 0) addFiles(e, files);
};
const editPaste = (evt: ClipboardEvent): void => {
    const form = formOf(evt);
    if (!form) return;
    const e = editEls(form);
    if (!e) return;
    const files = filesFromClipboard(evt.clipboardData);
    if (files.length > 0) {
        evt.preventDefault();
        addFiles(e, files);
    }
};

window.chatActions = {
    fillModel,
    editStart,
    editCancel,
    editPickFiles,
    editFilesPicked,
    editDragOver,
    editDragLeave,
    editDrop,
    editPaste,
};
