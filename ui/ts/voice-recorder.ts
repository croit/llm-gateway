// Shared in-browser voice recorder.
//
// Extracted from the chat composer mic (`chat/mic.ts`) so both the chat
// composer AND the feedback widget capture audio the same way: raw PCM via
// the Web Audio API + AudioWorklet, resampled to 16 kHz mono and encoded as
// a canonical 44-byte WAV. We capture PCM (not MediaRecorder/Opus) because
// the server-side `/api/v0/transcriptions` handler runs the upload through a
// neural VAD (earshot) that operates on raw PCM16 — and there's no pure-Rust
// Opus decoder yet. See `crates/gateway/src/rama_server/vad.rs`.
//
// Quality knobs (each is load-bearing for transcript quality):
//   * echoCancellation / noiseSuppression / autoGainControl — the WebRTC DSP
//     filters; the browser defaults send HVAC hum + reverb into Whisper.
//   * AudioContext({ sampleRate: 16000 }) — Whisper's native rate. Safari and
//     some Android browsers force the device rate; the worklet handler then
//     resamples to 16 kHz via linear interpolation.

export const TARGET_RATE = 16000;

/**
 * Cheap linear resampler. Fine for both earshot (robust to minor
 * reconstruction artefacts) and Whisper (which mel-spec's the input anyway).
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
 * Pack a Float32 sample buffer (range -1..1) into a 16 kHz mono 16-bit PCM
 * WAV (44-byte canonical header). Matches the format
 * `rama_server::vad::parse_pcm16_mono_16k` expects — diff-by-byte if the two
 * ever drift.
 */
export const encodeWav = (samples: Float32Array): Blob => {
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
 * Single-recording session. Owns the AudioContext, mic stream, worklet node,
 * and the buffered Float32 chunks. `stop()` returns the encoded WAV blob and
 * tears everything down.
 */
export class VoiceRecorder {
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

/**
 * Capability check shared by every recorder consumer. Returns an error string
 * to toast, or `null` when recording is possible. Runs at click time (not
 * module init) so a fresh mount re-verifies the environment.
 */
export const recordingUnavailableReason = (): string | null => {
    if (!navigator.mediaDevices || !window.isSecureContext) {
        return 'Voice recording requires HTTPS or localhost — disabled on plain http.';
    }
    if (!(window.AudioContext && 'audioWorklet' in AudioContext.prototype)) {
        return 'Voice recording requires AudioWorklet support.';
    }
    return null;
};

/**
 * Start a recording session. `meterEl` (optional) receives a `--vol` CSS var
 * updated per audio frame for a level meter. Throws on mic-permission / device
 * errors — callers map `err.name` to a friendly message.
 */
export const startRecording = async (
    meterEl: HTMLElement | null = null,
): Promise<VoiceRecorder> => {
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
    // The processor only runs when there's an active path from a source to
    // the destination. Route `source → node → muted_gain → destination` so
    // `process()` is scheduled; gain of 0 so the mic isn't played back.
    source.connect(node);
    const muted = ctx.createGain();
    muted.gain.value = 0;
    node.connect(muted).connect(ctx.destination);
    return new VoiceRecorder(ctx, stream, source, node, muted, meterEl);
};

/**
 * Map a `getUserMedia` / worklet error to a user-facing message.
 */
export const recordingErrorMessage = (err: unknown): string => {
    const name = (err instanceof Error && err.name) || 'Error';
    if (name === 'NotAllowedError' || name === 'PermissionDeniedError') {
        return 'Microphone access denied. Allow it in the browser and retry.';
    }
    if (name === 'NotFoundError' || name === 'DevicesNotFoundError') {
        return 'No microphone found.';
    }
    if (name === 'NotReadableError') {
        return 'Microphone is busy — another app may be using it.';
    }
    const message = err instanceof Error ? err.message : String(err);
    return `Mic error: ${message}`;
};
