// Strategy metadata: validation, URI resolution and bounded fetching with a fake fetch.

import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

import {
    fetchStrategyMetadata,
    parseAccountSource,
    parseStrategyMetadata,
    resolveMetadataUri,
    STRATEGY_METADATA_SCHEMA,
    type FetchLike,
} from '../src/index.js';
import { key } from './helpers.js';
import { sampleMetadata } from './metadata-sample.js';

function errorsOf(doc: unknown): string[] {
    const result = parseStrategyMetadata(doc);
    return result.ok ? [] : result.errors;
}

describe('parseStrategyMetadata', () => {
    it('accepts the full example and keeps unknown keys', () => {
        const doc: Record<string, any> = { ...sampleMetadata(), x_future: { anything: 1 } };
        doc.actions[0].x_extra = true;
        const result = parseStrategyMetadata(doc);
        expect(result).toMatchObject({ ok: true, warnings: [] });
        if (result.ok) {
            expect(result.value.x_future).toEqual({ anything: 1 });
            expect(result.value.actions![0]!.x_extra).toBe(true);
        }
    });

    it('accepts the minimal document', () => {
        expect(parseStrategyMetadata({ schema: STRATEGY_METADATA_SCHEMA, name: 'x' }).ok).toBe(true);
    });

    it('warns, but accepts, unknown categories and action kinds', () => {
        const doc = sampleMetadata();
        doc.category = 'perpetuals';
        doc.actions[0].kind = 'stake';
        const result = parseStrategyMetadata(doc);
        expect(result.ok).toBe(true);
        expect(result.ok && result.warnings).toEqual([
            'category: unknown category "perpetuals"',
            'actions[0].kind: unknown kind "stake"',
        ]);
    });

    it('rejects a wrong schema, missing name and non-objects', () => {
        expect(errorsOf({ schema: 'pipeline-strategy-metadata/2', name: 'x' })).toEqual([
            'schema: must be "pipeline-strategy-metadata/1"',
        ]);
        expect(errorsOf({ schema: STRATEGY_METADATA_SCHEMA })).toEqual(['name: must be a non-empty string']);
        expect(errorsOf([])).toEqual(['$: must be an object']);
        expect(errorsOf(null)).toEqual(['$: must be an object']);
    });

    it('rejects unsafe or malformed URLs', () => {
        const doc = sampleMetadata();
        doc.image = 'javascript:alert(1)';
        doc.links.website = 'http://example.com';
        doc.links.extra = 'not a url';
        expect(errorsOf(doc)).toEqual([
            'image: URL scheme must be one of https:, ipfs:, ar:',
            'links.website: URL scheme must be one of https:, ipfs:, ar:',
            'links.extra: must be a URL',
        ]);
    });

    it('rejects bad args, sources and discriminators', () => {
        const doc = sampleMetadata();
        const action = doc.actions[0];
        action.discriminator = 'xyz';
        action.args.push({ name: 'amount_a', type: 'u64' }, { name: 'n', type: 'f32' });
        action.accounts.push(
            { name: 'a', source: 'mint:4' },
            { name: 'b', source: 'pda:strategy:key:pda:strategy:str:x' },
            { name: 'c', source: 'pda:strategy:arg:missing' },
            { name: 'd', source: 'treasury' },
            { name: 'e', source: 'fixed:notanaddress' },
            { name: 'f', source: 'pda:strategy:str:' + 'x'.repeat(33) },
        );
        expect(errorsOf(doc)).toEqual([
            'actions[0].discriminator: must be 16 hex digits',
            'actions[0].args[2].name: duplicate argument "amount_a"',
            'actions[0].args[3].type: must be one of u8, u16, u32, u64, i64, bool, pubkey, bytes, string',
            'actions[0].accounts[10].source: "mint:4": index must be 0 to 3',
            'actions[0].accounts[11].source: "pda:strategy:key:pda:strategy:str:x": pda and ata sources cannot be nested',
            'actions[0].accounts[12].source: seed refers to unknown argument "missing"',
            'actions[0].accounts[13].source: unknown account source "treasury"',
            'actions[0].accounts[14].source: "fixed:notanaddress": not an address',
            `actions[0].accounts[15].source: "pda:strategy:str:${'x'.repeat(33)}": str seed longer than 32 bytes`,
        ]);
    });

    it('rejects market-indexed sources in init', () => {
        const doc = sampleMetadata();
        doc.init.accounts.push({ name: 'vault', source: 'vault:0' });
        doc.init.market_state = 'market';
        expect(errorsOf(doc)).toEqual([
            'init.market_state: cannot refer to the market itself',
            'init.accounts[7].source: "vault:0" refers to the market\'s mints, which don\'t exist before the market is created',
        ]);
    });

    it('checks the optional trust fields', () => {
        const doc = sampleMetadata();
        doc.source = { commit: 'zz' };
        doc.conformance = { sha256: '12' };
        doc.security = { audits: [{ date: '2026-13-45' }] };
        expect(errorsOf(doc)).toEqual([
            'source.repository: must be a non-empty string',
            'source.commit: must be a commit hash (7 to 64 hex digits)',
            'conformance.report: must be a non-empty string',
            'conformance.sha256: must be 64 hex digits',
            'security.contact: must be a non-empty string',
            'security.audits[0].by: must be a non-empty string',
            'security.audits[0].date: must be a date as YYYY-MM-DD',
        ]);
    });
});

describe('STRATEGY-GUIDE.md section 7', () => {
    it('shows exactly the document these tests validate and build', () => {
        const guide = readFileSync(new URL('../../../docs/STRATEGY-GUIDE.md', import.meta.url), 'utf8');
        const section = guide.slice(guide.indexOf('## 7.'), guide.indexOf('## 8.'));
        const start = section.indexOf('```json') + '```json'.length;
        const example = JSON.parse(section.slice(start, section.indexOf('```', start)));
        expect(example).toEqual(sampleMetadata());
        expect(parseStrategyMetadata(example)).toMatchObject({ ok: true, warnings: [] });
    });
});

describe('parseAccountSource', () => {
    it('parses every source kind', () => {
        expect(parseAccountSource('wallet')).toEqual({ kind: 'wallet' });
        expect(parseAccountSource('wallet_token_account:3')).toEqual({ kind: 'wallet_token_account', index: 3 });
        expect(parseAccountSource(`fixed:${key(1)}`)).toEqual({ kind: 'fixed', address: key(1) });
        expect(parseAccountSource('ata:wallet,mint:1')).toEqual({
            kind: 'ata',
            owner: { kind: 'wallet' },
            mint: { kind: 'mint', index: 1 },
        });
        expect(parseAccountSource('token_program_of:input:m')).toEqual({
            kind: 'token_program_of',
            mint: { kind: 'input', name: 'm' },
        });
        expect(parseAccountSource(`pda:${key(2)}:str:a,hex:0aff,key:market,arg:n`)).toEqual({
            kind: 'pda',
            program: key(2),
            seeds: [
                { kind: 'str', text: 'a' },
                { kind: 'hex', bytes: new Uint8Array([0x0a, 0xff]) },
                { kind: 'key', source: { kind: 'market' } },
                { kind: 'arg', name: 'n' },
            ],
        });
        expect(parseAccountSource('pda:strategy')).toEqual({ kind: 'pda', program: 'strategy', seeds: [] });
        expect(typeof parseAccountSource('ata:wallet')).toBe('string');
        expect(typeof parseAccountSource('pda:strategy:salt:1')).toBe('string');
    });

    it('agrees with the JSON Schema pattern on every sample source', () => {
        const schema = JSON.parse(
            readFileSync(new URL('../schema/strategy-metadata.v1.json', import.meta.url), 'utf8'),
        );
        const pattern = new RegExp(schema.$defs.source.pattern);
        const doc = sampleMetadata();
        const sources = [
            doc.init.market_state,
            ...doc.init.accounts.map((a: any) => a.source),
            ...doc.actions[0].accounts.map((a: any) => a.source),
        ];
        for (const source of sources) {
            expect(pattern.test(source), source).toBe(true);
        }
        expect(pattern.test('treasury')).toBe(false);
    });
});

describe('resolveMetadataUri', () => {
    it('maps https, ipfs and ar URIs, and refuses the rest', () => {
        expect(resolveMetadataUri('https://a.example/m.json')).toBe('https://a.example/m.json');
        expect(resolveMetadataUri('ipfs://bafy/m.json')).toBe('https://ipfs.io/ipfs/bafy/m.json');
        expect(resolveMetadataUri('ipfs://ipfs/bafy')).toBe('https://ipfs.io/ipfs/bafy');
        expect(resolveMetadataUri('ar://tx1')).toBe('https://arweave.net/tx1');
        expect(
            resolveMetadataUri('ipfs://bafy', { ipfs: 'https://gw.example/ipfs/', arweave: 'https://ar.example/' }),
        ).toBe('https://gw.example/ipfs/bafy');
        expect(resolveMetadataUri('ar://tx1', { arweave: 'https://ar.example/' })).toBe('https://ar.example/tx1');
        expect(resolveMetadataUri('http://a.example/m.json')).toBeNull();
        expect(resolveMetadataUri('data:application/json,{}')).toBeNull();
        expect(resolveMetadataUri('ipfs://')).toBeNull();
    });
});

function fakeFetch(respond: (url: string, init?: RequestInit) => Response | Promise<Response>) {
    const urls: string[] = [];
    const fetch: FetchLike = async (url, init) => {
        urls.push(url);
        return respond(url, init);
    };
    return { fetch, urls };
}

describe('fetchStrategyMetadata', () => {
    it('fetches through the gateway and validates', async () => {
        const { fetch, urls } = fakeFetch(() => Response.json(sampleMetadata()));
        const result = await fetchStrategyMetadata('ipfs://bafy/m.json', { fetch });
        expect(urls).toEqual(['https://ipfs.io/ipfs/bafy/m.json']);
        expect(result.ok).toBe(true);
        expect(result.ok && result.value.name).toBe('Constant product');
    });

    it('reports missing and unsupported URIs without fetching', async () => {
        const { fetch, urls } = fakeFetch(() => Response.json({}));
        expect(await fetchStrategyMetadata('', { fetch })).toMatchObject({ ok: false, reason: 'missing' });
        expect(await fetchStrategyMetadata('http://x.example/m', { fetch })).toMatchObject({
            ok: false,
            reason: 'unsupported-uri',
        });
        expect(urls).toEqual([]);
    });

    it('reports HTTP errors, bad JSON and invalid documents', async () => {
        const notFound = fakeFetch(() => new Response('nope', { status: 404 }));
        expect(await fetchStrategyMetadata('https://x.example/m', { fetch: notFound.fetch })).toMatchObject({
            ok: false,
            reason: 'http-status',
            errors: ['HTTP 404'],
        });
        const html = fakeFetch(() => new Response('<html>'));
        expect(await fetchStrategyMetadata('https://x.example/m', { fetch: html.fetch })).toMatchObject({
            ok: false,
            reason: 'not-json',
        });
        const invalid = fakeFetch(() => Response.json({ schema: 'other' }));
        const result = await fetchStrategyMetadata('https://x.example/m', { fetch: invalid.fetch });
        expect(result).toMatchObject({ ok: false, reason: 'invalid', url: 'https://x.example/m' });
    });

    it('enforces the size limit from the header and while streaming', async () => {
        const big = JSON.stringify({ ...sampleMetadata(), padding: 'x'.repeat(2000) });
        const declared = fakeFetch(() => new Response(big, { headers: { 'content-length': String(big.length) } }));
        expect(
            await fetchStrategyMetadata('https://x.example/m', { fetch: declared.fetch, maxBytes: 1000 }),
        ).toMatchObject({ ok: false, reason: 'too-large' });

        const streamed = fakeFetch(() => {
            const chunk = new TextEncoder().encode(big);
            const body = new ReadableStream<Uint8Array>({
                start(controller) {
                    controller.enqueue(chunk.subarray(0, 600));
                    controller.enqueue(chunk.subarray(600));
                    controller.close();
                },
            });
            return new Response(body);
        });
        expect(
            await fetchStrategyMetadata('https://x.example/m', { fetch: streamed.fetch, maxBytes: 1000 }),
        ).toMatchObject({ ok: false, reason: 'too-large' });
    });

    it('times out and reports network errors instead of throwing', async () => {
        const hang = fakeFetch(
            (_url, init) =>
                new Promise<Response>((_resolve, reject) => {
                    init?.signal?.addEventListener('abort', () => reject(new Error('aborted')));
                }),
        );
        expect(await fetchStrategyMetadata('https://x.example/m', { fetch: hang.fetch, timeoutMs: 10 })).toMatchObject({
            ok: false,
            reason: 'timeout',
        });
        const down = fakeFetch(() => {
            throw new TypeError('fetch failed');
        });
        expect(await fetchStrategyMetadata('https://x.example/m', { fetch: down.fetch })).toEqual({
            ok: false,
            url: 'https://x.example/m',
            reason: 'network',
            errors: ['fetch failed'],
        });
    });
});

describe('fetch audit fixes', () => {
    it('refuses a redirect that leaves https', async () => {
        const { fetch } = fakeFetch(() => {
            const response = Response.json(sampleMetadata());
            Object.defineProperty(response, 'url', { value: 'http://169.254.169.254/latest' });
            return response;
        });
        expect(await fetchStrategyMetadata('https://x.example/m', { fetch })).toMatchObject({
            ok: false,
            reason: 'network',
        });
    });

    it('does not fetch when the signal is already aborted', async () => {
        const { fetch } = fakeFetch((_url, init) =>
            init?.signal?.aborted ? Promise.reject(new Error('aborted')) : Response.json(sampleMetadata()),
        );
        const controller = new AbortController();
        controller.abort();
        expect(await fetchStrategyMetadata('https://x.example/m', { fetch, signal: controller.signal })).toMatchObject({
            ok: false,
        });
    });
});

describe('PDA seed limit', () => {
    it('allows 15 seeds, leaving room for the bump', () => {
        const seeds = (n: number) => Array.from({ length: n }, (_, i) => `str:s${i}`).join(',');
        expect(typeof parseAccountSource(`pda:strategy:${seeds(15)}`)).toBe('object');
        expect(parseAccountSource(`pda:strategy:${seeds(16)}`)).toMatch(/at most 15 seeds/);
    });
});

describe('social links', () => {
    it('accepts the named social links and warns when one is on an unexpected site', () => {
        const ok = parseStrategyMetadata(sampleMetadata());
        expect(ok.ok && ok.value.links?.twitter).toBe('https://x.com/example');
        expect(ok.ok && ok.warnings).toEqual([]);

        const elsewhere = sampleMetadata();
        elsewhere.links = {
            twitter: 'https://x-com.example/phish',
            github: 'https://docs.github.com/en',
            discord: 'https://discord.com/invite/abc',
            farcaster: 'https://warpcast.com/example',
        };
        const result = parseStrategyMetadata(elsewhere);
        expect(result.ok).toBe(true);
        // A subdomain of the expected site is fine; another site gets a warning.
        expect(result.ok && result.warnings).toEqual(['links.twitter: expected a link on x.com or twitter.com']);

        const bad = sampleMetadata();
        bad.links = { telegram: 'http://t.me/example' };
        expect(parseStrategyMetadata(bad).ok).toBe(false);
    });
});
