// Window augmentations for the gateway-only TS modules. The
// chat-composer + chat-scroll surfaces are declared in
// `crates/session-core/ui/ts/global.d.ts` and merged in via the
// `include` glob in `tsconfig.json`. TypeScript merges multiple
// `interface Window { … }` blocks across files in the same
// compilation, so the resulting `window` type carries every key
// regardless of which file declared it.

export {};

declare global {
    type ToastKind = 'success' | 'error' | 'info';

    interface Window {
        /** Render a transient toast into the `#toasts` region. */
        pushToast(kind: ToastKind, message: string): void;
        /**
         * Copy a referenced element's textContent to the clipboard.
         * Called from `data-on:click="window.uiCopy(el)"` on any
         * button that carries `data-copy-target="#selector"`.
         */
        uiCopy(btn: HTMLElement): Promise<void>;
        /**
         * Voice composer entry. The chat-page mic button calls
         * `toggle(el)` on each click — first click starts the
         * AudioWorklet capture chain, second stops it and uploads
         * the buffered samples as 16 kHz PCM WAV. Owned by
         * `ui/ts/chat/mic.ts`.
         */
        chatMic: {
            toggle(btn: HTMLElement): Promise<void>;
        };
        /**
         * Per-message retry/edit glue for the chat page. Called from
         * the bubbles' `data-on:*` directives. Owned by
         * `ui/ts/chat/actions.ts`.
         */
        chatActions: {
            fillModel(form: HTMLFormElement): boolean;
            editStart(turnId: string): void;
            editCancel(turnId: string): void;
        };
        /**
         * Browser geolocation sharing. `share(btn)` requests the
         * position and POSTs it to `/api/v0/me/location`; `forget(btn)`
         * clears it. Called from `data-on:click` on the `/tools`
         * "share location" / "stop sharing" buttons. Owned by
         * `ui/ts/geo.ts`.
         */
        geo: {
            share(btn?: HTMLElement): Promise<{ ok: boolean; reason?: string }>;
            forget(btn?: HTMLElement): Promise<void>;
            /** Chat prompt "Share location": deliver the position to the
             *  parked `get_user_location` tool for `turnId`. */
            shareForTurn(turnId: string): Promise<void>;
            /** Chat prompt "Not now": tell the parked tool to stop waiting. */
            declineForTurn(turnId: string): Promise<void>;
        };
    }
}
