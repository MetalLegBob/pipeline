// Turns a metadata `action` or `init` description into real instruction data and
// accounts, so a desk can render a form for any strategy and submit it without
// strategy-specific code.

import {
    AccountRole,
    getAddressEncoder,
    getI64Encoder,
    getProgramDerivedAddress,
    getU16Encoder,
    getU32Encoder,
    getU64Encoder,
    getU8Encoder,
    isAddress,
    type AccountMeta,
    type Address,
    type GetAccountInfoApi,
    type ReadonlyUint8Array,
    type Rpc,
} from '@solana/kit';

import { getBorshBytesCodec, getBorshStringCodec } from './codecs.js';
import {
    ASSOCIATED_TOKEN_PROGRAM_ADDRESS,
    SYSTEM_PROGRAM_ADDRESS,
    TOKEN_2022_PROGRAM_ADDRESS,
    TOKEN_PROGRAM_ADDRESS,
} from './constants.js';
import { createMarket, type PipelineInstruction } from './instructions.js';
import {
    hexToBytes,
    parseAccountSource,
    sourceNeedsMarket,
    type AccountSource,
    type ArgType,
    type MetadataAccount,
    type MetadataAction,
    type MetadataArg,
    type MetadataInit,
} from './metadata.js';
import { associatedTokenAddress, marketPda } from './pda.js';
import type { MarketView } from './quote.js';

/** A value for one metadata argument. Integers may be given as decimal strings. */
export type ArgValue = bigint | number | string | boolean | ReadonlyUint8Array;

/** Thrown when an action or init description can't be turned into an instruction. */
export class MetadataInstructionError extends Error {
    override name = 'MetadataInstructionError';
}

/** Everything account sources may refer to. Leave out what doesn't apply. */
export type SourceContext = {
    strategyProgram: Address;
    /** The user's wallet; the creator for `init`. */
    wallet?: Address;
    marketState?: Address;
    /** Mints, vaults and token programs, for an existing market. */
    market?: MarketView;
    /** Values for `input:<name>` sources. */
    inputs?: Record<string, Address>;
    /** Encoded arguments, for `arg:<name>` seeds. */
    args?: Record<string, Uint8Array>;
    /**
     * The token program owning each mint, for `ata:` and `token_program_of:` sources whose
     * mint isn't one of the market's. Looked up with `rpc` if missing.
     */
    tokenPrograms?: Record<string, Address>;
    rpc?: Rpc<GetAccountInfoApi>;
};

const addressEncoder = getAddressEncoder();
const utf8 = new TextEncoder();

/** Anchor's instruction discriminator, `sha256("global:<name>")[..8]`. */
export async function anchorDiscriminator(name: string): Promise<Uint8Array> {
    const digest = await crypto.subtle.digest('SHA-256', utf8.encode(`global:${name}`));
    return new Uint8Array(digest).slice(0, 8);
}

function fail(message: string): never {
    throw new MetadataInstructionError(message);
}

function integer(value: ArgValue, name: string): bigint {
    if (typeof value === 'bigint') {
        return value;
    }
    if (typeof value === 'number' && Number.isSafeInteger(value)) {
        return BigInt(value);
    }
    if (typeof value === 'string' && /^-?\d+$/.test(value.trim())) {
        return BigInt(value.trim());
    }
    return fail(`argument "${name}" must be an integer`);
}

const INTEGER_RANGES: Partial<Record<ArgType, [bigint, bigint]>> = {
    u8: [0n, 0xffn],
    u16: [0n, 0xffffn],
    u32: [0n, 0xffff_ffffn],
    u64: [0n, 0xffff_ffff_ffff_ffffn],
    i64: [-(1n << 63n), (1n << 63n) - 1n],
};

/** Borsh-encodes one argument value as its declared type. */
export function encodeArg(type: ArgType, value: ArgValue, name = 'value'): Uint8Array {
    const range = INTEGER_RANGES[type];
    if (range) {
        const n = integer(value, name);
        if (n < range[0] || n > range[1]) {
            fail(`argument "${name}" is out of range for ${type}`);
        }
        switch (type) {
            case 'u8':
                return getU8Encoder().encode(Number(n)) as Uint8Array;
            case 'u16':
                return getU16Encoder().encode(Number(n)) as Uint8Array;
            case 'u32':
                return getU32Encoder().encode(Number(n)) as Uint8Array;
            case 'u64':
                return getU64Encoder().encode(n) as Uint8Array;
            default:
                return getI64Encoder().encode(n) as Uint8Array;
        }
    }
    switch (type) {
        case 'bool':
            if (value === true || value === 'true') {
                return new Uint8Array([1]);
            }
            if (value === false || value === 'false') {
                return new Uint8Array([0]);
            }
            return fail(`argument "${name}" must be a boolean`);
        case 'pubkey':
            if (typeof value !== 'string' || !isAddress(value)) {
                return fail(`argument "${name}" must be an address`);
            }
            return addressEncoder.encode(value) as Uint8Array;
        case 'bytes': {
            let bytes: ReadonlyUint8Array;
            if (value instanceof Uint8Array) {
                bytes = value;
            } else if (typeof value === 'string' && /^(?:0x)?(?:[0-9a-fA-F]{2})*$/.test(value)) {
                bytes = hexToBytes(value.replace(/^0x/, ''));
            } else {
                return fail(`argument "${name}" must be bytes or a hex string`);
            }
            return getBorshBytesCodec().encode(bytes) as Uint8Array;
        }
        case 'string':
            if (typeof value !== 'string') {
                return fail(`argument "${name}" must be a string`);
            }
            return getBorshStringCodec().encode(value) as Uint8Array;
        default:
            return fail(`unknown argument type "${type as string}"`);
    }
}

/** Encodes every argument by name. Fails on a missing value. */
export function encodeArgs(
    specs: readonly MetadataArg[],
    values: Record<string, ArgValue>,
): Record<string, Uint8Array> {
    const encoded: Record<string, Uint8Array> = {};
    for (const spec of specs) {
        const value = values[spec.name];
        if (value === undefined) {
            fail(`missing argument "${spec.name}"`);
        }
        encoded[spec.name] = encodeArg(spec.type, value, spec.name);
    }
    return encoded;
}

function concat(parts: readonly Uint8Array[]): Uint8Array {
    const total = parts.reduce((sum, part) => sum + part.length, 0);
    const out = new Uint8Array(total);
    let offset = 0;
    for (const part of parts) {
        out.set(part, offset);
        offset += part.length;
    }
    return out;
}

function parsed(text: string): AccountSource {
    const source = parseAccountSource(text);
    return typeof source === 'string' ? fail(source) : source;
}

function marketIndex(context: SourceContext, kind: string, index: number) {
    const market = context.market ?? fail(`"${kind}:${index}" needs a market`);
    const mint = market.mints[index] ?? fail(`the market has no mint ${index}`);
    return { market, mint, vault: market.vaults[index], tokenProgram: market.tokenPrograms[index] };
}

async function tokenProgramOf(context: SourceContext, mint: Address): Promise<Address> {
    const index = context.market?.mints.indexOf(mint) ?? -1;
    if (index >= 0 && context.market?.tokenPrograms[index]) {
        return context.market.tokenPrograms[index];
    }
    const known = context.tokenPrograms?.[mint];
    if (known) {
        return known;
    }
    if (!context.rpc) {
        return fail(`token program of ${mint} unknown: pass tokenPrograms or rpc`);
    }
    const { value } = await context.rpc
        .getAccountInfo(mint, { encoding: 'base64', dataSlice: { offset: 0, length: 0 } })
        .send();
    if (!value || (value.owner !== TOKEN_PROGRAM_ADDRESS && value.owner !== TOKEN_2022_PROGRAM_ADDRESS)) {
        return fail(`${mint} is not a token mint`);
    }
    return value.owner;
}

/** Resolves an account source to an address. */
export async function resolveAccountSource(source: string | AccountSource, context: SourceContext): Promise<Address> {
    const s = typeof source === 'string' ? parsed(source) : source;
    switch (s.kind) {
        case 'wallet':
            return context.wallet ?? fail('"wallet" needs a wallet');
        case 'market_state':
            return context.marketState ?? context.market?.marketState ?? fail('"market_state" needs a market state');
        case 'market': {
            if (context.market) {
                return context.market.market;
            }
            const marketState = context.marketState ?? fail('"market" needs a market state');
            return marketPda(context.strategyProgram, marketState);
        }
        case 'strategy_program':
            return context.strategyProgram;
        case 'system_program':
            return SYSTEM_PROGRAM_ADDRESS;
        case 'associated_token_program':
            return ASSOCIATED_TOKEN_PROGRAM_ADDRESS;
        case 'mint':
            return marketIndex(context, s.kind, s.index).mint;
        case 'vault':
            return marketIndex(context, s.kind, s.index).vault ?? fail(`the market has no vault ${s.index}`);
        case 'token_program':
            return marketIndex(context, s.kind, s.index).tokenProgram ?? fail(`no token program for mint ${s.index}`);
        case 'wallet_token_account': {
            const { mint, tokenProgram } = marketIndex(context, s.kind, s.index);
            const wallet = context.wallet ?? fail('"wallet_token_account" needs a wallet');
            return associatedTokenAddress(wallet, mint, tokenProgram ?? (await tokenProgramOf(context, mint)));
        }
        case 'fixed':
            return s.address;
        case 'input': {
            const value = context.inputs?.[s.name] ?? fail(`missing input "${s.name}"`);
            return isAddress(value) ? value : fail(`input "${s.name}" is not an address`);
        }
        case 'ata': {
            const owner = await resolveAccountSource(s.owner, context);
            const mint = await resolveAccountSource(s.mint, context);
            return associatedTokenAddress(owner, mint, await tokenProgramOf(context, mint));
        }
        case 'token_program_of':
            return tokenProgramOf(context, await resolveAccountSource(s.mint, context));
        case 'pda': {
            const seeds: Uint8Array[] = [];
            for (const seed of s.seeds) {
                switch (seed.kind) {
                    case 'str':
                        seeds.push(utf8.encode(seed.text));
                        break;
                    case 'hex':
                        seeds.push(seed.bytes);
                        break;
                    case 'key':
                        seeds.push(
                            addressEncoder.encode(await resolveAccountSource(seed.source, context)) as Uint8Array,
                        );
                        break;
                    case 'arg': {
                        const bytes = context.args?.[seed.name] ?? fail(`seed needs argument "${seed.name}"`);
                        if (bytes.length > 32) {
                            fail(`argument "${seed.name}" is longer than 32 bytes and can't be a seed`);
                        }
                        seeds.push(bytes);
                        break;
                    }
                }
            }
            const programAddress = s.program === 'strategy' ? context.strategyProgram : s.program;
            const [pda] = await getProgramDerivedAddress({ programAddress, seeds });
            return pda;
        }
    }
}

function role(account: MetadataAccount): AccountRole {
    if (account.signer) {
        return account.writable ? AccountRole.WRITABLE_SIGNER : AccountRole.READONLY_SIGNER;
    }
    return account.writable ? AccountRole.WRITABLE : AccountRole.READONLY;
}

/** Resolves an ordered account list. */
export async function resolveMetadataAccounts(
    accounts: readonly MetadataAccount[],
    context: SourceContext,
): Promise<AccountMeta[]> {
    const metas: AccountMeta[] = [];
    for (const account of accounts) {
        metas.push({ address: await resolveAccountSource(account.source, context), role: role(account) });
    }
    return metas;
}

/**
 * Builds a custom action from strategy metadata as an instruction to the strategy
 * program: the discriminator (given, or Anchor's from the action name), the Borsh-encoded
 * arguments, and the resolved accounts. The user signs it directly; it doesn't go through
 * the router.
 */
export async function buildActionInstruction(input: {
    action: MetadataAction;
    market: MarketView;
    wallet: Address;
    args?: Record<string, ArgValue>;
    inputs?: Record<string, Address>;
    tokenPrograms?: Record<string, Address>;
    rpc?: Rpc<GetAccountInfoApi>;
}): Promise<PipelineInstruction> {
    const specs = input.action.args ?? [];
    const args = encodeArgs(specs, input.args ?? {});
    const context: SourceContext = {
        strategyProgram: input.market.strategyProgram,
        wallet: input.wallet,
        market: input.market,
        marketState: input.market.marketState,
        inputs: input.inputs,
        args,
        tokenPrograms: input.tokenPrograms,
        rpc: input.rpc,
    };
    const discriminator = input.action.discriminator
        ? hexToBytes(input.action.discriminator)
        : await anchorDiscriminator(input.action.name);
    return {
        programAddress: input.market.strategyProgram,
        accounts: await resolveMetadataAccounts(input.action.accounts ?? [], context),
        data: concat([discriminator, ...specs.map((spec) => args[spec.name]!)]),
    };
}

/** What `create_market` needs from a strategy's `init` description. */
export type ResolvedInit = {
    marketState: Address;
    /** `true` when the market state is a new keypair that must sign the transaction. */
    marketStateSigns: boolean;
    /** The market's router account. */
    market: Address;
    initData: Uint8Array;
    strategyAccounts: AccountMeta[];
};

/**
 * Resolves a strategy's metadata `init` into `create_market` inputs. When the market
 * state is a `new_keypair` (the default), pass the new keypair's address as
 * `marketState`; it must sign.
 */
export async function resolveInit(input: {
    init: MetadataInit;
    strategyProgram: Address;
    creator: Address;
    marketState?: Address;
    args?: Record<string, ArgValue>;
    inputs?: Record<string, Address>;
    tokenPrograms?: Record<string, Address>;
    rpc?: Rpc<GetAccountInfoApi>;
}): Promise<ResolvedInit> {
    const specs = input.init.args ?? [];
    const args = encodeArgs(specs, input.args ?? {});
    const context: SourceContext = {
        strategyProgram: input.strategyProgram,
        wallet: input.creator,
        inputs: input.inputs,
        args,
        tokenPrograms: input.tokenPrograms,
        rpc: input.rpc,
    };
    const stateSource = input.init.market_state ?? 'new_keypair';
    let marketState: Address;
    let marketStateSigns: boolean;
    if (stateSource === 'new_keypair') {
        marketState = input.marketState ?? fail('pass the new market state keypair address as marketState');
        marketStateSigns = true;
    } else {
        const source = parsed(stateSource);
        if (sourceNeedsMarket(source) || source.kind === 'market_state' || source.kind === 'market') {
            fail(`init.market_state "${stateSource}" can't be resolved before the market exists`);
        }
        marketState = await resolveAccountSource(source, context);
        marketStateSigns = false;
    }
    context.marketState = marketState;
    for (const account of input.init.accounts ?? []) {
        if (sourceNeedsMarket(parsed(account.source))) {
            fail(`init account "${account.name}" refers to the market's mints before the market exists`);
        }
    }
    return {
        marketState,
        marketStateSigns,
        market: await marketPda(input.strategyProgram, marketState),
        initData: concat(specs.map((spec) => args[spec.name]!)),
        strategyAccounts: await resolveMetadataAccounts(input.init.accounts ?? [], context),
    };
}

/** {@link resolveInit}, then the router's `create_market` instruction. */
export async function buildCreateMarketFromMetadata(
    input: Parameters<typeof resolveInit>[0] & { maxCreatorLamports: bigint },
): Promise<{ instruction: PipelineInstruction; resolved: ResolvedInit }> {
    const resolved = await resolveInit(input);
    const instruction = await createMarket({
        creator: input.creator,
        strategyProgram: input.strategyProgram,
        marketState: resolved.marketState,
        marketStateSigns: resolved.marketStateSigns,
        initData: resolved.initData,
        maxCreatorLamports: input.maxCreatorLamports,
        strategyAccounts: resolved.strategyAccounts,
    });
    return { instruction, resolved };
}
