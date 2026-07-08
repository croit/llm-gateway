// Voice-conversation mode — the modal call surface.
//
// The composer's waveform button calls `open()`, which shows the `<voice-modal>`
// dialog (native `showModal()`, same pattern as the feedback widget) and speaks
// a greeting. Inside the modal:
//
//   - a state-reflecting control (`#voice-control`) that is tap-to-talk: tap to
//     start listening, tap to stop + send; tap while the assistant speaks to
//     interrupt (stop playback);
//   - a status line + live You/AI captions;
//   - `data-voice-state` (idle|listening|working|speaking) on the dialog drives
//     the control animation + status text via CSS.
//
// The turn itself is unchanged: transcript → normal `/chat/{id}/messages`
// submit → streamed into the conversation *behind* the modal, so closing it
// leaves the whole exchange as ordinary chat turns. This module is purely the
// mic capture + TTS playback + modal presentation over that existing flow.
//
// v1 is tap-to-talk; hands-free (VAD auto-submit) is a later phase and slots
// into the same modal.

import { startRecording, recordingUnavailableReason, type VoiceRecorder } from '../voice-recorder.js';

type VoiceState = 'idle' | 'listening' | 'working' | 'speaking';

let recorder: VoiceRecorder | null = null;
let awaitingReply = false;   // submitted, reply not finished streaming yet
let spokenLang = '';         // language the user last spoke (for TTS voice)

const modal = (): HTMLDialogElement | null =>
    document.getElementById('voice-modal') as HTMLDialogElement | null;
const isOpen = (): boolean => !!modal()?.open;

// ---- state + captions ------------------------------------------------------

const setState = (s: VoiceState): void => {
    const m = modal();
    if (!m) return;
    m.dataset.voiceState = s;
    const status = document.getElementById('voice-status');
    if (!status) return;
    // Status text lives in data-* on the dialog (server-owned i18n).
    const d = m.dataset;
    const txt =
        s === 'listening' ? `${d.txtListening ?? ''} · ${d.txtSend ?? ''}`
        : s === 'working' ? (d.txtWorking ?? '')
        : s === 'speaking' ? `${d.txtSpeaking ?? ''} · ${d.txtInterrupt ?? ''}`
        : (d.txtIdle ?? '');
    status.textContent = txt;
};

// Derive the state from the live flags (single source of truth).
const refreshState = (): void => {
    if (recorder) setState('listening');
    else if (playing) setState('speaking');
    else if (awaitingReply) setState('working');
    else setState('idle');
};

const setCaption = (who: 'user' | 'ai', text: string): void => {
    const el = document.getElementById(who === 'user' ? 'voice-cap-user' : 'voice-cap-ai');
    if (el) el.textContent = text;
};

// ---- TTS playback queue (sentence-by-sentence, ordered) --------------------

const ttsQueue: string[] = [];
let playing = false;
const audio = new Audio();

const enqueueSpeech = (text: string): void => {
    const t = text.trim();
    if (t) ttsQueue.push(t);
    if (!playing) void playNext();
};

const playNext = async (): Promise<void> => {
    const next = ttsQueue.shift();
    if (next === undefined) {
        playing = false;
        refreshState();
        return;
    }
    playing = true;
    refreshState();
    try {
        const resp = await fetch('/api/v0/speech', {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify({ text: next, language: spokenLang }),
        });
        if (resp.status === 204 || !resp.ok) {
            if (!resp.ok && resp.status !== 204) {
                const msg = await resp.text().catch(() => '');
                if (msg) window.pushToast('error', msg.slice(0, 160));
            }
            return void playNext();
        }
        const url = URL.createObjectURL(await resp.blob());
        await new Promise<void>((resolve) => {
            audio.src = url;
            audio.onended = () => resolve();
            audio.onerror = () => resolve();
            void audio.play().catch(() => resolve());
        });
        URL.revokeObjectURL(url);
    } catch (err) {
        window.pushToast('error', `speech playback: ${err}`);
    }
    void playNext();
};

const stopPlayback = (): void => {
    ttsQueue.length = 0;
    audio.pause();
    audio.src = '';
    playing = false;
};

// ---- live frequency-bars visualizer ---------------------------------------
//
// Draws real audio-reactive bars in the control: off the mic while listening
// (the recorder's AnalyserNode) and off the TTS audio while speaking (a
// MediaElementSource analyser on the shared `audio` element). Idle → a calm
// breathing animation. Colour follows the control's CSS `color`, which is set
// per state, so it stays theme- and state-aware.

let vizCtx: AudioContext | null = null;
let ttsAnalyser: AnalyserNode | null = null;
let vizRaf = 0;
const VIZ_BARS = 18;

// Lazily wire the TTS audio element through an analyser. Must be created once
// (a MediaElementSource can only be made once per element) and needs a resumed
// context — open() is a user gesture, so we set it up there.
const ensureTtsAnalyser = (): void => {
    try {
        if (!vizCtx) vizCtx = new AudioContext();
        if (vizCtx.state === 'suspended') void vizCtx.resume();
        if (!ttsAnalyser) {
            const src = vizCtx.createMediaElementSource(audio);
            ttsAnalyser = vizCtx.createAnalyser();
            ttsAnalyser.fftSize = 64;
            ttsAnalyser.smoothingTimeConstant = 0.7;
            src.connect(ttsAnalyser);
            ttsAnalyser.connect(vizCtx.destination); // keep the sound audible
        }
    } catch { /* no Web Audio → viz just runs idle; audio still plays */ }
};

const drawViz = (): void => {
    const canvas = document.getElementById('voice-viz') as HTMLCanvasElement | null;
    if (!canvas || !isOpen()) { vizRaf = 0; return; }
    const g = canvas.getContext('2d');
    if (!g) { vizRaf = 0; return; }
    const w = canvas.width;
    const h = canvas.height;
    g.clearRect(0, 0, w, h);
    g.fillStyle = getComputedStyle(canvas).color || '#8888aa';

    // Active source: mic while recording, else TTS while speaking, else none.
    const analyser = recorder?.analyser ?? (playing ? ttsAnalyser : null);
    let data: Uint8Array<ArrayBuffer> | null = null;
    if (analyser) {
        data = new Uint8Array(new ArrayBuffer(analyser.frequencyBinCount));
        analyser.getByteFrequencyData(data);
    }

    const gap = w / (VIZ_BARS * 3);
    const barW = (w - gap * (VIZ_BARS + 1)) / VIZ_BARS;
    const mid = h / 2;
    const now = performance.now();
    for (let i = 0; i < VIZ_BARS; i++) {
        let mag: number;
        if (data) {
            // Spread across the lower-mid bins (where speech energy sits).
            const idx = Math.floor((i / VIZ_BARS) * (data.length * 0.8));
            mag = (data[idx] ?? 0) / 255;
        } else {
            // Idle: a gentle breathing wave so it never looks frozen.
            mag = 0.10 + 0.05 * (0.5 + 0.5 * Math.sin(now / 320 + i * 0.5));
        }
        const barH = Math.max(barW, mag * h * 0.9);
        const x = gap + i * (barW + gap);
        const y = mid - barH / 2;
        g.beginPath();
        g.roundRect(x, y, barW, barH, barW / 2);
        g.fill();
    }
    vizRaf = requestAnimationFrame(drawViz);
};

const startViz = (): void => {
    if (!vizRaf) vizRaf = requestAnimationFrame(drawViz);
};
const stopViz = (): void => {
    if (vizRaf) cancelAnimationFrame(vizRaf);
    vizRaf = 0;
};

// ---- reply observation: peel sentences off the new bubble + caption it -----

let replyObserver: MutationObserver | null = null;
let spokenChars = 0;

const speakableText = (bubble: HTMLElement): string => {
    const clone = bubble.cloneNode(true) as HTMLElement;
    clone
        .querySelectorAll('pre, code, table, .chat-msg__actions, .thinking-block, .tool-call, .tool-calls-group, .chat-attachment')
        .forEach((n) => n.remove());
    return (clone.innerText || '').replace(/\s+/g, ' ').trim();
};

const splitSentences = (text: string): { done: string[]; rest: string } => {
    const done: string[] = [];
    const re = /[^.!?…]*[.!?…]+["')\]]*\s+/g;
    let last = 0;
    let m: RegExpExecArray | null;
    while ((m = re.exec(text)) !== null) {
        done.push(m[0].trim());
        last = re.lastIndex;
    }
    return { done, rest: text.slice(last) };
};

const observeReply = (): void => {
    const conv = document.getElementById('conversation');
    if (!conv) return;
    replyObserver?.disconnect();
    spokenChars = 0;
    // Only read a bubble that appears AFTER submit — otherwise we latch onto the
    // previous turn's reply and speak that first.
    const priorAssistants = conv.querySelectorAll(':scope > .chat-msg--assistant').length;

    const flush = (final: boolean): void => {
        const bubbles = conv.querySelectorAll<HTMLElement>(':scope > .chat-msg--assistant');
        if (bubbles.length <= priorAssistants) return;
        const bubble = bubbles[bubbles.length - 1];
        if (!bubble) return;
        const text = speakableText(bubble);
        if (text) setCaption('ai', text);
        if (text.length < spokenChars) return;
        const fresh = text.slice(spokenChars);
        const { done, rest } = splitSentences(fresh);
        for (const s of done) enqueueSpeech(s);
        spokenChars += fresh.length - rest.length;
        if (final || bubble.querySelector('.chat-msg__actions')) {
            if (rest.trim()) {
                enqueueSpeech(rest);
                spokenChars += rest.length;
            }
            replyObserver?.disconnect();
            replyObserver = null;
            awaitingReply = false;
            refreshState();
        }
    };

    replyObserver = new MutationObserver(() => flush(false));
    replyObserver.observe(conv, { childList: true, subtree: true, characterData: true });
    flush(false);
};

// ---- talk (tap-to-toggle) --------------------------------------------------

const talk = async (btn: HTMLElement): Promise<void> => {
    if (!isOpen()) return;

    // Recording → stop, transcribe, submit.
    if (recorder) {
        const current = recorder;
        recorder = null;
        refreshState();
        let wav: Blob;
        try {
            wav = await current.stop();
        } catch (err) {
            window.pushToast('error', `recording stop failed: ${err}`);
            return;
        }
        if (wav.size) await transcribeAndSubmit(wav);
        return;
    }

    // Assistant speaking → tap interrupts (stop playback); mic stays free.
    if (playing) {
        stopPlayback();
        refreshState();
        return;
    }

    // Idle → start listening.
    const unavailable = recordingUnavailableReason();
    if (unavailable) {
        window.pushToast('error', unavailable);
        return;
    }
    try {
        recorder = await startRecording(null);
        refreshState();
    } catch (err) {
        window.pushToast('error', `mic: ${err}`);
    }
    void btn;
};

const transcribeAndSubmit = async (wav: Blob): Promise<void> => {
    setState('working');
    const modelSelect = document.querySelector<HTMLSelectElement>('[data-mic-model]');
    const fd = new FormData();
    fd.append('model', modelSelect ? modelSelect.value : '');
    fd.append('file', wav, 'recording.wav');
    // No `verbose_json` — Voxtral (and some realtime STT) reject it. We get
    // plain `{text}`; TTS then uses the pool's default (multilingual) voice.
    let data: { text?: string; language?: string };
    try {
        const resp = await fetch('/api/v0/transcriptions', { method: 'POST', body: fd });
        if (!resp.ok) {
            const raw = await resp.text();
            let msg = raw;
            try { msg = (JSON.parse(raw) as { error?: { message?: string } })?.error?.message || raw; } catch { /* raw */ }
            window.pushToast('error', msg.slice(0, 200));
            refreshState();
            return;
        }
        data = await resp.json();
    } catch (err) {
        window.pushToast('error', `network error: ${err}`);
        refreshState();
        return;
    }
    const text = (data.text ?? '').trim();
    if (!text) {
        setCaption('user', modal()?.dataset.txtNotcaught ?? '…');
        refreshState();
        return;
    }
    spokenLang = (data.language ?? '').toLowerCase().slice(0, 2);
    setCaption('user', text);
    setCaption('ai', '');
    submitVoiceTurn(text);
};

const submitVoiceTurn = (text: string): void => {
    const f = document.getElementById('chat-form') as HTMLFormElement | null;
    const input = document.getElementById('message') as HTMLTextAreaElement | null;
    const flag = document.getElementById('chat-voice-flag') as HTMLInputElement | null;
    if (!f || !input) return;
    input.value = text;
    input.dispatchEvent(new Event('input', { bubbles: true }));
    if (flag) flag.value = 'true';
    awaitingReply = true;
    refreshState();
    observeReply();
    if (typeof f.requestSubmit === 'function') f.requestSubmit();
    else f.submit();
    setTimeout(() => { if (flag) flag.value = 'false'; }, 0);
};

// ---- open / close ----------------------------------------------------------

const open = (): void => {
    const m = modal();
    if (!m || m.open) return;
    m.showModal();
    // Reset transient state + captions.
    ttsQueue.length = 0;
    playing = false;
    awaitingReply = false;
    spokenChars = 0;
    setCaption('user', '');
    setCaption('ai', '');
    spokenLang = (document.documentElement.lang || 'en').toLowerCase().slice(0, 2);
    refreshState();
    // Wire the TTS analyser (open() is a user gesture → the audio context can
    // start) and kick off the visualizer.
    ensureTtsAnalyser();
    startViz();
    // Greet in the UI language.
    const greeting = m.dataset.voiceGreeting || '';
    if (greeting) enqueueSpeech(greeting);
    // Native dialog close (Esc / backdrop) → tear down.
    m.addEventListener('close', teardown, { once: true });
};

const teardown = (): void => {
    stopViz();
    stopPlayback();
    replyObserver?.disconnect();
    replyObserver = null;
    awaitingReply = false;
    if (recorder) { void recorder.stop().catch(() => {}); recorder = null; }
};

const close = (): void => {
    const m = modal();
    if (m?.open) m.close();   // fires 'close' → teardown
    else teardown();
};

// Space = tap-to-talk while the modal is open (but not when typing elsewhere).
document.addEventListener('keydown', (e) => {
    if (e.key !== ' ' || !isOpen()) return;
    const tag = (e.target as HTMLElement)?.tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA') return;
    e.preventDefault();
    const ctl = document.getElementById('voice-control');
    if (ctl) void talk(ctl);
});

window.chatVoice = { open, close, talk };
