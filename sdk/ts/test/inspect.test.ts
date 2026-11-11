// Trust facts against a fake RPC and fake fetch, with account bytes built by hand:
// loader-v3/v4/v2 programs, SPL Token and Token-2022 mints with many extensions, and
// router Strategy and Market accounts.

import { createHash } from 'node:crypto';

import { getAddressDecoder, getAddressEncoder, getBase64Decoder, type Address } from '@solana/kit';
import { describe, expect, it } from 'vitest';

import {
    BPF_LOADER_UPGRADEABLE_PROGRAM_ADDRESS,
    decodeMint,
    encodeAccount,
    EXTENSION_TYPES,
    fetchVerifiedBuild,
    filterMarkets,
    FLAG_DESCRIPTIONS,
    FLAGS,
    getMarketCodec,
    getStrategyCodec,
    inspectMarket,
    inspectMint,
    inspectProgram,
    inspectStrategy,
    LOADER_ADDRESSES,
    marketPda,
    PIPELINE_PROGRAM_ADDRESS,
    programDataPda,
    strategyPda,
    TOKEN_2022_PROGRAM_ADDRESS,
    TOKEN_PROGRAM_ADDRESS,
    type FetchLike,
    type InspectRpc,
} from '../src/index.js';
import { key } from './helpers.js';
import { sampleMetadata } from './metadata-sample.js';

// ---------------------------------------------------------------------------------------
// Fakes and byte builders
// ---------------------------------------------------------------------------------------

type FakeAccount = { owner: Address; executable?: boolean; data: Uint8Array };

function fakeRpc(accounts: Record<string, FakeAccount>, epoch = 10n) {
    const calls: { address: string; config: any }[] = [];
    const base64 = getBase64Decoder();
    const rpc = {
        getAccountInfo(address: string, config: any) {
            calls.push({ address, config });
            const account = accounts[address];
            let data = account?.data ?? new Uint8Array();
            if (config?.dataSlice) {
                data = data.subarray(config.dataSlice.offset, config.dataSlice.offset + config.dataSlice.length);
            }
            return {
                send: async () => ({
                    context: { slot: 1n },
                    value: account
                        ? {
                              owner: account.owner,
                              executable: account.executable ?? false,
                              lamports: 1n,
                              space: BigInt(account.data.length),
                              data: [base64.decode(data), 'base64'],
                          }
                        : null,
                }),
            };
        },
        getEpochInfo() {
            calls.push({ address: 'getEpochInfo', config: null });
            return { send: async () => ({ epoch }) };
        },
    };
    return { rpc: rpc as unknown as InspectRpc, calls };
}

/** The address whose 32 bytes all equal `n`, beyond the fixture keys. */
const bigKey = (n: number) => getAddressDecoder().decode(new Uint8Array(32).fill(n));

const addr = (address: Address) => getAddressEncoder().encode(address) as Uint8Array;

function concat(...parts: (Uint8Array | number[])[]): Uint8Array {
    const arrays = parts.map((p) => (p instanceof Uint8Array ? p : Uint8Array.from(p)));
    const out = new Uint8Array(arrays.reduce((n, a) => n + a.length, 0));
    let offset = 0;
    for (const a of arrays) {
        out.set(a, offset);
        offset += a.length;
    }
    return out;
}

function u16(n: number) {
    return [n & 0xff, n >> 8];
}
function u32(n: number) {
    const b = new Uint8Array(4);
    new DataView(b.buffer).setUint32(0, n, true);
    return b;
}
function u64(n: bigint) {
    const b = new Uint8Array(8);
    new DataView(b.buffer).setBigUint64(0, n, true);
    return b;
}
function i64(n: bigint) {
    const b = new Uint8Array(8);
    new DataView(b.buffer).setBigInt64(0, n, true);
    return b;
}
function f64(n: number) {
    const b = new Uint8Array(8);
    new DataView(b.buffer).setFloat64(0, n, true);
    return b;
}
function str(s: string) {
    const b = new TextEncoder().encode(s);
    return concat(u32(b.length), b);
}
const none = new Uint8Array(32);

const ELF = concat(new TextEncoder().encode('\x7fELF fake program'), [0, 0, 1, 2, 3]);

/** A loader-v3 program and its ProgramData, padded with zeros as a real deploy is. */
async function upgradeableProgram(programId: Address, authority: Address | null, slot = 1234n, elf = ELF) {
    const programData = await programDataPda(programId);
    const accounts: Record<string, FakeAccount> = {
        [programId]: {
            owner: BPF_LOADER_UPGRADEABLE_PROGRAM_ADDRESS,
            executable: true,
            data: concat(u32(2), addr(programData)),
        },
        [programData]: {
            owner: BPF_LOADER_UPGRADEABLE_PROGRAM_ADDRESS,
            data: concat(
                u32(3),
                u64(slot),
                authority ? concat([1], addr(authority)) : concat([0], none),
                elf,
                new Uint8Array(500),
            ),
        },
    };
    return { accounts, programData };
}

function expectedHash(elf = ELF) {
    return createHash('sha256').update(elf).digest('hex');
}

function mintBase(mintAuthority: Address | null, supply: bigint, decimals: number, freezeAuthority: Address | null) {
    const option = (a: Address | null) => (a ? concat(u32(1), addr(a)) : concat(u32(0), none));
    return concat(option(mintAuthority), u64(supply), [decimals, 1], option(freezeAuthority));
}

function tlv(type: number, value: Uint8Array | number[]) {
    const v = value instanceof Uint8Array ? value : Uint8Array.from(value);
    return concat(u16(type), u16(v.length), v);
}

function token2022Mint(base: Uint8Array, extensions: Uint8Array[]) {
    return concat(base, new Uint8Array(165 - base.length), [1], ...extensions);
}

const MINT_A = key(13);
const MINT_B = key(14);
const HOOK = bigKey(50);
const DELEGATE = bigKey(51);

/** A Token-2022 mint with many extensions, one unknown, and trailing padding. */
function richMint() {
    const transferFee = concat(
        addr(bigKey(52)),
        none,
        u64(77n),
        concat(u64(0n), u64(1_000n), u16(50)),
        concat(u64(20n), u64(2_000n), u16(100)),
    );
    const metadata = concat(
        none,
        addr(MINT_B),
        str('Hooked'),
        str('HOOK'),
        str('https://x.example/t.json'),
        u32(1),
        str('k'),
        str('v'),
    );
    const interest = concat(addr(bigKey(53)), i64(100n), u16(5), i64(200n), u16(7));
    return token2022Mint(mintBase(bigKey(54), 1_000_000n, 6, bigKey(55)), [
        tlv(EXTENSION_TYPES.TransferFeeConfig, transferFee),
        tlv(EXTENSION_TYPES.TransferHook, concat(addr(bigKey(56)), addr(HOOK))),
        tlv(EXTENSION_TYPES.PermanentDelegate, addr(DELEGATE)),
        tlv(EXTENSION_TYPES.NonTransferable, []),
        tlv(EXTENSION_TYPES.DefaultAccountState, [2]),
        tlv(EXTENSION_TYPES.MintCloseAuthority, addr(bigKey(57))),
        tlv(EXTENSION_TYPES.InterestBearingConfig, interest),
        tlv(EXTENSION_TYPES.ScaledUiAmount, concat(none, f64(1.5), i64(300n), f64(2))),
        tlv(EXTENSION_TYPES.Pausable, concat(addr(bigKey(58)), [1])),
        tlv(EXTENSION_TYPES.ConfidentialTransferMint, concat(none, [1], none)),
        tlv(EXTENSION_TYPES.MetadataPointer, concat(none, addr(MINT_B))),
        tlv(EXTENSION_TYPES.TokenMetadata, metadata),
        tlv(EXTENSION_TYPES.GroupMemberPointer, concat(none, addr(MINT_B))),
        tlv(EXTENSION_TYPES.TokenGroupMember, concat(addr(MINT_B), addr(bigKey(59)), u64(4n))),
        tlv(EXTENSION_TYPES.PermissionedBurn, addr(bigKey(60))),
        tlv(99, [1, 2, 3]),
        new Uint8Array(12),
    ]);
}

/** A minimal `pipeline-check` JSON report for `programId` and code hash `hash`. */
function checkReport(programId: string, hash: string): string {
    return JSON.stringify({
        schema: 'pipeline-check-report/1',
        generated_at: 1_794_000_000,
        strategy: { program_id: programId, executable_sha256: hash },
        summary: { result: 'pass', pass: 13, warn: 0, fail: 0, skip: 1 },
        checks: [],
    });
}

function sha256(text: string): string {
    return createHash('sha256').update(text).digest('hex');
}

function fetchFor(routes: Record<string, () => Response>) {
    const urls: string[] = [];
    const fetch: FetchLike = async (url) => {
        urls.push(url);
        const route = routes[url];
        if (!route) {
            throw new TypeError(`fetch failed: ${url}`);
        }
        return route();
    };
    return { fetch, urls };
}

// Shapes taken from https://verify.osec.io/status/<program> (November 2026).
function osecVerified(hash: string) {
    return {
        is_verified: true,
        message: 'On chain program verified',
        on_chain_hash: hash,
        executable_hash: hash,
        repo_url: 'https://github.com/example/cp',
        commit: 'None',
        last_verified_at: '2024-12-12T13:23:28.496764',
        is_frozen: false,
        is_closed: false,
    };
}
const OSEC_UNVERIFIED = {
    is_verified: false,
    message: 'On chain program not verified',
    on_chain_hash: '',
    executable_hash: '',
    repo_url: '',
    commit: '',
    last_verified_at: null,
    is_frozen: false,
    is_closed: false,
};

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

describe('flags', () => {
    it('documents every flag', () => {
        expect(FLAGS.length).toBe(Object.keys(FLAG_DESCRIPTIONS).length);
        for (const flag of FLAGS) {
            expect(FLAG_DESCRIPTIONS[flag].length).toBeGreaterThan(10);
        }
    });

    it('filterMarkets requires and excludes', () => {
        const items = [
            { id: 1, flags: ['upgradeable', 'mint-authority'] as const },
            { id: 2, flags: ['frozen', 'verified-build'] as const },
            { id: 3, flags: ['frozen', 'transfer-hook'] as const },
        ];
        expect(filterMarkets(items, { require: ['frozen'] }).map((i) => i.id)).toEqual([2, 3]);
        expect(filterMarkets(items, { require: ['frozen'], exclude: ['transfer-hook'] }).map((i) => i.id)).toEqual([2]);
        expect(filterMarkets(items, {}).map((i) => i.id)).toEqual([1, 2, 3]);
    });
});

describe('inspectProgram', () => {
    it('reads an upgradeable loader-v3 program and hashes it like solana-verify', async () => {
        const programId = key(7);
        const { accounts, programData } = await upgradeableProgram(programId, key(9), 4321n);
        const { rpc } = fakeRpc(accounts);
        const result = await inspectProgram(rpc, programId);
        expect(result).toEqual({
            programId,
            loader: 'upgradeable',
            loaderAddress: BPF_LOADER_UPGRADEABLE_PROGRAM_ADDRESS,
            deployed: true,
            upgradeable: true,
            upgradeAuthority: key(9),
            programDataAddress: programData,
            lastDeploySlot: 4321n,
            loaderV4Status: null,
            executableSha256: expectedHash(),
            flags: ['upgradeable'],
        });
    });

    it('reports a frozen program and only reads the header when not hashing', async () => {
        const programId = key(7);
        const { accounts, programData } = await upgradeableProgram(programId, null);
        const { rpc, calls } = fakeRpc(accounts);
        const result = await inspectProgram(rpc, programId, { hashExecutable: false });
        expect(result).toMatchObject({
            upgradeable: false,
            upgradeAuthority: null,
            executableSha256: null,
            flags: ['frozen'],
        });
        expect(calls.find((c) => c.address === programData)!.config.dataSlice).toEqual({ offset: 0, length: 45 });
    });

    it('reports a missing or closed program', async () => {
        expect((await inspectProgram(fakeRpc({}).rpc, key(7))).flags).toEqual(['program-missing']);
        const { accounts, programData } = await upgradeableProgram(key(7), key(9));
        delete accounts[programData];
        const closed = await inspectProgram(fakeRpc(accounts).rpc, key(7));
        expect(closed).toMatchObject({ loader: 'upgradeable', deployed: false, flags: ['program-missing'] });
    });

    it('reads loader-v4 and loader-v2 programs', async () => {
        const v4 = (status: bigint) => ({
            [key(7)]: {
                owner: LOADER_ADDRESSES.v4,
                executable: true,
                data: concat(u64(99n), addr(key(9)), u64(status), ELF, new Uint8Array(10)),
            },
        });
        expect(await inspectProgram(fakeRpc(v4(1n)).rpc, key(7))).toMatchObject({
            loader: 'v4',
            loaderV4Status: 'deployed',
            upgradeable: true,
            upgradeAuthority: key(9),
            lastDeploySlot: 99n,
            executableSha256: expectedHash(),
            flags: ['upgradeable'],
        });
        expect(await inspectProgram(fakeRpc(v4(2n)).rpc, key(7))).toMatchObject({
            loaderV4Status: 'finalized',
            upgradeable: false,
            upgradeAuthority: null,
            flags: ['frozen'],
        });
        const v2 = { [key(7)]: { owner: LOADER_ADDRESSES.v2, executable: true, data: concat(ELF, new Uint8Array(7)) } };
        expect(await inspectProgram(fakeRpc(v2).rpc, key(7))).toMatchObject({
            loader: 'v2',
            executableSha256: expectedHash(),
            flags: ['frozen'],
        });
    });
});

describe('fetchVerifiedBuild', () => {
    const url = `https://verify.osec.io/status/${key(7)}`;

    it('normalises a verified response', async () => {
        const { fetch, urls } = fetchFor({ [url]: () => Response.json(osecVerified('ab'.repeat(32))) });
        expect(await fetchVerifiedBuild(key(7), { fetch })).toEqual({
            status: 'verified',
            verified: true,
            repository: 'https://github.com/example/cp',
            commit: null,
            onchainHash: 'ab'.repeat(32),
            executableHash: 'ab'.repeat(32),
            lastVerifiedAt: '2024-12-12T13:23:28.496764',
            message: 'On chain program verified',
        });
        expect(urls).toEqual([url]);
    });

    it('reports unverified programs and a custom API URL', async () => {
        const { fetch } = fetchFor({ [`https://api.example/status/${key(7)}`]: () => Response.json(OSEC_UNVERIFIED) });
        expect(await fetchVerifiedBuild(key(7), { fetch, apiUrl: 'https://api.example/' })).toMatchObject({
            status: 'unverified',
            verified: false,
            repository: null,
            onchainHash: null,
        });
    });

    it('never throws: network errors, HTTP errors and odd bodies are unknown', async () => {
        expect(await fetchVerifiedBuild(key(7), { fetch: fetchFor({}).fetch })).toMatchObject({
            status: 'unknown',
            verified: false,
        });
        const bad = fetchFor({ [url]: () => new Response('Invalid program id', { status: 400 }) });
        expect(await fetchVerifiedBuild(key(7), { fetch: bad.fetch })).toMatchObject({
            status: 'unknown',
            error: 'HTTP 400',
        });
        const odd = fetchFor({ [url]: () => Response.json({ hello: 1 }) });
        expect(await fetchVerifiedBuild(key(7), { fetch: odd.fetch })).toMatchObject({
            status: 'unknown',
            error: 'unexpected response',
        });
    });
});

describe('inspectMint', () => {
    it('reads a plain SPL Token mint', async () => {
        const { rpc, calls } = fakeRpc({
            [MINT_A]: { owner: TOKEN_PROGRAM_ADDRESS, data: mintBase(null, 5n, 9, null) },
        });
        expect(await inspectMint(rpc, MINT_A)).toEqual({
            mint: MINT_A,
            tokenProgram: TOKEN_PROGRAM_ADDRESS,
            decimals: 9,
            supply: 5n,
            mintAuthority: null,
            freezeAuthority: null,
            extensions: {},
            extensionTypes: [],
            unknownExtensions: [],
            flags: [],
        });
        expect(calls.some((c) => c.address === 'getEpochInfo')).toBe(false);
    });

    it('decodes Token-2022 extensions and lists unknown ones by number', async () => {
        const { rpc } = fakeRpc({ [MINT_B]: { owner: TOKEN_2022_PROGRAM_ADDRESS, data: richMint() } }, 25n);
        const mint = (await inspectMint(rpc, MINT_B))!;
        expect(mint).toMatchObject({
            tokenProgram: TOKEN_2022_PROGRAM_ADDRESS,
            decimals: 6,
            supply: 1_000_000n,
            mintAuthority: bigKey(54),
            freezeAuthority: bigKey(55),
            unknownExtensions: [99],
        });
        expect(mint.extensionTypes).toEqual([1, 14, 12, 9, 6, 3, 10, 25, 26, 4, 18, 19, 22, 23, 28, 99]);
        const e = mint.extensions;
        expect(e.transferFee).toEqual({
            authority: bigKey(52),
            withdrawWithheldAuthority: null,
            withheldAmount: 77n,
            older: { epoch: 0n, maximumFee: 1_000n, basisPoints: 50 },
            newer: { epoch: 20n, maximumFee: 2_000n, basisPoints: 100 },
            current: { epoch: 20n, maximumFee: 2_000n, basisPoints: 100 },
        });
        expect(e.transferHook).toEqual({ authority: bigKey(56), programId: HOOK });
        expect(e.permanentDelegate).toEqual({ delegate: DELEGATE });
        expect(e.nonTransferable).toBe(true);
        expect(e.defaultAccountState).toEqual({ state: 'frozen' });
        expect(e.mintCloseAuthority).toEqual({ closeAuthority: bigKey(57) });
        expect(e.interestBearing).toEqual({
            rateAuthority: bigKey(53),
            initializationTimestamp: 100n,
            preUpdateAverageRateBps: 5,
            lastUpdateTimestamp: 200n,
            currentRateBps: 7,
        });
        expect(e.scaledUiAmount).toEqual({
            authority: null,
            multiplier: 1.5,
            newMultiplierEffectiveTimestamp: 300n,
            newMultiplier: 2,
        });
        expect(e.pausable).toEqual({ authority: bigKey(58), paused: true });
        expect(e.confidentialTransfer).toEqual({ authority: null, autoApproveNewAccounts: true });
        expect(e.metadataPointer).toEqual({ authority: null, metadataAddress: MINT_B });
        expect(e.tokenMetadata).toEqual({
            updateAuthority: null,
            mint: MINT_B,
            name: 'Hooked',
            symbol: 'HOOK',
            uri: 'https://x.example/t.json',
            additionalMetadata: [['k', 'v']],
        });
        expect(e.groupMemberPointer).toEqual({ authority: null, memberAddress: MINT_B });
        expect(e.tokenGroupMember).toEqual({ mint: MINT_B, group: bigKey(59), memberNumber: 4n });
        expect(e.permissionedBurn).toEqual({ authority: bigKey(60) });
        expect(mint.flags).toEqual([
            'token-2022',
            'mint-authority',
            'freeze-authority',
            'transfer-hook',
            'transfer-hook-authority',
            'transfer-fee',
            'transfer-fee-authority',
            'permanent-delegate',
            'non-transferable',
            'default-frozen',
            'pausable',
            'paused',
            'closable-mint',
            'interest-bearing',
            'scaled-ui-amount',
            'confidential-transfers',
            'permissioned-burn',
            'unknown-extension',
        ]);
    });

    it('picks the older transfer fee before the newer epoch, and takes the epoch as an option', async () => {
        const { rpc, calls } = fakeRpc({ [MINT_B]: { owner: TOKEN_2022_PROGRAM_ADDRESS, data: richMint() } }, 25n);
        const mint = (await inspectMint(rpc, MINT_B, { epoch: 19n }))!;
        expect(mint.extensions.transferFee!.current.basisPoints).toBe(50);
        expect(calls.some((c) => c.address === 'getEpochInfo')).toBe(false);
    });

    it('flags a zero, authority-less transfer fee and hook as absent', () => {
        const data = token2022Mint(mintBase(null, 0n, 0, null), [
            tlv(EXTENSION_TYPES.TransferFeeConfig, concat(none, none, u64(0n), new Uint8Array(36))),
            tlv(EXTENSION_TYPES.TransferHook, concat(none, none)),
        ]);
        expect(decodeMint(MINT_B, TOKEN_2022_PROGRAM_ADDRESS, data)!.flags).toEqual(['token-2022']);
    });

    it('separates a set hook or fee from an authority that could set one', () => {
        const authorityOnly = token2022Mint(mintBase(null, 0n, 0, null), [
            tlv(EXTENSION_TYPES.TransferFeeConfig, concat(addr(bigKey(52)), none, u64(0n), new Uint8Array(36))),
            tlv(EXTENSION_TYPES.TransferHook, concat(addr(bigKey(56)), none)),
        ]);
        expect(decodeMint(MINT_B, TOKEN_2022_PROGRAM_ADDRESS, authorityOnly)!.flags).toEqual([
            'token-2022',
            'transfer-hook-authority',
            'transfer-fee-authority',
        ]);
        // A fee scheduled for a later epoch counts, as does a hook with no authority.
        const scheduled = token2022Mint(mintBase(null, 0n, 0, null), [
            tlv(
                EXTENSION_TYPES.TransferFeeConfig,
                concat(none, none, u64(0n), concat(u64(0n), u64(0n), u16(0)), concat(u64(500n), u64(9n), u16(10))),
            ),
            tlv(EXTENSION_TYPES.TransferHook, concat(none, addr(HOOK))),
        ]);
        expect(decodeMint(MINT_B, TOKEN_2022_PROGRAM_ADDRESS, scheduled, 1n)!.flags).toEqual([
            'token-2022',
            'transfer-hook',
            'transfer-fee',
        ]);
    });

    it('returns null for missing accounts and non-mints, and survives malformed extensions', async () => {
        const { rpc } = fakeRpc({
            [key(1)]: { owner: TOKEN_PROGRAM_ADDRESS, data: new Uint8Array(165) },
            [key(2)]: { owner: key(3), data: mintBase(null, 0n, 0, null) },
        });
        expect(await inspectMint(rpc, key(1))).toBeNull();
        expect(await inspectMint(rpc, key(2))).toBeNull();
        expect(await inspectMint(rpc, key(4))).toBeNull();
        const truncated = token2022Mint(mintBase(null, 0n, 0, null), [
            tlv(EXTENSION_TYPES.TransferHook, [1, 2, 3]),
            concat(u16(EXTENSION_TYPES.PermanentDelegate), u16(32), [1]),
        ]);
        const mint = decodeMint(MINT_B, TOKEN_2022_PROGRAM_ADDRESS, truncated)!;
        expect(mint.unknownExtensions).toEqual([EXTENSION_TYPES.TransferHook]);
        expect(mint.flags).toEqual(['token-2022', 'unknown-extension']);
    });
});

/** A registered, frozen strategy with two markets and the given metadata URI. */
async function world(metadataUri: string) {
    const programId = key(7);
    const { accounts } = await upgradeableProgram(programId, null);
    accounts[await strategyPda(programId)] = {
        owner: PIPELINE_PROGRAM_ADDRESS,
        data: encodeAccount('Strategy', getStrategyCodec(), {
            programId,
            metadataUri,
            interfaceVersion: 1,
            registeredAt: 1_700_000_000n,
            marketCount: 2n,
            bump: 255,
        }),
    };
    const market = await marketPda(programId, key(8));
    accounts[market] = {
        owner: PIPELINE_PROGRAM_ADDRESS,
        data: encodeAccount('Market', getMarketCodec(), {
            strategyProgram: programId,
            marketState: key(8),
            mintCount: 2,
            mints: [MINT_A, MINT_B, key(0), key(0)],
            vaults: [key(16), key(17), key(0), key(0)],
            supportsExactOut: false,
            createdBy: key(30),
            createdAt: 1_700_000_100n,
            bump: 254,
            feeAuthorityBump: 253,
        }),
    };
    accounts[MINT_A] = { owner: TOKEN_PROGRAM_ADDRESS, data: mintBase(null, 1n, 6, null) };
    accounts[MINT_B] = { owner: TOKEN_2022_PROGRAM_ADDRESS, data: richMint() };
    return { programId, market, accounts };
}

describe('inspectStrategy', () => {
    it('combines registration, program, metadata and a matching verified build', async () => {
        const { programId, accounts } = await world('ipfs://bafy/m.json');
        const report = checkReport(programId, expectedHash());
        const metadata = sampleMetadata();
        metadata.conformance = { report: 'https://example.com/report.json', sha256: sha256(report) };
        const { fetch, urls } = fetchFor({
            'https://ipfs.io/ipfs/bafy/m.json': () => Response.json(metadata),
            'https://example.com/report.json': () => new Response(report),
            [`https://verify.osec.io/status/${programId}`]: () => Response.json(osecVerified(expectedHash())),
        });
        const result = await inspectStrategy(fakeRpc(accounts).rpc, programId, { fetch });
        expect(result.registration).toMatchObject({
            registeredAt: 1_700_000_000n,
            marketCount: 2n,
            metadataUri: 'ipfs://bafy/m.json',
        });
        expect(result.metadata.status).toBe('valid');
        expect(result.verifiedBuild?.status).toBe('verified');
        expect(result.flags).toEqual(['frozen', 'verified-build', 'conformance-report']);
        expect(result.conformance).toMatchObject({ status: 'current', reason: null, result: 'pass' });
        expect(urls.sort()).toEqual([
            'https://example.com/report.json',
            'https://ipfs.io/ipfs/bafy/m.json',
            `https://verify.osec.io/status/${programId}`,
        ]);
    });

    it('flags a conformance report for other code as stale, and checks nothing it cannot fetch', async () => {
        const { programId, accounts } = await world('https://x.example/m.json');
        const metadata = sampleMetadata();
        metadata.conformance = { report: 'https://x.example/report.json' };
        const inspect = (routes: Record<string, () => Response>, options = {}) =>
            inspectStrategy(fakeRpc(accounts).rpc, programId, {
                fetch: fetchFor({ 'https://x.example/m.json': () => Response.json(metadata), ...routes }).fetch,
                verifiedBuild: false,
                ...options,
            });

        const stale = await inspect({
            'https://x.example/report.json': () => new Response(checkReport(programId, '11'.repeat(32))),
        });
        expect(stale.flags).toEqual(['frozen', 'conformance-stale']);
        expect(stale.conformance).toMatchObject({ status: 'stale', reason: 'other-code' });

        const missing = await inspect({});
        expect(missing.flags).toEqual(['frozen']);
        expect(missing.conformance).toMatchObject({ status: 'unverified', reason: 'unreachable' });

        const tooLarge = await inspect({
            'https://x.example/report.json': () => new Response('x'.repeat(1024 * 1024 + 1)),
        });
        expect(tooLarge.conformance).toMatchObject({ status: 'unverified', reason: 'unreachable' });

        const unhashed = await inspect(
            { 'https://x.example/report.json': () => new Response(checkReport(programId, expectedHash())) },
            { hashExecutable: false },
        );
        expect(unhashed.flags).toEqual(['frozen']);
        expect(unhashed.conformance?.reason).toBe('code-not-hashed');

        const skipped = await inspect({}, { conformanceReport: false });
        expect(skipped.conformance).toBeNull();
        expect(skipped.flags).toEqual(['frozen']);
    });

    it('treats a verification of different code as unverified', async () => {
        const { programId, accounts } = await world('');
        const { fetch } = fetchFor({
            [`https://verify.osec.io/status/${programId}`]: () => Response.json(osecVerified('00'.repeat(32))),
        });
        const result = await inspectStrategy(fakeRpc(accounts).rpc, programId, { fetch });
        expect(result.flags).toEqual(['frozen', 'unverified-build', 'metadata-missing']);
    });

    it('reports unreachable and invalid metadata, and skips requests when disabled', async () => {
        const { programId, accounts } = await world('https://x.example/m.json');
        const unreachable = await inspectStrategy(fakeRpc(accounts).rpc, programId, { fetch: fetchFor({}).fetch });
        expect(unreachable.metadata.status).toBe('unreachable');
        // The verified-builds API was unreachable too, so neither build flag is set.
        expect(unreachable.flags).toEqual(['frozen', 'metadata-unreachable']);

        const invalid = fetchFor({ 'https://x.example/m.json': () => Response.json({ name: 1 }) });
        const result = await inspectStrategy(fakeRpc(accounts).rpc, programId, {
            fetch: invalid.fetch,
            verifiedBuild: false,
        });
        expect(result.metadata.status).toBe('invalid');
        expect(result.verifiedBuild).toBeNull();
        expect(result.flags).toEqual(['frozen', 'metadata-invalid']);

        const none = fetchFor({});
        const skipped = await inspectStrategy(fakeRpc(accounts).rpc, programId, {
            fetch: none.fetch,
            metadata: false,
            verifiedBuild: false,
        });
        expect(skipped.metadata).toEqual({ status: 'not-fetched', uri: 'https://x.example/m.json' });
        expect(none.urls).toEqual([]);
    });

    it('skips the verified-build lookup off mainnet', async () => {
        const { programId, accounts } = await world('');
        const { fetch, urls } = fetchFor({});
        const devnet = await inspectStrategy(fakeRpc(accounts).rpc, programId, { fetch, cluster: 'devnet' });
        expect(devnet.verifiedBuild).toMatchObject({ status: 'not-applicable', verified: false });
        expect(devnet.flags).toEqual(['frozen', 'metadata-missing']);
        expect(urls).toEqual([]);

        const verified = fetchFor({
            [`https://verify.osec.io/status/${programId}`]: () => Response.json(osecVerified(expectedHash())),
        });
        const mainnet = await inspectStrategy(fakeRpc(accounts).rpc, programId, {
            fetch: verified.fetch,
            cluster: 'mainnet',
        });
        expect(mainnet.flags).toContain('verified-build');
    });

    it('flags an unregistered program', async () => {
        const { accounts } = await upgradeableProgram(key(9), key(1));
        const result = await inspectStrategy(fakeRpc(accounts).rpc, key(9), { metadata: false, verifiedBuild: false });
        expect(result.registration).toBeNull();
        expect(result.flags).toEqual(['upgradeable', 'unregistered']);
    });
});

describe('inspectMarket', () => {
    it('combines the market, its strategy and its mints', async () => {
        const { programId, market, accounts } = await world('');
        const { rpc } = fakeRpc(accounts, 25n);
        const result = (await inspectMarket(rpc, market, { metadata: false, verifiedBuild: false }))!;
        expect(result.mints).toEqual([MINT_A, MINT_B]);
        expect(result.createdBy).toBe(key(30));
        expect(result.createdAt).toBe(1_700_000_100n);
        expect(result.strategy.programId).toBe(programId);
        expect(result.mintInspections.map((m) => m?.tokenProgram)).toEqual([
            TOKEN_PROGRAM_ADDRESS,
            TOKEN_2022_PROGRAM_ADDRESS,
        ]);
        expect(result.flags).toContain('frozen');
        expect(result.flags).toContain('metadata-missing');
        expect(result.flags).toContain('transfer-hook');
        expect(result.flags).not.toContain('mint-unreadable');

        expect(filterMarkets([result], { exclude: ['permanent-delegate'] })).toEqual([]);
        expect(filterMarkets([result], { require: ['frozen'] })).toEqual([result]);
    });

    it('reuses a strategy inspection, flags unreadable mints, and returns null for non-markets', async () => {
        const { programId, market, accounts } = await world('');
        delete accounts[MINT_A];
        const { rpc, calls } = fakeRpc(accounts);
        const strategy = await inspectStrategy(rpc, programId, { metadata: false, verifiedBuild: false });
        calls.length = 0;
        const result = (await inspectMarket(rpc, market, { strategy }))!;
        expect(result.strategy).toBe(strategy);
        expect(result.mintInspections[0]).toBeNull();
        expect(result.flags).toContain('mint-unreadable');
        expect(calls.map((c) => c.address)).not.toContain(programId);
        expect(await inspectMarket(rpc, MINT_B)).toBeNull();
    });
});

describe('audit fixes', () => {
    it('sets no build flag without a hash to compare, and needs OtterSec to report the hash', async () => {
        const { programId, accounts } = await world('');
        const verified = fetchFor({
            [`https://verify.osec.io/status/${programId}`]: () => Response.json(osecVerified(expectedHash())),
        });
        // Without hashing the deployed code, OtterSec's record can't be checked as current.
        const unhashed = await inspectStrategy(fakeRpc(accounts).rpc, programId, {
            fetch: verified.fetch,
            hashExecutable: false,
        });
        expect(unhashed.flags).not.toContain('verified-build');
        expect(unhashed.flags).not.toContain('unverified-build');

        // A "verified" answer that omits the on-chain hash doesn't count.
        const noHash = fetchFor({
            [`https://verify.osec.io/status/${programId}`]: () => Response.json(osecVerified('')),
        });
        const result = await inspectStrategy(fakeRpc(accounts).rpc, programId, { fetch: noHash.fetch });
        expect(result.flags).toContain('unverified-build');
        expect(result.flags).not.toContain('verified-build');
    });

    it('does not flag a transfer fee that was lowered to zero', () => {
        const lowered = token2022Mint(mintBase(null, 0n, 0, null), [
            tlv(
                EXTENSION_TYPES.TransferFeeConfig,
                concat(none, none, u64(0n), concat(u64(0n), u64(9n), u16(10)), concat(u64(5n), u64(0n), u16(0))),
            ),
        ]);
        expect(decodeMint(MINT_B, TOKEN_2022_PROGRAM_ADDRESS, lowered, 10n)!.flags).toEqual(['token-2022']);
        // Before the change takes effect, the old fee is still in force.
        expect(decodeMint(MINT_B, TOKEN_2022_PROGRAM_ADDRESS, lowered, 1n)!.flags).toContain('transfer-fee');
    });

    it('reads an unknown loader-v4 status as neither upgradeable nor frozen', async () => {
        const accounts = {
            [key(7)]: {
                owner: LOADER_ADDRESSES.v4,
                executable: true,
                data: concat(u64(99n), addr(key(9)), u64(7n), ELF, new Uint8Array(10)),
            },
        };
        const result = await inspectProgram(fakeRpc(accounts).rpc, key(7));
        expect(result.loaderV4Status).toBe('unknown');
        expect(result.flags).toEqual([]);
    });
});
