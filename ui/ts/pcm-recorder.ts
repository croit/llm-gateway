// AudioWorkletProcessor that ships raw mono PCM samples back to the
// main thread. Lives in its own file because AudioWorklet's
// `addModule()` only takes a URL — the spec doesn't let us inline
// processor source via a Blob+ObjectURL on every browser.
//
// The processor copies each render-quantum's worth of samples
// (typically 128 frames at the AudioContext's sample rate) into a
// transferable Float32Array and posts it on `this.port`. The main
// thread accumulates these and, on stop, packs them into a WAV header
// before uploading to /api/v0/transcriptions.
//
// We intentionally do not buffer or aggregate frames here — the worklet
// has a tiny audio-thread budget, and `postMessage` of a Float32Array
// is cheap (the underlying ArrayBuffer is transferred, not copied).

// Ambient declarations for the AudioWorklet global scope. TypeScript's
// lib.dom.d.ts doesn't include AudioWorklet processor types (those
// only exist inside the AudioWorkletGlobalScope, which is a separate
// JS realm). One small dep we don't need.
declare class AudioWorkletProcessor {
    readonly port: MessagePort;
    constructor(options?: AudioWorkletNodeOptions);
}
declare function registerProcessor(
    name: string,
    processor: new (options?: AudioWorkletNodeOptions) => AudioWorkletProcessor,
): void;

class PcmRecorder extends AudioWorkletProcessor {
    process(inputs: Float32Array[][]): boolean {
        const channel = inputs[0] && inputs[0][0];
        if (channel && channel.length) {
            // .slice() because the underlying buffer is reused by the
            // graph on the next quantum — without the copy the main
            // thread would observe overwritten samples.
            this.port.postMessage(channel.slice());
        }
        return true;
    }
}

registerProcessor('pcm-recorder', PcmRecorder);
