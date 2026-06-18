// Voice composer.
//
// The chat page renders a mic button with
// `data-on:click="window.chatMic.toggle(el)"`. Each click toggles a
// single recording session: first click starts the AudioWorklet
// capture chain, second click stops it, encodes the buffered samples
// as 16 kHz mono PCM WAV, and uploads to `/api/v0/transcriptions`.
// The transcribed text is appended to the composer's `#message`
// input.
//
// We capture raw PCM via the Web Audio API + AudioWorklet rather
// than MediaRecorder. The reason is server-side: the
// `/api/v0/transcriptions` handler (proxy.rs) runs the upload through
// `vad.rs`, which uses a neural VAD (earshot) to strip leading/
// trailing silence and clip long internal pauses before forwarding
// to Whisper. That VAD operates on raw PCM, and Opus-in-WebM has no
// pure-Rust decoder yet, so we let the browser hand us the samples
// directly.
//
// Quality knobs (every one is load-bearing for transcript quality):
//   * echoCancellation / noiseSuppression / autoGainControl — the
//     three WebRTC DSP filters. Off-by-default browser baselines
//     send HVAC hum + room reverb straight into Whisper.
//   * AudioContext({ sampleRate: 16000 }) — Whisper's native rate,
//     so the server VAD doesn't have to resample. Safari and some
//     Android browsers force the device's native rate; the worklet
//     handler resamples to 16 kHz in that case via linear
//     interpolation (fine for VAD + Whisper, which both downsample
//     internally anyway).
//   * No prompt / no language / no temperature override — the
//     upstream's defaults (auto-detect language, 0 temperature with
//     a fallback ladder) outperform every clever override we tried.

const TARGET_RATE = 16000;

/**
 * Cheap linear resampler. Fine for both earshot (it's robust to
 * minor reconstruction artefacts) and Whisper (which mel-spec's the
 * input anyway). A polyphase filter would be ~10× the code for sub-
 * perceptual quality gain in this pipeline.
 */
const resampleTo16k = (samples: Float32Array, fromRate: number): Float32Array => {
    if (fromRate === TARGET_RATE) return samples;
    const ratio = fromRate / TARGET_RATE;
    const outLen = Math.floor(samples.length / ratio);
    const out = new Float32Array(outLen);
    for (let i = 0; i < outLen; i++) {
        const src = i * ratio;
        const lo = Math.floor(src);
        const hi = Math.min(lo + 1, samples.length - 1);
        const t = src - lo;
        out[i] = samples[lo]! * (1 - t) + samples[hi]! * t;
    }
    return out;
};

/**
 * Pack a Float32 sample buffer (range -1..1) into a 16 kHz mono
 * 16-bit PCM WAV (44-byte canonical header). Matches the format
 * `rama_server::vad::parse_pcm16_mono_16k` expects — diff-by-byte
 * if the two ever drift.
 */
const encodeWav = (samples: Float32Array): Blob => {
    const numSamples = samples.length;
    const dataSize = numSamples * 2;
    const buf = new ArrayBuffer(44 + dataSize);
    const view = new DataView(buf);
    const writeStr = (offset: number, s: string): void => {
        for (let i = 0; i < s.length; i++) view.setUint8(offset + i, s.charCodeAt(i));
    };
    writeStr(0, 'RIFF');
    view.setUint32(4, 36 + dataSize, true);
    writeStr(8, 'WAVE');
    writeStr(12, 'fmt ');
    view.setUint32(16, 16, true);
    view.setUint16(20, 1, true);                 // PCM
    view.setUint16(22, 1, true);                 // mono
    view.setUint32(24, TARGET_RATE, true);
    view.setUint32(28, TARGET_RATE * 2, true);   // byte rate
    view.setUint16(32, 2, true);                 // block align
    view.setUint16(34, 16, true);                // bits per sample
    writeStr(36, 'data');
    view.setUint32(40, dataSize, true);
    let offset = 44;
    for (let i = 0; i < numSamples; i++) {
        const s = Math.max(-1, Math.min(1, samples[i]!));
        view.setInt16(offset, s < 0 ? s * 0x8000 : s * 0x7fff, true);
        offset += 2;
    }
    return new Blob([buf], { type: 'audio/wav' });
};

/**
 * Single-recording session. Owns the AudioContext, mic stream,
 * worklet node, and the buffered Float32 chunks. `stop()` returns
 * the encoded WAV blob and tears everything down.
 */
class Session {
    private readonly chunks: Float32Array[] = [];
    private readonly captureRate: number;

    constructor(
        private readonly ctx: AudioContext,
        private readonly stream: MediaStream,
        private readonly source: MediaStreamAudioSourceNode,
        private readonly node: AudioWorkletNode,
        private readonly sink: GainNode,
        private readonly meterEl: HTMLElement | null,
    ) {
        this.captureRate = ctx.sampleRate;
        this.node.port.onmessage = (e: MessageEvent): void => {
            if (!(e.data instanceof Float32Array)) return;
            this.chunks.push(e.data);
            if (this.meterEl) {
                const samples = e.data;
                let sumSq = 0;
                for (let i = 0; i < samples.length; i++) {
                    sumSq += samples[i]! * samples[i]!;
                }
                const rms = Math.sqrt(sumSq / Math.max(samples.length, 1));
                const norm = Math.min(1, rms * 6);
                this.meterEl.style.setProperty('--vol', norm.toFixed(3));
            }
        };
    }

    async stop(): Promise<Blob> {
        try {
            this.node.port.onmessage = null;
            this.source.disconnect();
            this.node.disconnect();
            this.sink.disconnect();
            this.stream.getTracks().forEach((t) => t.stop());
            await this.ctx.close();
        } catch (_) { /* tear-down is best-effort */ }
        let total = 0;
        for (const c of this.chunks) total += c.length;
        const flat = new Float32Array(total);
        let off = 0;
        for (const c of this.chunks) {
            flat.set(c, off);
            off += c.length;
        }
        const at16k = resampleTo16k(flat, this.captureRate);
        return encodeWav(at16k);
    }
}

let session: Session | null = null;

const startRecording = async (meterEl: HTMLElement | null): Promise<Session> => {
    const stream = await navigator.mediaDevices.getUserMedia({
        audio: {
            echoCancellation: true,
            noiseSuppression: true,
            autoGainControl: true,
            channelCount: 1,
            sampleRate: TARGET_RATE,
        },
    });
    const ctx = new AudioContext({ sampleRate: TARGET_RATE });
    const pcmRecorderUrl =
        document.querySelector<HTMLScriptElement>('script[data-pcm-recorder]')
            ?.dataset.pcmRecorder
        ?? '/assets/pcm-recorder.js';
    try {
        await ctx.audioWorklet.addModule(pcmRecorderUrl);
    } catch (err) {
        await ctx.close().catch(() => {});
        stream.getTracks().forEach((t) => t.stop());
        throw err;
    }
    const source = ctx.createMediaStreamSource(stream);
    const node = new AudioWorkletNode(ctx, 'pcm-recorder');
    // The processor only runs when there's an active path from a
    // source to the destination — a node with no outgoing connection
    // is treated as dead code by the audio engine and `process()` is
    // never called. Route `source → node → muted_gain → destination`
    // so the processor is scheduled; gain of 0 so the mic isn't
    // played back through the speakers (which would cause an echo
    // loop).
    source.connect(node);
    const muted = ctx.createGain();
    muted.gain.value = 0;
    node.connect(muted).connect(ctx.destination);
    return new Session(ctx, stream, source, node, muted, meterEl);
};

const toggle = async (micBtn: HTMLElement): Promise<void> => {
    const messageInput = document.getElementById('message') as HTMLTextAreaElement | null;
    const modelSelect = document.querySelector<HTMLSelectElement>('[data-mic-model]');
    const meterEl = document.querySelector<HTMLElement>('[data-mic-meter]');
    if (!messageInput) return;

    // Capability checks happen here (not at module init) so a fresh
    // mount doesn't need to re-run them; each click re-verifies the
    // environment.
    if (!navigator.mediaDevices || !window.isSecureContext) {
        window.pushToast(
            'error',
            'Voice recording requires HTTPS or localhost — disabled on plain http.',
        );
        return;
    }
    if (!(window.AudioContext && 'audioWorklet' in AudioContext.prototype)) {
        window.pushToast('error', 'Voice recording requires AudioWorklet support.');
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

    // While the transcription HTTP request is in flight, a fresh
    // click would otherwise fall through to the "start recording"
    // branch (session is already null at that point) and clobber the
    // in-flight upload. Bail early — the spinner icon on the button
    // is the visible signal that the user should wait.
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
        // Spinner icon + click-guard for the duration of the POST.
        // Cleared in `finally` so a network error or unexpected throw
        // can't leave the button stuck in the spinning state.
        setTranscribing(true);
        try {
            const resp = await fetch('/api/v0/transcriptions', {
                method: 'POST',
                body: form,
            });
            if (!resp.ok) {
                // Server uses the OpenAI-style error envelope
                // (`{"error": {"message": "...", ...}}`); prefer the
                // human message over the raw JSON when present so the
                // toast reads cleanly. Falls back to the raw text for
                // unexpected upstream shapes.
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
        const name = (err instanceof Error && err.name) || 'Error';
        let msg: string;
        if (name === 'NotAllowedError' || name === 'PermissionDeniedError') {
            msg = 'Microphone access denied. Allow it in the browser and retry.';
        } else if (name === 'NotFoundError' || name === 'DevicesNotFoundError') {
            msg = 'No microphone found.';
        } else if (name === 'NotReadableError') {
            msg = 'Microphone is busy — another app may be using it.';
        } else {
            const message = err instanceof Error ? err.message : String(err);
            msg = `Mic error: ${message}`;
        }
        window.pushToast('error', msg);
    }
};

window.chatMic = { toggle };
