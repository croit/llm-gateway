// Window augmentations for session-core's reusable chat-form
// helpers. The gateway imports composer.ts + scroll.ts as
// side-effect modules; this file teaches the type checker about
// the surfaces they attach to `window` so `data-on:*` / `data-init`
// expressions in the server-rendered HTML stay typed.
//
// The bin's own d.ts adds any extra `Window` keys (e.g. gateway's
// `chatMic`, `uiCopy`, `pushToast`); TypeScript merges the
// declarations across files in the same compilation.

export {};

declare global {
    interface Window {
        /**
         * Chat composer entry points called from Datastar `data-on:*`
         * attributes on the chat form / textarea. Owned by
         * `crates/session-core/ui/ts/composer.ts`.
         */
        chatComposer: {
            /** Called from `data-on:submit__prevent` on `#chat-form`.
             *  Returns false to abort the @post (empty / streaming). */
            onSubmit(evt: Event): boolean;
            /** Called from `data-on:keydown` on `#message`. Handles
             *  the desktop Enter-to-submit gesture. */
            onKeydown(evt: KeyboardEvent): void;
            /** Called from the conversation MutationObserver — clears
             *  the input on the first server response after a submit. */
            notifyConversationMutated(): void;
            /** Click handler for the chat-composer attach button.
             *  Opens the hidden `<input type="file" multiple>`. */
            openFilePicker(): void;
            /** Change handler for the hidden file input — files
             *  picked through the native dialog land here. */
            onFilesPicked(evt: Event): void;
            /** Dragover / dragleave / drop on the form: highlight
             *  the composer while a file is hovering, accept the
             *  dropped FileList. */
            onDragOver(evt: DragEvent): void;
            onDragLeave(evt: DragEvent): void;
            onDrop(evt: DragEvent): void;
            /** Paste handler on the form — grabs File objects from
             *  the clipboard payload (e.g. screenshots copied with
             *  Cmd-Shift-Ctrl-4 on macOS). */
            onPaste(evt: ClipboardEvent): void;
        };
        /**
         * Per-element initialiser for the conversation autoscroll +
         * mutation-driven composer-clear pipeline. Owned by
         * `crates/session-core/ui/ts/scroll.ts`. Wired via
         * `data-init="window.chatScroll.init(el)"` on the
         * `#conversation` section so it self-binds on every mount.
         */
        chatScroll: {
            init(conversation: HTMLElement): void;
            /** Arm the one-shot scroll-to-top for the message about to
             *  be sent. Called from the composer's submit handler. */
            onUserSend(): void;
        };
    }
}
