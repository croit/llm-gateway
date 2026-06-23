// Voice composer.
//
// The chat page renders a mic button with
// `data-on:click="window.chatMic.toggle(el)"`. Each click toggles a single
// recording session: first click starts the AudioWorklet capture chain,
// second click stops it, encodes the buffered samples as 16 kHz mono PCM WAV,
// and uploads to `/api/v0/transcriptions`. The transcribed text is appended
// to the composer's `#message` input.
//
// The recorder itself (PCM capture, WAV encoding, capability checks) lives in
// the shared `../voice-recorder.ts` module, reused by the feedback widget.
// This file owns only the chat-composer wiring: the #message target, the mic
// model select, the level meter, and the button busy/transcribing states.

import {
    recordingErrorMessage,
    recordingUnavailableReason,
    startRecording,
    type VoiceRecorder,
} from '../voice-recorder.js';

let session: VoiceRecorder | null = null;

const toggle = async (micBtn: HTMLElement): Promise<void> => {
    const messageInput = document.getElementById('message') as HTMLTextAreaElement | null;
    const modelSelect = document.querySelector<HTMLSelectElement>('[data-mic-model]');
    const meterEl = document.querySelector<HTMLElement>('[data-mic-meter]');
    if (!messageInput) return;

    const unavailable = recordingUnavailableReason();
    if (unavailable) {
        window.pushToast('error', unavailable);
        return;
    }

    const setBusy = (busy: boolean): void => {
        if (busy) micBtn.dataset.recording = '1';
        else delete micBtn.dataset.recording;
    };
    const setTranscribing = (busy: boolean): void => {
        if (busy) micBtn.dataset.transcribing = '1';
        else delete micBtn.dataset.transcribing;
    };

    // While the transcription HTTP request is in flight, a fresh click would
    // otherwise fall through to the "start recording" branch (session is
    // already null) and clobber the in-flight upload. Bail early — the spinner
    // icon is the signal to wait.
    if (micBtn.dataset.transcribing === '1') return;

    if (session) {
        const current = session;
        session = null;
        setBusy(false);
        if (meterEl) meterEl.style.setProperty('--vol', '0');
        let wav: Blob;
        try {
            wav = await current.stop();
        } catch (err) {
            window.pushToast('error', `recording stop failed: ${err}`);
            return;
        }
        if (!wav.size) {
            window.pushToast('error', 'no audio captured');
            return;
        }
        const form = new FormData();
        form.append('model', modelSelect ? modelSelect.value : '');
        form.append('file', wav, 'recording.wav');
        // Spinner icon + click-guard for the duration of the POST. Cleared in
        // `finally` so a network error can't leave the button stuck spinning.
        setTranscribing(true);
        try {
            const resp = await fetch('/api/v0/transcriptions', {
                method: 'POST',
                body: form,
            });
            if (!resp.ok) {
                // Server uses the OpenAI-style error envelope; prefer the human
                // message over the raw JSON when present.
                const raw = await resp.text();
                let msg = raw;
                try {
                    const parsed = JSON.parse(raw) as { error?: { message?: string } };
                    msg = parsed?.error?.message || raw;
                } catch (_) { /* not JSON, use raw */ }
                window.pushToast('error', msg.slice(0, 200));
                return;
            }
            const data = await resp.json() as { text?: string };
            const txt = (data.text ?? '').trim();
            if (txt) {
                const prev = messageInput.value;
                messageInput.value = prev ? `${prev} ${txt}` : txt;
                messageInput.focus();
                messageInput.dispatchEvent(new Event('input', { bubbles: true }));
            }
        } catch (err) {
            window.pushToast('error', `network error: ${err}`);
        } finally {
            setTranscribing(false);
        }
        return;
    }

    try {
        session = await startRecording(meterEl);
        setBusy(true);
    } catch (err) {
        window.pushToast('error', recordingErrorMessage(err));
    }
};

window.chatMic = { toggle };
