// Feedback widget client.
//
// Wires the static FAB + `<dialog>` rendered by `pages/feedback.rs`:
//   1. On open, capture a viewport screenshot (DOM re-render via
//      `modern-screenshot`, supersampled to the device pixel ratio — NOT the
//      pixel-perfect getDisplayMedia path) and load it into the canvas
//      annotator so the user can mark it up (rectangle / arrow / pen / text).
//   2. Optional voice → fields: record audio (shared recorder), transcribe via
//      `/api/v0/transcriptions` (using the operator-configured voice model),
//      then turn the transcript into structured fields via `/feedback/extract`
//      (operator-configured text model — the user never picks a model).
//   3. Submit: POST the form + the annotated screenshot (base64) as JSON to
//      `/feedback`, which files a GitHub issue.
//
// The FAB + dialog are siblings of `<main>`, so they persist across Datastar
// SPA navigation. This module runs once at full-page load (bundled into
// `app.js`); its listeners stay bound for the life of the page.

import { snapdom } from '@zumer/snapdom';
import { createAnnotator, type AnnotatorTool } from './feedback-annotator.js';
import { getConsoleLogs, getNetworkLogs } from './feedback-capture.js';
import {
    recordingErrorMessage,
    recordingUnavailableReason,
    startRecording,
    type VoiceRecorder,
} from './voice-recorder.js';

const $ = <T extends HTMLElement>(id: string): T | null =>
    document.getElementById(id) as T | null;

// Capture at the device pixel ratio (crisp, matches the screen), capped at 3×
// to bound the output size on very high-DPR displays.
const captureScale = (): number => {
    const dpr = window.devicePixelRatio;
    if (!Number.isFinite(dpr) || dpr <= 0) return 1;
    return Math.min(Math.ceil(dpr), 3);
};

// Resolve the page background so transparent regions don't render black.
const backgroundColor = (): string => {
    const pick = (el: Element | null): string => {
        if (!el) return '';
        const c = getComputedStyle(el).backgroundColor;
        return c && c !== 'rgba(0, 0, 0, 0)' && c !== 'transparent' ? c : '';
    };
    return pick(document.documentElement) || pick(document.body) || '#ffffff';
};

// The widget's own chrome + the toast layer must never appear in the shot.
const SHOT_EXCLUDE = ['[data-feedback-fab]', '[data-feedback-dialog]', '#toasts'];

const init = (): void => {
    const fab = $<HTMLButtonElement>('feedback-fab');
    const dialog = $<HTMLDialogElement>('feedback-dialog');
    const form = $<HTMLFormElement>('feedback-form');
    if (!fab || !dialog || !form) return; // not an authed page with the widget

    const confirmDialog = $<HTMLDialogElement>('feedback-confirm');

    const titleEl = $<HTMLInputElement>('feedback-title');
    const descEl = $<HTMLTextAreaElement>('feedback-description');
    const businessEl = $<HTMLTextAreaElement>('feedback-business');
    const acceptanceEl = $<HTMLTextAreaElement>('feedback-acceptance');
    const priorityEl = $<HTMLSelectElement>('feedback-priority');
    const submitBtn = $<HTMLButtonElement>('feedback-submit');

    const voiceBtn = $<HTMLButtonElement>('feedback-voice-btn');
    const voiceLabel = voiceBtn?.querySelector<HTMLElement>('.feedback-voice-label') ?? null;

    const shotStatus = $<HTMLSpanElement>('feedback-shot-status');
    const shotRecapture = $<HTMLButtonElement>('feedback-shot-recapture');
    const shotRemove = $<HTMLButtonElement>('feedback-shot-remove');
    const shotWrap = $<HTMLDivElement>('feedback-shot-wrap');
    const toolbar = $<HTMLDivElement>('feedback-annot-toolbar');
    const canvas = $<HTMLCanvasElement>('feedback-shot-canvas');

    const logBrowserCb = $<HTMLInputElement>('feedback-log-browser');
    const logChatCb = $<HTMLInputElement>('feedback-log-chat');
    const logChatWrap = $<HTMLElement>('feedback-log-chat-wrap');

    const annotator = canvas ? createAnnotator(canvas) : null;

    // --- config gating ----------------------------------------------------
    // Model selection is operator-config only; the client just learns which
    // transcription model to attach to its recording upload.
    let voiceModel = '';
    fetch('/feedback/config')
        .then((r) => (r.ok ? r.json() : Promise.reject(new Error(String(r.status)))))
        .then((cfg: { enabled?: boolean; voice_enabled?: boolean; voice_model?: string }) => {
            if (cfg.enabled) fab.hidden = false;
            if (cfg.voice_enabled && voiceBtn) {
                voiceModel = cfg.voice_model ?? '';
                voiceBtn.hidden = false;
            }
        })
        .catch(() => { /* stays hidden — feature unconfigured or offline */ });

    // The user's granted tools — context for the issue. Fetched once,
    // best-effort, so `collectSystemInfo()` stays synchronous at submit.
    let allowedTools: string[] = [];
    fetch('/api/v0/me')
        .then((r) => (r.ok ? r.json() : Promise.reject(new Error(String(r.status)))))
        .then((me: { allowed_tools?: Array<{ id?: string; name?: string }> }) => {
            allowedTools = (me.allowed_tools ?? [])
                .map((t) => t.id || t.name || '')
                .filter(Boolean);
        })
        .catch(() => { /* tools context simply omitted */ });

    // --- screenshot + annotation ------------------------------------------
    const showShotStatus = (text: string): void => {
        if (shotStatus) shotStatus.textContent = text;
    };
    const showShot = (visible: boolean): void => {
        if (shotWrap) shotWrap.hidden = !visible;
        if (toolbar) toolbar.hidden = !visible;
    };

    // Native form controls (daisyUI toggle / checkbox / radio) capture blank —
    // their checked-state knob/dot/check is drawn by the control's `::before`
    // pseudo + box-shadow on an `appearance:none` input, which snapdom can't
    // read. Before capturing, stamp a stand-in over each one that COPIES the
    // control's real computed track style (background/border/radius) and its
    // `::before` knob/dot (size + colour), so the result is styled identically
    // to the live UI rather than approximated. Removed again afterwards.
    const stampControlStates = (): (() => void) => {
        const made: HTMLElement[] = [];
        const hidden: Array<{ el: HTMLElement; prev: string }> = [];
        // A div positioned exactly over `el`, carrying the control's own track
        // look. We also HIDE the real control (visibility:hidden keeps layout)
        // so snapdom captures only this overlay — otherwise the still-rendered
        // original track sits a sub-pixel off ours and you see a double border.
        const trackOverlay = (el: HTMLElement, cs: CSSStyleDeclaration): HTMLDivElement => {
            const r = el.getBoundingClientRect();
            const d = document.createElement('div');
            d.style.cssText =
                `position:absolute;left:${r.left + window.scrollX}px;top:${r.top + window.scrollY}px;`
                + `width:${r.width}px;height:${r.height}px;box-sizing:border-box;`
                + `background:${cs.backgroundColor};border:${cs.border};border-radius:${cs.borderRadius};`
                + 'pointer-events:none;z-index:2147483646;';
            document.body.appendChild(d);
            made.push(d);
            hidden.push({ el, prev: el.style.visibility });
            el.style.visibility = 'hidden';
            return d;
        };
        const controls = document.querySelectorAll<HTMLInputElement>(
            'input[type=checkbox], input[type=radio]',
        );
        for (const el of controls) {
            const r = el.getBoundingClientRect();
            if (r.width === 0 || r.height === 0) continue;
            const checked = el.checked;
            const cs = getComputedStyle(el);
            const before = getComputedStyle(el, '::before');
            const pad = parseFloat(cs.paddingLeft) || 3;

            const border = parseFloat(cs.borderLeftWidth) || 0;
            if (el.classList.contains('toggle')) {
                // Track + the knob (::before): copy its exact size, shape
                // (border-radius — a rounded square, NOT a full circle) and
                // colour, and place it `padding` from the active inner side.
                const box = trackOverlay(el, cs);
                const kw = parseFloat(before.width) || Math.max(4, r.height - 2 * pad);
                const kh = parseFloat(before.height) || kw;
                const innerW = r.width - 2 * border;
                const innerH = r.height - 2 * border;
                const left = checked ? innerW - kw - pad : pad;
                const top = (innerH - kh) / 2;
                const knob = document.createElement('div');
                knob.style.cssText =
                    `position:absolute;left:${left}px;top:${top}px;`
                    + `width:${kw}px;height:${kh}px;border-radius:${before.borderRadius || '9999px'};`
                    + `background:${before.backgroundColor};`;
                box.appendChild(knob);
            } else if (el.type === 'radio') {
                const box = trackOverlay(el, cs);
                if (checked) {
                    const dw = parseFloat(before.width) || Math.max(4, Math.round(r.width * 0.45));
                    const dh = parseFloat(before.height) || dw;
                    const dot = document.createElement('div');
                    dot.style.cssText =
                        `position:absolute;left:50%;top:50%;transform:translate(-50%,-50%);`
                        + `width:${dw}px;height:${dh}px;border-radius:${before.borderRadius || '9999px'};`
                        + `background:${before.backgroundColor};`;
                    box.appendChild(dot);
                }
            } else if (checked) {
                // Checkbox: track + a checkmark in the control's own colour.
                const box = trackOverlay(el, cs);
                box.style.color = cs.color;
                box.style.display = 'flex';
                box.style.alignItems = 'center';
                box.style.justifyContent = 'center';
                box.style.fontSize = `${Math.max(8, r.height - 4)}px`;
                box.style.lineHeight = '1';
                box.textContent = '✓';
            }
        }
        return () => {
            for (const d of made) d.remove();
            for (const { el, prev } of hidden) el.style.visibility = prev;
        };
    };

    // Capture the page as the user currently sees it, via snapdom. We capture
    // while the dialog is NOT open, so the shot is exactly "the UI at the
    // moment the user hit the button" (the FAB itself is excluded). snapdom
    // inlines per-element computed styles rather than embedding the page
    // stylesheet, so it renders this daisyUI/Tailwind-v4 UI correctly where
    // foreignObject-stylesheet libraries (modern-screenshot, html-to-image)
    // produce a blank image. `document.body` == the viewport here (the shell
    // is `h-dvh overflow-hidden`, so the body never scrolls).
    const capturePageDataUrl = async (): Promise<string | null> => {
        const restore = stampControlStates();
        try {
            const result = await Promise.race([
                snapdom(document.body, {
                    dpr: captureScale(),
                    backgroundColor: backgroundColor(),
                    exclude: SHOT_EXCLUDE,
                    excludeMode: 'remove',
                }),
                new Promise<never>((_, reject) =>
                    window.setTimeout(() => reject(new Error('timeout')), 10000),
                ),
            ]);
            const canvas = await result.toCanvas();
            if (!canvas.width || !canvas.height) return null;
            return canvas.toDataURL('image/png');
        } catch (_) {
            return null;
        } finally {
            restore();
        }
    };

    // Load a freshly captured page into the annotator (or show the failure
    // state). `annotator.reset()` must happen before the dialog is shown so the
    // canvas starts clean.
    const loadShot = async (dataUrl: string | null): Promise<void> => {
        if (!annotator) return;
        annotator.reset();
        if (dataUrl) {
            await annotator.loadDataUrl(dataUrl);
            showShot(true);
            showShotStatus('Attached — draw on it to annotate');
        } else {
            showShot(false);
            showShotStatus('Screenshot unavailable');
        }
    };

    // --- annotation toolbar ----------------------------------------------
    const toolBtns = Array.from(document.querySelectorAll<HTMLButtonElement>('.feedback-tool-btn'));
    const colorBtns = Array.from(document.querySelectorAll<HTMLButtonElement>('.feedback-color-btn'));
    const undoBtn = $<HTMLButtonElement>('feedback-undo');
    const redoBtn = $<HTMLButtonElement>('feedback-redo');
    const zoomReset = $<HTMLButtonElement>('feedback-zoom-reset');

    const syncToolbar = (): void => {
        if (!annotator) return;
        if (undoBtn) undoBtn.disabled = !annotator.canUndo();
        if (redoBtn) redoBtn.disabled = !annotator.canRedo();
        if (zoomReset) zoomReset.textContent = `${Math.round(annotator.getZoom() * 100)}%`;
    };
    annotator?.onChange(syncToolbar);

    const selectTool = (t: AnnotatorTool, btn: HTMLButtonElement): void => {
        annotator?.setTool(t);
        toolBtns.forEach((b) => b.classList.toggle('feedback-tool-btn--active', b === btn));
    };
    for (const btn of toolBtns) {
        btn.addEventListener('click', () => selectTool((btn.dataset.tool as AnnotatorTool) ?? 'rect', btn));
    }
    // Default tool is rectangle (the annotator's default) — mark it active.
    const rectBtn = toolBtns.find((b) => b.dataset.tool === 'rect') ?? toolBtns[0] ?? null;
    rectBtn?.classList.add('feedback-tool-btn--active');
    colorBtns[0]?.classList.add('feedback-color-btn--active');
    for (const btn of colorBtns) {
        btn.addEventListener('click', () => {
            annotator?.setColor(btn.dataset.color ?? '#ef4444');
            colorBtns.forEach((b) => b.classList.toggle('feedback-color-btn--active', b === btn));
        });
    }
    undoBtn?.addEventListener('click', () => annotator?.undo());
    redoBtn?.addEventListener('click', () => annotator?.redo());
    $('feedback-clear-annot')?.addEventListener('click', () => annotator?.clearAnnotations());
    $('feedback-zoom-in')?.addEventListener('click', () => annotator?.setZoom(annotator.getZoom() + 0.25));
    $('feedback-zoom-out')?.addEventListener('click', () => annotator?.setZoom(annotator.getZoom() - 0.25));
    zoomReset?.addEventListener('click', () => annotator?.setZoom(1));

    const closeDialog = (): void => {
        if (typeof dialog.close === 'function') dialog.close();
        else dialog.removeAttribute('open');
    };
    const showDialog = (): void => {
        if (typeof dialog.showModal === 'function') dialog.showModal();
        else dialog.setAttribute('open', '');
    };

    // Recapture: close the modal first so the page renders normally for the
    // grab, then reopen with the result.
    shotRecapture?.addEventListener('click', async () => {
        showShotStatus('Capturing…');
        closeDialog();
        await new Promise((r) => window.requestAnimationFrame(() => r(null)));
        const dataUrl = await capturePageDataUrl();
        showDialog();
        await loadShot(dataUrl);
    });
    shotRemove?.addEventListener('click', () => {
        annotator?.reset();
        showShot(false);
        showShotStatus('No screenshot');
    });

    // --- open / close -----------------------------------------------------
    const open = async (): Promise<void> => {
        // The chat & tool log option only makes sense on a chat page; reflect
        // the current page each time the dialog opens (survives SPA nav).
        if (logChatWrap) {
            logChatWrap.hidden = location.pathname.split('/').filter(Boolean)[0] !== 'chat';
        }
        // Capture BEFORE showing the modal — see `capturePageDataUrl`. This is
        // also why the shot reflects the exact pre-dialog UI state.
        showShot(false);
        showShotStatus('Capturing…');
        const dataUrl = await capturePageDataUrl();
        showDialog();
        await loadShot(dataUrl);
    };
    const close = closeDialog;

    fab.addEventListener('click', () => { void open(); });
    $('feedback-close')?.addEventListener('click', close);
    $('feedback-cancel')?.addEventListener('click', close);

    // --- voice → fields ---------------------------------------------------
    let voiceSession: VoiceRecorder | null = null;
    let voiceBusy = false;

    const setVoiceState = (state: 'idle' | 'recording' | 'working'): void => {
        if (!voiceBtn) return;
        voiceBtn.dataset.state = state;
        if (voiceLabel) {
            voiceLabel.textContent =
                state === 'recording' ? 'Stop & fill'
                : state === 'working' ? 'Transcribing…'
                : 'Fill in by voice';
        }
        voiceBtn.disabled = state === 'working';
    };

    const errorMessage = async (resp: Response): Promise<string> => {
        const raw = await resp.text();
        try {
            const parsed = JSON.parse(raw) as { error?: { message?: string } };
            return (parsed?.error?.message || raw).slice(0, 200);
        } catch (_) {
            return raw.slice(0, 200) || `request failed (${resp.status})`;
        }
    };

    const runExtraction = async (wav: Blob): Promise<void> => {
        setVoiceState('working');
        try {
            // 1. Transcribe via the existing VAD+Whisper endpoint, using the
            //    operator-configured voice model.
            const tForm = new FormData();
            tForm.append('model', voiceModel);
            tForm.append('file', wav, 'feedback.wav');
            const tResp = await fetch('/api/v0/transcriptions', { method: 'POST', body: tForm });
            if (!tResp.ok) { window.pushToast('error', await errorMessage(tResp)); return; }
            const transcript = ((await tResp.json()) as { text?: string }).text?.trim() ?? '';
            if (!transcript) { window.pushToast('error', 'No speech detected — try again.'); return; }
            // 2. Transcript → structured fields (text model is server-side config).
            const locale = document.documentElement.lang || navigator.language || '';
            const xResp = await fetch('/feedback/extract', {
                method: 'POST',
                headers: { 'content-type': 'application/json' },
                body: JSON.stringify({ transcript, locale }),
            });
            if (!xResp.ok) { window.pushToast('error', await errorMessage(xResp)); return; }
            const f = (await xResp.json()) as {
                title?: string; description?: string; business_value?: string;
                acceptance_criteria?: string; priority?: string;
            };
            if (titleEl && f.title) titleEl.value = f.title;
            if (descEl && f.description) descEl.value = f.description;
            if (businessEl && f.business_value) businessEl.value = f.business_value;
            if (acceptanceEl && f.acceptance_criteria) acceptanceEl.value = f.acceptance_criteria;
            if (priorityEl && f.priority) priorityEl.value = f.priority;
            window.pushToast('success', 'Filled from your recording — review and send.');
        } catch (err) {
            window.pushToast('error', `network error: ${err}`);
        } finally {
            setVoiceState('idle');
        }
    };

    voiceBtn?.addEventListener('click', async () => {
        if (voiceBusy) return;
        const unavailable = recordingUnavailableReason();
        if (unavailable) { window.pushToast('error', unavailable); return; }

        if (voiceSession) {
            const current = voiceSession;
            voiceSession = null;
            setVoiceState('idle');
            let wav: Blob;
            try {
                wav = await current.stop();
            } catch (err) {
                window.pushToast('error', `recording stop failed: ${err}`);
                return;
            }
            if (!wav.size) { window.pushToast('error', 'no audio captured'); return; }
            voiceBusy = true;
            try { await runExtraction(wav); } finally { voiceBusy = false; }
            return;
        }

        try {
            voiceSession = await startRecording(null);
            setVoiceState('recording');
        } catch (err) {
            window.pushToast('error', recordingErrorMessage(err));
        }
    });

    // --- diagnostics assembled into the issue (see feedback-capture.ts) ---
    // Network logs are sliced to the last 50 (the buffer caps at 100); console
    // is the full ≤100 buffer. Plus gateway-relevant context: the page/module,
    // the user's granted tools, and — on a chat page — the conversation tail.
    const collectSystemInfo = (): Record<string, unknown> => {
        const segments = location.pathname.split('/').filter(Boolean);
        // Default-on consent: include the logs unless the user unticked them.
        const includeBrowser = logBrowserCb ? logBrowserCb.checked : true;
        const includeChat = logChatCb ? logChatCb.checked : true;
        const info: Record<string, unknown> = {
            url: location.href,
            module: segments[0] ?? '',
            timestamp: new Date().toISOString(),
            browser: navigator.userAgent,
            language: navigator.language,
            screen_resolution: `${window.screen.width}x${window.screen.height}`,
            viewport_size: `${window.innerWidth}x${window.innerHeight}`,
            device_pixel_ratio: window.devicePixelRatio,
        };
        // Browser activity log (console + network), opt-out.
        if (includeBrowser) {
            info.console_logs = getConsoleLogs();
            info.network_logs = getNetworkLogs().slice(-50);
        }
        // Chat & tool usage log, opt-out, only on chat pages: the granted tools
        // + the session id and a bounded tail of the rendered conversation.
        if (includeChat && segments[0] === 'chat' && segments[1]) {
            info.allowed_tools = allowedTools;
            const conv = document.getElementById('conversation');
            const text = (conv?.innerText ?? '').trim();
            info.chat = {
                session_id: segments[1],
                transcript_tail: text ? text.slice(-4000) : '',
            };
        }
        return info;
    };

    // --- submit -----------------------------------------------------------
    const resetForm = (): void => {
        form.reset();
        if (priorityEl) priorityEl.value = 'medium';
        annotator?.reset();
        showShot(false);
        showShotStatus('No screenshot');
    };

    const closeConfirm = (): void => {
        if (!confirmDialog) return;
        if (typeof confirmDialog.close === 'function') confirmDialog.close();
        else confirmDialog.removeAttribute('open');
    };
    const showConfirm = (): void => {
        if (!confirmDialog) return;
        if (typeof confirmDialog.showModal === 'function') confirmDialog.showModal();
        else confirmDialog.setAttribute('open', '');
    };

    // The actual POST. Reached only after the user confirms the public-tracker
    // warning (or immediately, if the confirm dialog isn't present).
    const doSubmit = async (): Promise<void> => {
        if (!titleEl || !descEl) return;
        const title = titleEl.value.trim();
        const description = descEl.value.trim();

        if (submitBtn) submitBtn.disabled = true;
        const shotDataUrl = annotator?.hasImage() ? annotator.toDataUrl() : null;
        const payload = {
            title,
            description,
            business_value: businessEl?.value.trim() ?? '',
            acceptance_criteria: acceptanceEl?.value.trim() ?? '',
            priority: priorityEl?.value ?? 'medium',
            screenshot_base64: shotDataUrl ? shotDataUrl.split(',')[1] : undefined,
            system_info: collectSystemInfo(),
        };
        try {
            const resp = await fetch('/feedback', {
                method: 'POST',
                headers: { 'content-type': 'application/json' },
                body: JSON.stringify(payload),
            });
            if (!resp.ok) {
                const raw = await resp.text();
                let msg = raw;
                try { msg = (JSON.parse(raw) as { error?: { message?: string } })?.error?.message || raw; } catch (_) { /* */ }
                window.pushToast('error', msg.slice(0, 200));
                return;
            }
            const data = (await resp.json()) as { number?: number; url?: string };
            window.pushToast('success', data.number ? `Thanks! Filed issue #${data.number}.` : 'Thanks for the feedback!');
            resetForm();
            close();
        } catch (err) {
            window.pushToast('error', `network error: ${err}`);
        } finally {
            if (submitBtn) submitBtn.disabled = false;
        }
    };

    // Submit gate: validate first (so the user isn't asked to confirm only to
    // hit a validation error), then show the public-tracker warning. The form
    // is NOT sent here — only after the user confirms.
    form.addEventListener('submit', (ev) => {
        ev.preventDefault();
        if (!titleEl || !descEl) return;
        const title = titleEl.value.trim();
        const description = descEl.value.trim();
        if (title.length < 4) { window.pushToast('error', 'Please add a short title.'); return; }
        if (!description) { window.pushToast('error', 'Please describe the feedback.'); return; }
        // No confirm dialog rendered → send directly (graceful fallback).
        if (!confirmDialog) { void doSubmit(); return; }
        showConfirm();
    });

    // "No, let me edit" / Esc / backdrop: dismiss the warning, leave the
    // feedback dialog open so the user can edit the ticket.
    $('feedback-confirm-cancel')?.addEventListener('click', closeConfirm);
    confirmDialog?.addEventListener('cancel', () => closeConfirm());
    // "Yes, send": close the warning and fire the POST.
    $('feedback-confirm-ok')?.addEventListener('click', () => {
        closeConfirm();
        void doSubmit();
    });

    syncToolbar();
};

if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
} else {
    init();
}
