// Strategy metadata, version 1: the JSON document a strategy's on-chain `metadata_uri`
// points to. Desks use it for names, links, trust facts the developer declares, and
// generic forms for creating markets and calling custom instructions.
//
// The JSON Schema is `schema/strategy-metadata.v1.json`. Keys are snake_case, as in the
// JSON. Unknown keys are allowed everywhere and kept in the parsed value, so later minor
// additions don't break older readers. Metadata is untrusted user content: render text
// as text and only follow the URL schemes this module accepts.

import { isAddress, type Address } from '@solana/kit';

import { boundedFetch, parseJsonBody, type FetchLike } from './http.js';

/** The `schema` value of a version 1 document. */
export const STRATEGY_METADATA_SCHEMA = 'pipeline-strategy-metadata/1';

export const STRATEGY_CATEGORIES = ['amm', 'launchpad', 'order-book', 'auction', 'lbp', 'other'] as const;
export type StrategyCategory = (typeof STRATEGY_CATEGORIES)[number];

export const ACTION_KINDS = ['add_liquidity', 'remove_liquidity', 'claim', 'create_market', 'other'] as const;
export type ActionKind = (typeof ACTION_KINDS)[number];

/** Argument types, Borsh-encoded in order after the discriminator. */
export const ARG_TYPES = ['u8', 'u16', 'u32', 'u64', 'i64', 'bool', 'pubkey', 'bytes', 'string'] as const;
export type ArgType = (typeof ARG_TYPES)[number];

/** Whether `url` is an https link on one of `hosts` or a subdomain of one. */
function onHost(url: string, hosts: readonly string[]): boolean {
    try {
        const parsed = new URL(url);
        const host = parsed.hostname.toLowerCase();
        return parsed.protocol === 'https:' && hosts.some((h) => host === h || host.endsWith(`.${h}`));
    } catch {
        return false;
    }
}

/** URL schemes accepted for links, images and reports. */
export const METADATA_URL_SCHEMES = ['https:', 'ipfs:', 'ar:'] as const;

export type MetadataArg = {
    name: string;
    type: ArgType;
    label?: string;
    description?: string;
    /** An account source naming the mint whose decimals a UI should use for this amount. */
    decimals_of?: string;
    [key: string]: unknown;
};

/** An account passed to an instruction, resolved from an account source string. */
export type MetadataAccount = {
    name: string;
    /** See {@link parseAccountSource}. */
    source: string;
    signer?: boolean;
    writable?: boolean;
    [key: string]: unknown;
};

/** A custom instruction on the strategy, such as adding liquidity or claiming. */
export type MetadataAction = {
    /** The instruction's name; for Anchor programs, the IDL instruction name. */
    name: string;
    label: string;
    kind?: ActionKind;
    description?: string;
    /** 8-byte discriminator as 16 hex digits. Defaults to Anchor's `sha256("global:<name>")[..8]`. */
    discriminator?: string;
    args?: MetadataArg[];
    accounts?: MetadataAccount[];
    [key: string]: unknown;
};

/** How to build `create_market`'s `init_data` and the strategy's setup accounts. */
export type MetadataInit = {
    description?: string;
    /** The IDL type `init_data` decodes to, if the strategy publishes an IDL. */
    idl_type?: string;
    /**
     * Where the market state lives: `new_keypair` (the default; a fresh keypair signs
     * `create_market`), or an account source such as a `pda:` source.
     */
    market_state?: string;
    /** Borsh-encoded in order, with no discriminator, to form `init_data`. */
    args?: MetadataArg[];
    /** The strategy's own setup accounts, passed after the router's four fixed accounts. */
    accounts?: MetadataAccount[];
    [key: string]: unknown;
};

export type MetadataLinks = {
    website?: string;
    docs?: string;
    /** A human-facing link to the source code. Verified builds use `source.repository`. */
    source?: string;
    /** The strategy's Anchor IDL or Codama document. */
    idl?: string;
    /** X (Twitter) profile. */
    twitter?: string;
    /** GitHub organisation, user or repository. */
    github?: string;
    /** Discord invite. */
    discord?: string;
    /** Telegram group or channel. */
    telegram?: string;
    /** Farcaster profile or channel. */
    farcaster?: string;
    [key: string]: unknown;
};

/**
 * The sites each social link is expected on. A link elsewhere is still accepted, with a
 * warning, so a desk can decide whether to show it.
 */
export const SOCIAL_LINK_HOSTS = {
    twitter: ['x.com', 'twitter.com'],
    github: ['github.com'],
    discord: ['discord.gg', 'discord.com'],
    telegram: ['t.me', 'telegram.me'],
    farcaster: ['farcaster.xyz', 'warpcast.com'],
} as const;

/** The code a verified build was made from. */
export type MetadataSource = {
    repository: string;
    commit?: string;
    /** The program's directory within the repository. */
    path?: string;
    [key: string]: unknown;
};

/** A `pipeline-check` conformance report for the deployed program. */
export type MetadataConformance = {
    report: string;
    /** Hex SHA-256 of the report file. */
    sha256?: string;
    [key: string]: unknown;
};

export type MetadataAudit = {
    by: string;
    url?: string;
    /** `YYYY-MM-DD`. */
    date?: string;
    [key: string]: unknown;
};

export type MetadataSecurity = {
    /** Email address, URL or handle for reporting vulnerabilities. */
    contact: string;
    policy?: string;
    audits?: MetadataAudit[];
    [key: string]: unknown;
};

export type StrategyMetadata = {
    schema: typeof STRATEGY_METADATA_SCHEMA;
    name: string;
    description?: string;
    image?: string;
    category?: StrategyCategory;
    tags?: string[];
    links?: MetadataLinks;
    source?: MetadataSource;
    init?: MetadataInit;
    actions?: MetadataAction[];
    conformance?: MetadataConformance;
    security?: MetadataSecurity;
    [key: string]: unknown;
};

/**
 * `warnings` report values a newer minor revision may add (an unknown category or action
 * kind); the document is still usable.
 */
export type ParseMetadataResult =
    { ok: true; value: StrategyMetadata; warnings: string[] } | { ok: false; errors: string[] };

// ---------------------------------------------------------------------------------------
// Account sources
// ---------------------------------------------------------------------------------------

/** One seed of a `pda:` source. */
export type PdaSeed =
    | { kind: 'str'; text: string }
    | { kind: 'hex'; bytes: Uint8Array }
    | { kind: 'key'; source: AccountSource }
    /** The Borsh encoding of the named argument. */
    | { kind: 'arg'; name: string };

/** A parsed account source string. */
export type AccountSource =
    | { kind: 'wallet' }
    | { kind: 'market_state' }
    | { kind: 'market' }
    | { kind: 'strategy_program' }
    | { kind: 'system_program' }
    | { kind: 'associated_token_program' }
    | { kind: 'mint' | 'vault' | 'token_program' | 'wallet_token_account'; index: number }
    | { kind: 'fixed'; address: Address }
    | { kind: 'input'; name: string }
    | { kind: 'pda'; program: Address | 'strategy'; seeds: PdaSeed[] }
    | { kind: 'ata'; owner: AccountSource; mint: AccountSource }
    | { kind: 'token_program_of'; mint: AccountSource };

const SIMPLE_SOURCES = [
    'wallet',
    'market_state',
    'market',
    'strategy_program',
    'system_program',
    'associated_token_program',
] as const;
const INDEXED_SOURCES = ['mint', 'vault', 'token_program', 'wallet_token_account'] as const;
const NAME = /^[A-Za-z_][A-Za-z0-9_]{0,63}$/;
const HEX = /^(?:[0-9a-fA-F]{2})*$/;

/**
 * Parses an account source string.
 *
 * | Source | Account |
 * |---|---|
 * | `wallet` | The user's wallet (the creator, for `init`) |
 * | `market_state`, `market` | The market's state account; the router's market account |
 * | `strategy_program`, `system_program`, `associated_token_program` | Those programs |
 * | `mint:i`, `vault:i`, `token_program:i` | The market's mint, vault, token program at index `i` |
 * | `wallet_token_account:i` | The user's associated token account for mint `i` |
 * | `fixed:<address>` | A fixed address |
 * | `input:<name>` | An address the user enters |
 * | `pda:<program>:<seed>,<seed>…` | A PDA of `<program>` (an address, or `strategy`) |
 * | `ata:<owner>,<mint>` | An associated token account; `<owner>` and `<mint>` are sources |
 * | `token_program_of:<mint>` | The token program that owns a mint given by a source |
 *
 * A `pda:` seed is `str:<text>` (no commas), `hex:<bytes>`, `key:<source>` or
 * `arg:<name>` (the argument's Borsh bytes). Sources nested in `pda:`, `ata:` and
 * `token_program_of:` cannot themselves be `pda:` or `ata:` sources.
 */
export function parseAccountSource(text: string): AccountSource | string {
    return parseSource(text);
}

function parseSource(text: string): AccountSource | string {
    if ((SIMPLE_SOURCES as readonly string[]).includes(text)) {
        return { kind: text as (typeof SIMPLE_SOURCES)[number] };
    }
    const colon = text.indexOf(':');
    if (colon < 0) {
        return `unknown account source "${text}"`;
    }
    const prefix = text.slice(0, colon);
    const rest = text.slice(colon + 1);
    if ((INDEXED_SOURCES as readonly string[]).includes(prefix)) {
        if (!/^[0-3]$/.test(rest)) {
            return `"${text}": index must be 0 to 3`;
        }
        return { kind: prefix as (typeof INDEXED_SOURCES)[number], index: Number(rest) };
    }
    switch (prefix) {
        case 'fixed':
            return isAddress(rest) ? { kind: 'fixed', address: rest } : `"${text}": not an address`;
        case 'input':
            return NAME.test(rest) ? { kind: 'input', name: rest } : `"${text}": invalid input name`;
        case 'token_program_of': {
            const mint = parseNested(rest, text);
            return typeof mint === 'string' ? mint : { kind: 'token_program_of', mint };
        }
        case 'ata': {
            const parts = rest.split(',');
            if (parts.length !== 2) {
                return `"${text}": expected ata:<owner>,<mint>`;
            }
            const owner = parseNested(parts[0]!, text);
            const mint = parseNested(parts[1]!, text);
            if (typeof owner === 'string') {
                return owner;
            }
            if (typeof mint === 'string') {
                return mint;
            }
            return { kind: 'ata', owner, mint };
        }
        case 'pda':
            return parsePda(rest, text);
        default:
            return `unknown account source "${text}"`;
    }
}

function parseNested(text: string, whole: string): AccountSource | string {
    if (text.startsWith('pda:') || text.startsWith('ata:')) {
        return `"${whole}": pda and ata sources cannot be nested`;
    }
    const source = parseSource(text);
    return typeof source === 'string' ? `"${whole}": ${source}` : source;
}

function parsePda(rest: string, whole: string): AccountSource | string {
    const colon = rest.indexOf(':');
    const programText = colon < 0 ? rest : rest.slice(0, colon);
    const seedText = colon < 0 ? '' : rest.slice(colon + 1);
    let program: Address | 'strategy';
    if (programText === 'strategy') {
        program = 'strategy';
    } else if (isAddress(programText)) {
        program = programText;
    } else {
        return `"${whole}": pda program must be an address or "strategy"`;
    }
    const seeds: PdaSeed[] = [];
    for (const seed of seedText === '' ? [] : seedText.split(',')) {
        const parsed = parseSeed(seed, whole);
        if (typeof parsed === 'string') {
            return parsed;
        }
        seeds.push(parsed);
    }
    // Solana allows 16 seeds including the bump, which the SDK adds.
    if (seeds.length > 15) {
        return `"${whole}": at most 15 seeds`;
    }
    return { kind: 'pda', program, seeds };
}

function parseSeed(seed: string, whole: string): PdaSeed | string {
    const colon = seed.indexOf(':');
    const kind = colon < 0 ? seed : seed.slice(0, colon);
    const value = colon < 0 ? '' : seed.slice(colon + 1);
    switch (kind) {
        case 'str':
            return new TextEncoder().encode(value).length <= 32
                ? { kind: 'str', text: value }
                : `"${whole}": str seed longer than 32 bytes`;
        case 'hex':
            if (!HEX.test(value) || value.length > 64) {
                return `"${whole}": hex seed must be at most 32 bytes of hex`;
            }
            return { kind: 'hex', bytes: hexToBytes(value) };
        case 'key': {
            const source = parseNested(value, whole);
            return typeof source === 'string' ? source : { kind: 'key', source };
        }
        case 'arg':
            return NAME.test(value) ? { kind: 'arg', name: value } : `"${whole}": invalid arg seed name`;
        default:
            return `"${whole}": unknown seed "${seed}" (expected str:, hex:, key: or arg:)`;
    }
}

/** Sources that only exist once a market does, so `init` cannot use them. */
export function sourceNeedsMarket(source: AccountSource): boolean {
    switch (source.kind) {
        case 'mint':
        case 'vault':
        case 'token_program':
        case 'wallet_token_account':
            return true;
        case 'ata':
            return sourceNeedsMarket(source.owner) || sourceNeedsMarket(source.mint);
        case 'token_program_of':
            return sourceNeedsMarket(source.mint);
        case 'pda':
            return source.seeds.some((seed) => seed.kind === 'key' && sourceNeedsMarket(seed.source));
        default:
            return false;
    }
}

export function hexToBytes(hex: string): Uint8Array {
    const bytes = new Uint8Array(hex.length / 2);
    for (let i = 0; i < bytes.length; i++) {
        bytes[i] = parseInt(hex.slice(2 * i, 2 * i + 2), 16);
    }
    return bytes;
}

// ---------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------

type Json = Record<string, unknown>;

class Checker {
    errors: string[] = [];
    warnings: string[] = [];

    error(path: string, message: string): void {
        this.errors.push(`${path}: ${message}`);
    }

    object(value: unknown, path: string): value is Json {
        if (value === null || typeof value !== 'object' || Array.isArray(value)) {
            this.error(path, 'must be an object');
            return false;
        }
        return true;
    }

    string(value: unknown, path: string, maxLength: number, required = false): value is string {
        if (value === undefined && !required) {
            return false;
        }
        if (typeof value !== 'string' || (required && value.length === 0)) {
            this.error(path, required ? 'must be a non-empty string' : 'must be a string');
            return false;
        }
        if (value.length > maxLength) {
            this.error(path, `must be at most ${maxLength} characters`);
            return false;
        }
        return true;
    }

    url(value: unknown, path: string, required = false): void {
        if (!this.string(value, path, 512, required)) {
            return;
        }
        let scheme: string;
        try {
            scheme = new URL(value).protocol;
        } catch {
            this.error(path, 'must be a URL');
            return;
        }
        if (!(METADATA_URL_SCHEMES as readonly string[]).includes(scheme)) {
            this.error(path, `URL scheme must be one of ${METADATA_URL_SCHEMES.join(', ')}`);
        }
    }

    boolean(value: unknown, path: string): void {
        if (value !== undefined && typeof value !== 'boolean') {
            this.error(path, 'must be a boolean');
        }
    }

    array(value: unknown, path: string, maxItems: number): unknown[] | null {
        if (value === undefined) {
            return null;
        }
        if (!Array.isArray(value)) {
            this.error(path, 'must be an array');
            return null;
        }
        if (value.length > maxItems) {
            this.error(path, `must have at most ${maxItems} items`);
            return null;
        }
        return value;
    }
}

function checkArgs(c: Checker, value: unknown, path: string, init: boolean): Set<string> {
    const names = new Set<string>();
    const args = c.array(value, path, 32) ?? [];
    args.forEach((arg, i) => {
        const p = `${path}[${i}]`;
        if (!c.object(arg, p)) {
            return;
        }
        if (typeof arg.name !== 'string' || !NAME.test(arg.name)) {
            c.error(`${p}.name`, 'must be an identifier (letters, digits, _)');
        } else if (names.has(arg.name)) {
            c.error(`${p}.name`, `duplicate argument "${arg.name}"`);
        } else {
            names.add(arg.name);
        }
        if (!(ARG_TYPES as readonly unknown[]).includes(arg.type)) {
            c.error(`${p}.type`, `must be one of ${ARG_TYPES.join(', ')}`);
        }
        c.string(arg.label, `${p}.label`, 64);
        c.string(arg.description, `${p}.description`, 1000);
        if (c.string(arg.decimals_of, `${p}.decimals_of`, 256)) {
            checkSource(c, arg.decimals_of, `${p}.decimals_of`, init, names);
        }
    });
    return names;
}

function checkSource(c: Checker, text: string, path: string, init: boolean, argNames: Set<string>): void {
    const source = parseAccountSource(text);
    if (typeof source === 'string') {
        c.error(path, source);
        return;
    }
    if (init && sourceNeedsMarket(source)) {
        c.error(path, `"${text}" refers to the market's mints, which don't exist before the market is created`);
    }
    if (source.kind === 'pda') {
        for (const seed of source.seeds) {
            if (seed.kind === 'arg' && !argNames.has(seed.name)) {
                c.error(path, `seed refers to unknown argument "${seed.name}"`);
            }
        }
    }
}

function checkAccounts(c: Checker, value: unknown, path: string, init: boolean, argNames: Set<string>): void {
    const accounts = c.array(value, path, 64) ?? [];
    accounts.forEach((account, i) => {
        const p = `${path}[${i}]`;
        if (!c.object(account, p)) {
            return;
        }
        c.string(account.name, `${p}.name`, 64, true);
        if (c.string(account.source, `${p}.source`, 512, true)) {
            checkSource(c, account.source, `${p}.source`, init, argNames);
        }
        c.boolean(account.signer, `${p}.signer`);
        c.boolean(account.writable, `${p}.writable`);
    });
}

function checkInit(c: Checker, init: unknown): void {
    if (!c.object(init, 'init')) {
        return;
    }
    c.string(init.description, 'init.description', 2000);
    c.string(init.idl_type, 'init.idl_type', 128);
    const argNames = checkArgs(c, init.args, 'init.args', true);
    if (c.string(init.market_state, 'init.market_state', 512) && init.market_state !== 'new_keypair') {
        checkSource(c, init.market_state, 'init.market_state', true, argNames);
        if (init.market_state === 'market_state' || init.market_state === 'market') {
            c.error('init.market_state', 'cannot refer to the market itself');
        }
    }
    checkAccounts(c, init.accounts, 'init.accounts', true, argNames);
}

function checkActions(c: Checker, value: unknown): void {
    const actions = c.array(value, 'actions', 64) ?? [];
    const names = new Set<string>();
    actions.forEach((action, i) => {
        const p = `actions[${i}]`;
        if (!c.object(action, p)) {
            return;
        }
        if (typeof action.name !== 'string' || !NAME.test(action.name)) {
            c.error(`${p}.name`, 'must be an identifier (letters, digits, _)');
        } else if (names.has(action.name)) {
            c.error(`${p}.name`, `duplicate action "${action.name}"`);
        } else {
            names.add(action.name);
        }
        c.string(action.label, `${p}.label`, 64, true);
        c.string(action.description, `${p}.description`, 2000);
        if (action.kind !== undefined) {
            if (typeof action.kind !== 'string') {
                c.error(`${p}.kind`, 'must be a string');
            } else if (!(ACTION_KINDS as readonly string[]).includes(action.kind)) {
                c.warnings.push(`${p}.kind: unknown kind "${action.kind}"`);
            }
        }
        if (
            action.discriminator !== undefined &&
            (typeof action.discriminator !== 'string' || !/^[0-9a-fA-F]{16}$/.test(action.discriminator))
        ) {
            c.error(`${p}.discriminator`, 'must be 16 hex digits');
        }
        const argNames = checkArgs(c, action.args, `${p}.args`, false);
        checkAccounts(c, action.accounts, `${p}.accounts`, false, argNames);
    });
}

/**
 * Validates a parsed JSON value as strategy metadata version 1. Unknown keys are allowed
 * and kept. Never throws.
 */
export function parseStrategyMetadata(json: unknown): ParseMetadataResult {
    const c = new Checker();
    if (!c.object(json, '$')) {
        return { ok: false, errors: c.errors };
    }
    if (json.schema !== STRATEGY_METADATA_SCHEMA) {
        c.error('schema', `must be "${STRATEGY_METADATA_SCHEMA}"`);
    }
    c.string(json.name, 'name', 64, true);
    c.string(json.description, 'description', 2000);
    c.url(json.image, 'image');
    if (json.category !== undefined) {
        if (typeof json.category !== 'string') {
            c.error('category', 'must be a string');
        } else if (!(STRATEGY_CATEGORIES as readonly string[]).includes(json.category)) {
            c.warnings.push(`category: unknown category "${json.category}"`);
        }
    }
    const tags = c.array(json.tags, 'tags', 16) ?? [];
    tags.forEach((tag, i) => c.string(tag, `tags[${i}]`, 32, true));
    if (json.links !== undefined && c.object(json.links, 'links')) {
        for (const [key, value] of Object.entries(json.links)) {
            c.url(value, `links.${key}`);
            const hosts = (SOCIAL_LINK_HOSTS as Record<string, readonly string[]>)[key];
            if (hosts && typeof value === 'string' && !onHost(value, hosts)) {
                c.warnings.push(`links.${key}: expected a link on ${hosts.join(' or ')}`);
            }
        }
    }
    if (json.source !== undefined && c.object(json.source, 'source')) {
        c.url(json.source.repository, 'source.repository', true);
        if (
            json.source.commit !== undefined &&
            (typeof json.source.commit !== 'string' || !/^[0-9a-fA-F]{7,64}$/.test(json.source.commit))
        ) {
            c.error('source.commit', 'must be a commit hash (7 to 64 hex digits)');
        }
        c.string(json.source.path, 'source.path', 256);
    }
    if (json.init !== undefined) {
        checkInit(c, json.init);
    }
    checkActions(c, json.actions);
    if (json.conformance !== undefined && c.object(json.conformance, 'conformance')) {
        c.url(json.conformance.report, 'conformance.report', true);
        if (
            json.conformance.sha256 !== undefined &&
            (typeof json.conformance.sha256 !== 'string' || !/^[0-9a-fA-F]{64}$/.test(json.conformance.sha256))
        ) {
            c.error('conformance.sha256', 'must be 64 hex digits');
        }
    }
    if (json.security !== undefined && c.object(json.security, 'security')) {
        c.string(json.security.contact, 'security.contact', 256, true);
        c.url(json.security.policy, 'security.policy');
        const audits = c.array(json.security.audits, 'security.audits', 32) ?? [];
        audits.forEach((audit, i) => {
            const p = `security.audits[${i}]`;
            if (!c.object(audit, p)) {
                return;
            }
            c.string(audit.by, `${p}.by`, 128, true);
            c.url(audit.url, `${p}.url`);
            if (
                audit.date !== undefined &&
                (typeof audit.date !== 'string' ||
                    !/^\d{4}-\d{2}-\d{2}$/.test(audit.date) ||
                    Number.isNaN(Date.parse(audit.date)))
            ) {
                c.error(`${p}.date`, 'must be a date as YYYY-MM-DD');
            }
        });
    }
    if (c.errors.length > 0) {
        return { ok: false, errors: c.errors };
    }
    return { ok: true, value: json as StrategyMetadata, warnings: c.warnings };
}

// ---------------------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------------------

/** HTTP gateways for `ipfs://` and `ar://` URIs. Each ends with `/`. */
export type MetadataGateways = {
    ipfs?: string;
    arweave?: string;
};

export const DEFAULT_GATEWAYS: Required<MetadataGateways> = {
    ipfs: 'https://ipfs.io/ipfs/',
    arweave: 'https://arweave.net/',
};

export const DEFAULT_METADATA_MAX_BYTES = 64 * 1024;
export const DEFAULT_METADATA_TIMEOUT_MS = 10_000;

export type FetchMetadataOptions = {
    /** Defaults to the global `fetch`. */
    fetch?: FetchLike;
    gateways?: MetadataGateways;
    /** Defaults to 64 KiB. */
    maxBytes?: number;
    /** Defaults to 10 seconds. */
    timeoutMs?: number;
    signal?: AbortSignal;
};

export type FetchMetadataFailure =
    'missing' | 'unsupported-uri' | 'network' | 'timeout' | 'http-status' | 'too-large' | 'not-json' | 'invalid';

export type FetchMetadataResult =
    | { ok: true; url: string; value: StrategyMetadata; warnings: string[] }
    | { ok: false; url: string | null; reason: FetchMetadataFailure; errors: string[] };

/**
 * The HTTPS URL to fetch for a metadata URI: `https://` as is, `ipfs://<cid>/<path>` and
 * `ar://<id>/<path>` through the gateways. `null` for anything else (including `http://`).
 */
export function resolveMetadataUri(uri: string, gateways: MetadataGateways = {}): string | null {
    const ipfs = gateways.ipfs ?? DEFAULT_GATEWAYS.ipfs;
    const arweave = gateways.arweave ?? DEFAULT_GATEWAYS.arweave;
    const trimmed = uri.trim();
    if (trimmed.startsWith('ipfs://')) {
        const path = trimmed.slice('ipfs://'.length).replace(/^ipfs\//, '');
        return path === '' ? null : ipfs + path;
    }
    if (trimmed.startsWith('ar://')) {
        const path = trimmed.slice('ar://'.length);
        return path === '' ? null : arweave + path;
    }
    try {
        return new URL(trimmed).protocol === 'https:' ? trimmed : null;
    } catch {
        return null;
    }
}

/**
 * Fetches and validates a strategy's metadata document. Never throws: network failures,
 * timeouts, oversized bodies and invalid documents are reported in the result.
 */
export async function fetchStrategyMetadata(
    uri: string,
    options: FetchMetadataOptions = {},
): Promise<FetchMetadataResult> {
    if (uri.trim() === '') {
        return { ok: false, url: null, reason: 'missing', errors: ['no metadata URI'] };
    }
    const url = resolveMetadataUri(uri, options.gateways);
    if (url === null) {
        return { ok: false, url: null, reason: 'unsupported-uri', errors: [`unsupported URI "${uri}"`] };
    }
    const response = await boundedFetch(url, {
        fetch: options.fetch,
        maxBytes: options.maxBytes ?? DEFAULT_METADATA_MAX_BYTES,
        timeoutMs: options.timeoutMs ?? DEFAULT_METADATA_TIMEOUT_MS,
        signal: options.signal,
    });
    if (!response.ok) {
        return { ok: false, url, reason: response.reason, errors: [response.message] };
    }
    const json = parseJsonBody(response.body);
    if (json === undefined) {
        return { ok: false, url, reason: 'not-json', errors: ['body is not UTF-8 JSON'] };
    }
    const parsed = parseStrategyMetadata(json);
    if (!parsed.ok) {
        return { ok: false, url, reason: 'invalid', errors: parsed.errors };
    }
    return { ok: true, url, value: parsed.value, warnings: parsed.warnings };
}
