// Canvas screenshot annotator for the feedback widget.
//
// A faithful vanilla-TS port of yachtlistings2's `screenshot-annotator.tsx`:
// the captured screenshot is drawn into a <canvas> at its intrinsic
// (full-resolution) pixel size and displayed CSS-scaled; the user draws
// rectangle / arrow / freehand / text annotations over it. Everything is
// redrawn from a flat list of shapes each frame, with undo/redo history.
//
// Coordinate mapping is the load-bearing bit and is ported verbatim:
//   x = (clientX - rect.left) * canvas.width / rect.width
// `canvas.width` is the screenshot's natural pixel width; `rect.width` is the
// displayed CSS width. The ratio absorbs both DPR upscaling and the CSS zoom,
// so stored points are always in intrinsic image-pixel space → 1:1 export.
//
// Improvements over the source: Pointer Events (so touch works too) and a
// stroke width that scales with the image resolution (the source's fixed 3px
// renders hairline-thin on a DPR-2/3 capture).

export type AnnotatorTool = 'rect' | 'arrow' | 'pen' | 'text' | 'redact';

interface Point { x: number; y: number }

interface Shape {
    tool: AnnotatorTool;
    color: string;
    width: number;
    points?: Point[];
    start?: Point;
    end?: Point;
    text?: string;
    font?: number;
}

export interface Annotator {
    loadDataUrl(dataUrl: string): Promise<void>;
    hasImage(): boolean;
    setTool(t: AnnotatorTool): void;
    setColor(c: string): void;
    undo(): void;
    redo(): void;
    canUndo(): boolean;
    canRedo(): boolean;
    clearAnnotations(): void;
    setZoom(z: number): void;
    getZoom(): number;
    reset(): void;
    /** PNG data URL of the image + annotations, or null when no image. */
    toDataUrl(): string | null;
    /** Fired after any state change (shape added, undo, tool/zoom). */
    onChange(cb: () => void): void;
}

export const ANNOTATOR_COLORS = ['#ef4444', '#3b82f6', '#10b981', '#f59e0b', '#ffffff'];
export const ZOOM_MIN = 0.5;
export const ZOOM_MAX = 3;
export const ZOOM_STEP = 0.25;

export function createAnnotator(canvas: HTMLCanvasElement): Annotator {
    const ctx = canvas.getContext('2d');
    let img: HTMLImageElement | null = null;
    let tool: AnnotatorTool = 'rect';
    let color = ANNOTATOR_COLORS[0]!;
    let shapes: Shape[] = [];
    let history: Shape[][] = [[]];
    let historyIndex = 0;
    let current: Shape | null = null;
    let drawing = false;
    let zoom = 1;
    let changeCb: (() => void) | null = null;

    const notify = (): void => { if (changeCb) changeCb(); };

    // Stroke width / font scaled to the capture resolution so annotations stay
    // visible on the CSS-downscaled preview.
    const strokeWidth = (): number => Math.max(2, Math.round((canvas.width || 320) / 320));
    const fontSize = (): number => Math.max(14, Math.round((canvas.width || 600) / 45));

    // Arrowhead sits at the START point (where the pointer went down): you
    // press on the thing you're pointing at, then drag the tail away to where
    // there's room — better UX than the head landing at the release point.
    const drawArrow = (c: CanvasRenderingContext2D, s: Point, e: Point): void => {
        const head = Math.max(10, strokeWidth() * 4);
        // Angle of the head, pointing from the tail (e) back toward the tip (s).
        const angle = Math.atan2(s.y - e.y, s.x - e.x);
        c.beginPath();
        c.moveTo(s.x, s.y);
        c.lineTo(e.x, e.y);
        c.stroke();
        c.beginPath();
        c.moveTo(s.x, s.y);
        c.lineTo(s.x - head * Math.cos(angle - Math.PI / 6), s.y - head * Math.sin(angle - Math.PI / 6));
        c.moveTo(s.x, s.y);
        c.lineTo(s.x - head * Math.cos(angle + Math.PI / 6), s.y - head * Math.sin(angle + Math.PI / 6));
        c.stroke();
    };

    const drawShape = (c: CanvasRenderingContext2D, s: Shape): void => {
        c.strokeStyle = s.color;
        c.fillStyle = s.color;
        c.lineWidth = s.width;
        c.lineCap = 'round';
        c.lineJoin = 'round';
        if (s.tool === 'redact' && s.start && s.end) {
            // Opaque fill to hide sensitive content — solid black, independent
            // of the stroke colour, so it always censors.
            c.fillStyle = '#000000';
            c.fillRect(
                Math.min(s.start.x, s.end.x),
                Math.min(s.start.y, s.end.y),
                Math.abs(s.end.x - s.start.x),
                Math.abs(s.end.y - s.start.y),
            );
        } else if (s.tool === 'rect' && s.start && s.end) {
            c.strokeRect(s.start.x, s.start.y, s.end.x - s.start.x, s.end.y - s.start.y);
        } else if (s.tool === 'arrow' && s.start && s.end) {
            drawArrow(c, s.start, s.end);
        } else if (s.tool === 'pen' && s.points && s.points.length > 1) {
            c.beginPath();
            c.moveTo(s.points[0]!.x, s.points[0]!.y);
            for (let i = 1; i < s.points.length; i++) c.lineTo(s.points[i]!.x, s.points[i]!.y);
            c.stroke();
        } else if (s.tool === 'text' && s.start && s.text) {
            c.font = `${s.font ?? fontSize()}px sans-serif`;
            c.textBaseline = 'top';
            c.fillText(s.text, s.start.x, s.start.y);
        }
    };

    const redraw = (): void => {
        if (!ctx || !img) return;
        ctx.clearRect(0, 0, canvas.width, canvas.height);
        ctx.drawImage(img, 0, 0, canvas.width, canvas.height);
        for (const s of shapes) drawShape(ctx, s);
        if (current) drawShape(ctx, current);
    };

    const pushHistory = (next: Shape[]): void => {
        history = history.slice(0, historyIndex + 1);
        history.push(next);
        historyIndex = history.length - 1;
    };

    const pos = (e: PointerEvent): Point => {
        const rect = canvas.getBoundingClientRect();
        return {
            x: ((e.clientX - rect.left) * canvas.width) / rect.width,
            y: ((e.clientY - rect.top) * canvas.height) / rect.height,
        };
    };

    const onDown = (e: PointerEvent): void => {
        if (!img) return;
        e.preventDefault();
        const p = pos(e);
        if (tool === 'text') {
            const text = window.prompt('Annotation text');
            if (text) {
                shapes = [...shapes, { tool: 'text', color, width: strokeWidth(), start: p, text, font: fontSize() }];
                pushHistory(shapes);
                redraw();
                notify();
            }
            return;
        }
        drawing = true;
        canvas.setPointerCapture(e.pointerId);
        current = tool === 'pen'
            ? { tool, color, width: strokeWidth(), points: [p] }
            : { tool, color, width: strokeWidth(), start: p, end: p };
    };

    const onMove = (e: PointerEvent): void => {
        if (!drawing || !current) return;
        const p = pos(e);
        if (current.tool === 'pen') current.points!.push(p);
        else current.end = p;
        redraw();
    };

    const onUp = (): void => {
        if (!drawing || !current) return;
        shapes = [...shapes, current];
        pushHistory(shapes);
        current = null;
        drawing = false;
        redraw();
        notify();
    };

    canvas.addEventListener('pointerdown', onDown);
    canvas.addEventListener('pointermove', onMove);
    canvas.addEventListener('pointerup', onUp);
    canvas.addEventListener('pointercancel', onUp);

    return {
        loadDataUrl(dataUrl: string): Promise<void> {
            return new Promise((resolve, reject) => {
                const image = new Image();
                image.onload = () => {
                    img = image;
                    canvas.width = image.naturalWidth;
                    canvas.height = image.naturalHeight;
                    shapes = [];
                    history = [[]];
                    historyIndex = 0;
                    current = null;
                    redraw();
                    notify();
                    resolve();
                };
                image.onerror = () => reject(new Error('image load failed'));
                image.src = dataUrl;
            });
        },
        hasImage: () => img !== null,
        setTool(t) { tool = t; canvas.style.cursor = 'crosshair'; notify(); },
        setColor(c) { color = c; notify(); },
        undo() {
            if (historyIndex <= 0) return;
            historyIndex -= 1;
            shapes = [...(history[historyIndex] ?? [])];
            redraw();
            notify();
        },
        redo() {
            if (historyIndex >= history.length - 1) return;
            historyIndex += 1;
            shapes = [...(history[historyIndex] ?? [])];
            redraw();
            notify();
        },
        canUndo: () => historyIndex > 0,
        canRedo: () => historyIndex < history.length - 1,
        clearAnnotations() {
            shapes = [];
            pushHistory([]);
            redraw();
            notify();
        },
        setZoom(z) {
            zoom = Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, z));
            canvas.style.width = `${zoom * 100}%`;
            notify();
        },
        getZoom: () => zoom,
        reset() {
            img = null;
            shapes = [];
            history = [[]];
            historyIndex = 0;
            current = null;
            zoom = 1;
            canvas.style.width = '100%';
            if (ctx) ctx.clearRect(0, 0, canvas.width, canvas.height);
            notify();
        },
        toDataUrl() {
            if (!img) return null;
            // Make sure the latest state is rendered before exporting.
            redraw();
            return canvas.toDataURL('image/png');
        },
        onChange(cb) { changeCb = cb; },
    };
}
