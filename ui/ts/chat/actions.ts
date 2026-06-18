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

window.chatActions = { fillModel, editStart, editCancel };
