// A bounded `fetch` for untrusted off-chain documents: a timeout, a size limit, and
// failures reported as values rather than exceptions.

export type FetchLike = (input: string, init?: RequestInit) => Promise<Response>;

export type BoundedFetchOptions = {
    fetch?: FetchLike;
    /** Largest response body accepted, in bytes. */
    maxBytes: number;
    timeoutMs: number;
    signal?: AbortSignal;
};

export type BoundedFetchResult =
    | { ok: true; status: number; body: Uint8Array }
    | { ok: false; reason: 'network' | 'timeout' | 'http-status' | 'too-large'; message: string; status?: number };

/** Fetches `url` and reads at most `maxBytes` of its body. Never throws. */
export async function boundedFetch(url: string, options: BoundedFetchOptions): Promise<BoundedFetchResult> {
    const fetchFn = options.fetch ?? globalThis.fetch;
    const controller = new AbortController();
    let timedOut = false;
    const timer = setTimeout(() => {
        timedOut = true;
        controller.abort();
    }, options.timeoutMs);
    const onAbort = () => controller.abort();
    // A signal that was already aborted never fires `abort` again, so check it first.
    if (options.signal?.aborted) {
        controller.abort();
    }
    options.signal?.addEventListener('abort', onAbort);
    try {
        const response = await fetchFn(url, { signal: controller.signal, redirect: 'follow' });
        // A redirect may lead off https (to plain http, or an internal address behind it).
        // The final URL is empty for some fetch stand-ins; only a known non-https one fails.
        if (response.url !== '' && !response.url.startsWith('https://')) {
            await response.body?.cancel().catch(() => {});
            return { ok: false, reason: 'network', message: `redirected off https to ${response.url}` };
        }
        if (!response.ok) {
            await response.body?.cancel().catch(() => {});
            return {
                ok: false,
                reason: 'http-status',
                status: response.status,
                message: `HTTP ${response.status}`,
            };
        }
        const declared = Number(response.headers.get('content-length') ?? NaN);
        if (Number.isFinite(declared) && declared > options.maxBytes) {
            await response.body?.cancel().catch(() => {});
            return tooLarge(options.maxBytes);
        }
        const body = await readLimited(response, options.maxBytes);
        if (body === null) {
            return tooLarge(options.maxBytes);
        }
        return { ok: true, status: response.status, body };
    } catch (error) {
        if (timedOut) {
            return { ok: false, reason: 'timeout', message: `no response within ${options.timeoutMs} ms` };
        }
        return { ok: false, reason: 'network', message: error instanceof Error ? error.message : String(error) };
    } finally {
        clearTimeout(timer);
        options.signal?.removeEventListener('abort', onAbort);
    }
}

function tooLarge(maxBytes: number): BoundedFetchResult {
    return { ok: false, reason: 'too-large', message: `response larger than ${maxBytes} bytes` };
}

/** The body, or `null` once it exceeds `maxBytes` (the rest is not downloaded). */
async function readLimited(response: Response, maxBytes: number): Promise<Uint8Array | null> {
    if (response.body === null) {
        const bytes = new Uint8Array(await response.arrayBuffer());
        return bytes.length > maxBytes ? null : bytes;
    }
    const reader = response.body.getReader();
    const chunks: Uint8Array[] = [];
    let total = 0;
    for (;;) {
        const { done, value } = await reader.read();
        if (done) {
            break;
        }
        total += value.length;
        if (total > maxBytes) {
            await reader.cancel().catch(() => {});
            return null;
        }
        chunks.push(value);
    }
    const body = new Uint8Array(total);
    let offset = 0;
    for (const chunk of chunks) {
        body.set(chunk, offset);
        offset += chunk.length;
    }
    return body;
}

const utf8 = new TextDecoder('utf-8', { fatal: true });

/** Parses a UTF-8 JSON body, or returns `undefined`. */
export function parseJsonBody(body: Uint8Array): unknown {
    try {
        return JSON.parse(utf8.decode(body));
    } catch {
        return undefined;
    }
}
