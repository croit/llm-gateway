// Browser diagnostics capture for the feedback widget.
//
// Faithful port of yachtlistings2's `capture-utils.ts`: monkey-patch
// `console.*`, `fetch`, and `XMLHttpRequest` once at startup and keep bounded
// ring buffers (100 each) of recent console + network activity, plus a 5s
// resource-timing sweep. The feedback dialog folds these into `system_info` so
// a report carries the errors + requests that led up to it. Limits are hard
// (never unbounded): the buffers cap at 100 and network is further sliced to
// the last 50 at submit time.

export interface ConsoleLog {
    timestamp: string;
    level: 'log' | 'info' | 'warn' | 'error' | 'debug';
    args: string[];
}

export interface NetworkLog {
    timestamp: string;
    method: string;
    url: string;
    status?: number | undefined;
    duration?: number | undefined;
    type?: string | undefined;
    size?: number | undefined;
    query?: string | undefined;
    requestBody?: string | undefined;
}

const MAX_CONSOLE_LOGS = 100;
const MAX_NETWORK_LOGS = 100;

const SENSITIVE_KEYS = new Set([
    'password', 'passwd', 'secret', 'token', 'access_token', 'refresh_token',
    'api_key', 'apikey', 'authorization', 'auth', 'credential', 'private_key',
    'privatekey', 'client_secret', 'clientsecret',
]);

const redactSensitive = (params: Record<string, string>): Record<string, string> => {
    const result: Record<string, string> = {};
    for (const [key, value] of Object.entries(params)) {
        result[key] = SENSITIVE_KEYS.has(key.toLowerCase()) ? '[REDACTED]' : value;
    }
    return result;
};

const extractQuery = (url: string): string | undefined => {
    try {
        const parsed = new URL(url, window.location.origin);
        if (parsed.search.length <= 1) return undefined;
        const params = redactSensitive(Object.fromEntries(parsed.searchParams));
        return new URLSearchParams(params).toString();
    } catch {
        return undefined;
    }
};

const sanitizeBody = (body: BodyInit | null | undefined): string | undefined => {
    if (!body) return undefined;
    if (body instanceof FormData) return '[FormData]';
    if (body instanceof Blob) return `[Blob ${body.size}B]`;
    if (body instanceof ArrayBuffer) return `[ArrayBuffer ${body.byteLength}B]`;
    if (ArrayBuffer.isView(body)) return `[TypedArray ${body.byteLength}B]`;
    const text = typeof body === 'string' ? body : String(body);
    try {
        const parsed = JSON.parse(text);
        if (typeof parsed === 'object' && parsed !== null) {
            const redacted: Record<string, unknown> = {};
            for (const [key, value] of Object.entries(parsed)) {
                redacted[key] = SENSITIVE_KEYS.has(key.toLowerCase()) ? '[REDACTED]' : value;
            }
            const result = JSON.stringify(redacted);
            return result.length > 500 ? `${result.substring(0, 497)}...` : result;
        }
    } catch {
        /* not JSON */
    }
    return text.length > 500 ? `${text.substring(0, 497)}...` : text;
};

const consoleLogs: ConsoleLog[] = [];
const networkLogs: NetworkLog[] = [];

interface CapturedConsoleFn { (...args: unknown[]): void; __captured?: boolean }
interface ExtWindow extends Window { __feedbackNetworkCapture?: boolean }
interface CapturedXHR extends XMLHttpRequest {
    __method?: string | undefined;
    __url?: string | undefined;
    __startTime?: number | undefined;
    __query?: string | undefined;
    __requestBody?: string | undefined;
}

const initConsoleCapture = (): void => {
    if ((console.log as CapturedConsoleFn).__captured) return;
    const wrap = (level: ConsoleLog['level'], original: (...a: unknown[]) => void): CapturedConsoleFn =>
        (...args: unknown[]) => {
            original.apply(console, args);
            consoleLogs.push({
                timestamp: new Date().toISOString(),
                level,
                args: args.map((arg) => {
                    try {
                        if (typeof arg === 'string') return arg;
                        if (arg instanceof Error) return `${arg.name}: ${arg.message}\n${arg.stack ?? ''}`;
                        return JSON.stringify(arg);
                    } catch {
                        return String(arg);
                    }
                }),
            });
            if (consoleLogs.length > MAX_CONSOLE_LOGS) consoleLogs.shift();
        };
    console.log = wrap('log', console.log) as typeof console.log;
    console.info = wrap('info', console.info) as typeof console.info;
    console.warn = wrap('warn', console.warn) as typeof console.warn;
    console.error = wrap('error', console.error) as typeof console.error;
    console.debug = wrap('debug', console.debug) as typeof console.debug;
    (console.log as CapturedConsoleFn).__captured = true;
};

const pushNetwork = (entry: NetworkLog): void => {
    networkLogs.push(entry);
    if (networkLogs.length > MAX_NETWORK_LOGS) networkLogs.shift();
};

const initNetworkCapture = (): void => {
    if ((window as ExtWindow).__feedbackNetworkCapture) return;

    if (typeof window.fetch === 'function') {
        const originalFetch = window.fetch;
        const intercepted = async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
            const start = performance.now();
            const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;
            const method = init?.method || (input instanceof Request ? input.method : 'GET');
            const query = extractQuery(url);
            const requestBody = sanitizeBody(init?.body ?? null);
            try {
                const resp = await originalFetch(input, init);
                pushNetwork({ timestamp: new Date().toISOString(), method, url, status: resp.status, duration: performance.now() - start, type: 'fetch', query, requestBody });
                return resp;
            } catch (err) {
                pushNetwork({ timestamp: new Date().toISOString(), method, url, status: 0, duration: performance.now() - start, type: 'fetch', query, requestBody });
                throw err;
            }
        };
        Object.setPrototypeOf(intercepted, originalFetch);
        window.fetch = intercepted as typeof fetch;
    }

    const XHR = window.XMLHttpRequest;
    if (XHR) {
        const open = XHR.prototype.open;
        const send = XHR.prototype.send;
        XHR.prototype.open = function (this: CapturedXHR, method: string, url: string | URL, ...rest: unknown[]) {
            const urlStr = typeof url === 'string' ? url : url.href;
            this.__method = method;
            this.__url = urlStr;
            this.__startTime = performance.now();
            this.__query = extractQuery(urlStr);
            return (open as (...a: unknown[]) => void).call(this, method, url, ...rest);
        };
        XHR.prototype.send = function (this: CapturedXHR, body?: Document | XMLHttpRequestBodyInit | null) {
            this.__requestBody = body instanceof Document ? '[Document]' : sanitizeBody(body ?? null);
            this.addEventListener('loadend', () => {
                pushNetwork({
                    timestamp: new Date().toISOString(),
                    method: this.__method ?? 'GET',
                    url: this.__url ?? '',
                    status: this.status,
                    duration: performance.now() - (this.__startTime ?? 0),
                    type: 'xhr',
                    query: this.__query,
                    requestBody: this.__requestBody,
                });
            });
            return (send as (...a: unknown[]) => void).call(this, body);
        };
    }

    // Resource-timing sweep for sub-resources (scripts, css, images) that
    // aren't fetch/XHR. Bounded scan + dedup by url.
    const sweep = (): void => {
        try {
            const entries = performance.getEntriesByType('resource') as PerformanceResourceTiming[];
            for (const e of entries.slice(-MAX_NETWORK_LOGS)) {
                if (networkLogs.some((l) => l.url === e.name && l.type === e.initiatorType)) continue;
                pushNetwork({
                    timestamp: new Date(e.startTime + performance.timeOrigin).toISOString(),
                    method: 'GET',
                    url: e.name,
                    duration: e.duration,
                    type: e.initiatorType,
                    size: e.transferSize,
                    query: extractQuery(e.name),
                });
            }
        } catch {
            /* ignore */
        }
    };
    window.setInterval(sweep, 5000);

    (window as ExtWindow).__feedbackNetworkCapture = true;
};

let started = false;
export const initFeedbackCapture = (): void => {
    if (started || typeof window === 'undefined') return;
    started = true;
    initConsoleCapture();
    initNetworkCapture();
};

export const getConsoleLogs = (): ConsoleLog[] => [...consoleLogs];
export const getNetworkLogs = (): NetworkLog[] => [...networkLogs];
