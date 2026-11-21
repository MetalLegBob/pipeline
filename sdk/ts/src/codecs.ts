// Borsh codecs for every type the router and strategies exchange, built from
// `@solana/kit` codecs. Layouts mirror the Rust crates `pipeline-strategy-interface` and
// `pipeline-client` field for field; the cross-language fixtures in `test/` pin them.
//
// Conventions: `u64`/`i64` decode to `bigint`, `u8`/`u16` to `number`, `Option<T>` to
// `T | null`, `Vec<u8>` to a byte array, `Pubkey` to a base58 `Address`.

import {
    addCodecSizePrefix,
    createCodec,
    getAddressCodec,
    getArrayCodec,
    getBytesCodec,
    getEncodedSize,
    getI64Codec,
    getStructCodec,
    getU16Codec,
    getU32Codec,
    getU64Codec,
    getU8Codec,
    transformCodec,
    type Address,
    type Codec,
    type Decoder,
    type ReadonlyUint8Array,
} from '@solana/kit';

import { ACCOUNT_DISCRIMINATORS, MAX_MARKET_MINTS, MAX_RETURN_DATA, NO_FEE_MINT } from './constants.js';

// ---------------------------------------------------------------------------------------
// Borsh primitives
// ---------------------------------------------------------------------------------------

/** Borsh `bool`: one byte that must be 0 or 1. */
export function getBorshBoolCodec(): Codec<boolean> {
    return transformCodec(
        getU8Codec(),
        (value: boolean) => (value ? 1 : 0),
        (byte: number) => {
            if (byte > 1) {
                throw new Error(`invalid bool byte ${byte}`);
            }
            return byte === 1;
        },
    );
}

/** Borsh `Option<T>`: a 0 or 1 tag byte, then the value if present. `None` is `null`. */
export function getBorshOptionCodec<TFrom, TTo extends TFrom>(
    item: Codec<TFrom, TTo>,
): Codec<TFrom | null, TTo | null> {
    return createCodec<TFrom | null, TTo | null>({
        getSizeFromValue: (value) => (value === null ? 1 : 1 + getEncodedSize(value, item)),
        write: (value, bytes, offset) => {
            if (value === null) {
                bytes.set([0], offset);
                return offset + 1;
            }
            bytes.set([1], offset);
            return item.write(value, bytes, offset + 1);
        },
        read: (bytes, offset) => {
            const tag = bytes[offset];
            if (tag === 0) {
                return [null, offset + 1];
            }
            if (tag === 1) {
                return item.read(bytes, offset + 1);
            }
            throw new Error(`invalid option tag ${tag}`);
        },
    });
}

/** Borsh `Vec<u8>`: a u32 length, then the bytes. */
export function getBorshBytesCodec(): Codec<ReadonlyUint8Array> {
    return addCodecSizePrefix(getBytesCodec(), getU32Codec());
}

const utf8Encoder = new TextEncoder();
const utf8Decoder = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });

/** Borsh `String`: a u32 byte length, then UTF-8. Invalid UTF-8 fails, as in Rust. */
export function getBorshStringCodec(): Codec<string> {
    return transformCodec(
        getBorshBytesCodec(),
        (value: string) => utf8Encoder.encode(value),
        (bytes: ReadonlyUint8Array) => utf8Decoder.decode(bytes as Uint8Array),
    );
}

/** Borsh `Vec<T>`: a u32 element count, then the elements. */
export function getBorshVecCodec<T>(item: Codec<T>): Codec<T[]> {
    return getArrayCodec(item, { size: getU32Codec() });
}

/**
 * Decodes `bytes` completely, as Rust's `try_from_slice`: returns `null` if decoding fails
 * or leaves bytes unread.
 */
export function decodeExact<T>(decoder: Decoder<T>, bytes: ReadonlyUint8Array): T | null {
    try {
        const [value, end] = decoder.read(bytes, 0);
        return end === bytes.length ? value : null;
    } catch {
        return null;
    }
}

/** Decodes a value from the start of `bytes`, ignoring trailing bytes. `null` on failure. */
export function decodePrefix<T>(decoder: Decoder<T>, bytes: ReadonlyUint8Array): T | null {
    try {
        return decoder.read(bytes, 0)[0];
    } catch {
        return null;
    }
}

/**
 * Decodes program return data as reported by RPC. The runtime strips trailing zero bytes
 * from recorded return data, so the bytes are padded back with zeros before decoding;
 * a strategy's trailing `0` amounts or `false` flags survive the round trip.
 */
export function decodeReturnData<T>(decoder: Decoder<T>, bytes: ReadonlyUint8Array): T | null {
    if (bytes.length > MAX_RETURN_DATA) {
        return null;
    }
    const padded = new Uint8Array(MAX_RETURN_DATA);
    padded.set(bytes);
    return decodePrefix(decoder, padded);
}

/** Instruction data: an 8-byte discriminator followed by the Borsh-encoded argument. */
export function instructionData<T>(discriminator: ReadonlyUint8Array, codec: Codec<T>, arg: T): Uint8Array {
    const body = codec.encode(arg);
    const data = new Uint8Array(discriminator.length + body.length);
    data.set(discriminator);
    data.set(body, discriminator.length);
    return data;
}

/** Decodes instruction data written by {@link instructionData}. `null` on any mismatch. */
export function parseInstruction<T>(
    discriminator: ReadonlyUint8Array,
    decoder: Decoder<T>,
    data: ReadonlyUint8Array,
): T | null {
    if (!startsWith(data, discriminator)) {
        return null;
    }
    return decodeExact(decoder, data.subarray(discriminator.length));
}

export function startsWith(data: ReadonlyUint8Array, prefix: ReadonlyUint8Array): boolean {
    if (data.length < prefix.length) {
        return false;
    }
    for (let i = 0; i < prefix.length; i++) {
        if (data[i] !== prefix[i]) {
            return false;
        }
    }
    return true;
}

const u8 = getU8Codec;
const u16 = getU16Codec;
const u64 = getU64Codec;
const i64 = getI64Codec;
const bool = getBorshBoolCodec;
const pubkey = getAddressCodec;
const bytes = getBorshBytesCodec;
const string = getBorshStringCodec;

// ---------------------------------------------------------------------------------------
// Strategy interface
// ---------------------------------------------------------------------------------------

export const TradeMode = {
    /** `amount` is the input the strategy receives; it reports the output. */
    ExactIn: 0,
    /** `amount` is the output the strategy must send; it reports the input it needs. */
    ExactOut: 1,
} as const;
export type TradeMode = (typeof TradeMode)[keyof typeof TradeMode];

/** Borsh unit enum: the variant index as one byte. */
export function getTradeModeCodec(): Codec<TradeMode> {
    return transformCodec(
        getU8Codec(),
        (mode: TradeMode) => mode,
        (byte: number) => {
            if (byte !== TradeMode.ExactIn && byte !== TradeMode.ExactOut) {
                throw new Error(`invalid trade mode ${byte}`);
            }
            return byte;
        },
    );
}

/**
 * What a trader wants, as seen by the strategy. Amounts are measured at the strategy's
 * own vaults: input is what arrives in the input vault (after any token transfer fee),
 * output is what leaves the output vault (before any token transfer fee).
 */
export type TradeRequest = {
    mode: TradeMode;
    /** Index into the market's mints. */
    inputIndex: number;
    outputIndex: number;
    amount: bigint;
    /** `null` for anonymous quotes, such as an aggregator's. */
    trader: Address | null;
    /** Strategy-specific data, passed through from the client unchanged. */
    extra: ReadonlyUint8Array;
};

export function getTradeRequestCodec(): Codec<TradeRequest> {
    return getStructCodec([
        ['mode', getTradeModeCodec()],
        ['inputIndex', u8()],
        ['outputIndex', u8()],
        ['amount', u64()],
        ['trader', getBorshOptionCodec(pubkey())],
        ['extra', bytes()],
    ]);
}

/** Argument of `pipeline_swap`. */
export type SwapRequest = {
    trade: TradeRequest;
    /** Gross input the router transferred into the input vault just before the call. */
    amountSent: bigint;
    /** The trader's minimum output. The router enforces it independently. */
    minAmountOut: bigint;
};

export function getSwapRequestCodec(): Codec<SwapRequest> {
    return getStructCodec([
        ['trade', getTradeRequestCodec()],
        ['amountSent', u64()],
        ['minAmountOut', u64()],
    ]);
}

/** Return data of `pipeline_quote`, in the strategy's vault-level units. */
export type QuoteResult = {
    /** Input the strategy keeps. Exact-in may keep less than requested and refund the rest. */
    amountIn: bigint;
    /** Output the strategy sends. */
    amountOut: bigint;
};

export function getQuoteResultCodec(): Codec<QuoteResult> {
    return getStructCodec([
        ['amountIn', u64()],
        ['amountOut', u64()],
    ]);
}

/** Return data of `pipeline_market_info`. */
export type MarketInfo = {
    mints: Address[];
    /** The token account that receives input for each mint, in the same order as `mints`. */
    vaults: Address[];
    supportsExactOut: boolean;
};

export function getMarketInfoCodec(): Codec<MarketInfo> {
    return getStructCodec([
        ['mints', getBorshVecCodec(pubkey())],
        ['vaults', getBorshVecCodec(pubkey())],
        ['supportsExactOut', bool()],
    ]);
}

/**
 * `pipeline_market_info` return data: the {@link MarketInfo}, then, if the strategy
 * declares one, its fee mint as a Borsh `Option<u8>` index into `mints`. A bare
 * `MarketInfo` (a strategy that predates fee mints) declares none.
 */
export function encodeMarketInfo(info: MarketInfo, feeMint: number | null): Uint8Array {
    const body = getMarketInfoCodec().encode(info);
    if (feeMint === null) {
        return Uint8Array.from(body);
    }
    const data = new Uint8Array(body.length + 2);
    data.set(body);
    data.set([1, u8().encode(feeMint)[0]!], body.length);
    return data;
}

/**
 * Decodes `pipeline_market_info` return data written by {@link encodeMarketInfo}, as the
 * router does: a bare `MarketInfo`, or one followed by `Option<u8>` (an explicit `None`
 * is accepted). `null` if the data doesn't decode or has bytes left over.
 *
 * For return data reported by RPC, which drops trailing zero bytes, use
 * {@link decodeMarketInfoReturnData}.
 */
export function decodeMarketInfo(data: ReadonlyUint8Array): { info: MarketInfo; feeMint: number | null } | null {
    let info: MarketInfo;
    let end: number;
    try {
        [info, end] = getMarketInfoCodec().read(data, 0);
    } catch {
        return null;
    }
    const rest = data.subarray(end);
    if (rest.length === 0 || (rest.length === 1 && rest[0] === 0)) {
        return { info, feeMint: null };
    }
    if (rest.length === 2 && rest[0] === 1) {
        return { info, feeMint: rest[1]! };
    }
    return null;
}

/**
 * {@link decodeMarketInfo} for return data reported by RPC, where the runtime has dropped
 * trailing zero bytes: missing bytes are read as zeros, so `[1]` after the info is a fee
 * mint at index 0.
 */
export function decodeMarketInfoReturnData(
    data: ReadonlyUint8Array,
): { info: MarketInfo; feeMint: number | null } | null {
    if (data.length > MAX_RETURN_DATA) {
        return null;
    }
    const padded = new Uint8Array(MAX_RETURN_DATA);
    padded.set(data);
    let info: MarketInfo;
    let end: number;
    try {
        [info, end] = getMarketInfoCodec().read(padded, 0);
    } catch {
        return null;
    }
    // Whatever survives after the info, with its dropped trailing zeros put back: nothing
    // (no declaration), or `[1]` / `[1, i]` (`Some(0)` / `Some(i)`).
    const rest = data.subarray(Math.min(end, data.length));
    if (rest.length === 0) {
        return { info, feeMint: null };
    }
    if (rest[0] === 1 && rest.length <= 2) {
        return { info, feeMint: rest[1] ?? 0 };
    }
    return null;
}

/** Argument of `pipeline_resolve_accounts`. */
export type ResolveRequest = {
    trade: TradeRequest;
    /** Starts at 0. Later rounds pass the accounts returned so far after the fixed accounts. */
    round: number;
};

export function getResolveRequestCodec(): Codec<ResolveRequest> {
    return getStructCodec([
        ['trade', getTradeRequestCodec()],
        ['round', u8()],
    ]);
}

export type AccountRef = {
    address: Address;
    writable: boolean;
};

export function getAccountRefCodec(): Codec<AccountRef> {
    return getStructCodec([
        ['address', pubkey()],
        ['writable', bool()],
    ]);
}

/**
 * Return data of `pipeline_resolve_accounts`: the full list of strategy-specific accounts
 * known so far (not just this round's additions).
 */
export type ResolveResult = {
    accounts: AccountRef[];
    /** `false` if the strategy needs to read the returned accounts before it can list more. */
    complete: boolean;
};

export function getResolveResultCodec(): Codec<ResolveResult> {
    return getStructCodec([
        ['accounts', getBorshVecCodec(getAccountRefCodec())],
        ['complete', bool()],
    ]);
}

// ---------------------------------------------------------------------------------------
// Router instruction arguments and return data
// ---------------------------------------------------------------------------------------

export type ConfigArgs = {
    listingFeeLamports: bigint;
    listingFeeRecipient: Address;
    treasuryOwner: Address;
};

export function getConfigArgsCodec(): Codec<ConfigArgs> {
    return getStructCodec([
        ['listingFeeLamports', u64()],
        ['listingFeeRecipient', pubkey()],
        ['treasuryOwner', pubkey()],
    ]);
}

/** Each field left `null` keeps its current value. */
export type UpdateConfigArgs = {
    admin: Address | null;
    listingFeeLamports: bigint | null;
    listingFeeRecipient: Address | null;
    treasuryOwner: Address | null;
};

export function getUpdateConfigArgsCodec(): Codec<UpdateConfigArgs> {
    return getStructCodec([
        ['admin', getBorshOptionCodec(pubkey())],
        ['listingFeeLamports', getBorshOptionCodec(u64())],
        ['listingFeeRecipient', getBorshOptionCodec(pubkey())],
        ['treasuryOwner', getBorshOptionCodec(pubkey())],
    ]);
}

export type SwapArgs = {
    mode: TradeMode;
    /** Exact-in: amount sent to the strategy. Exact-out: the most that may be sent. */
    amountIn: bigint;
    /** Exact-out only: output the strategy must send, in strategy vault units. */
    amountOut: bigint;
    minAmountOut: bigint;
    integratorFeeBps: number;
    /** Number of transfer-hook accounts at the end of the account list. */
    inputHookAccounts: number;
    extra: ReadonlyUint8Array;
};

export function getSwapArgsCodec(): Codec<SwapArgs> {
    return getStructCodec([
        ['mode', getTradeModeCodec()],
        ['amountIn', u64()],
        ['amountOut', u64()],
        ['minAmountOut', u64()],
        ['integratorFeeBps', u16()],
        ['inputHookAccounts', u8()],
        ['extra', bytes()],
    ]);
}

/** Return data of the router's `swap`. */
export type SwapResult = {
    netSent: bigint;
    received: bigint;
    /** In `feeMint`: the input mint, or the output mint when the fee is taken there. */
    pipelineFee: bigint;
    integratorFee: bigint;
    /** The mint Pipeline's fee was taken in. */
    feeMint: Address;
};

export function getSwapResultCodec(): Codec<SwapResult> {
    return getStructCodec([
        ['netSent', u64()],
        ['received', u64()],
        ['pipelineFee', u64()],
        ['integratorFee', u64()],
        ['feeMint', pubkey()],
    ]);
}

/** One hop of a `route`: how many strategy and input-hook accounts follow its fixed accounts. */
export type RouteHop = {
    strategyAccounts: number;
    inputHookAccounts: number;
    extra: ReadonlyUint8Array;
};

export function getRouteHopCodec(): Codec<RouteHop> {
    return getStructCodec([
        ['strategyAccounts', u8()],
        ['inputHookAccounts', u8()],
        ['extra', bytes()],
    ]);
}

/** Arguments of the router's `route`. */
export type RouteArgs = {
    amountIn: bigint;
    minAmountOut: bigint;
    integratorFeeBps: number;
    hops: RouteHop[];
};

export function getRouteArgsCodec(): Codec<RouteArgs> {
    return getStructCodec([
        ['amountIn', u64()],
        ['minAmountOut', u64()],
        ['integratorFeeBps', u16()],
        ['hops', getBorshVecCodec(getRouteHopCodec())],
    ]);
}

/** Return data of the router's `route`. */
export type RouteResult = {
    netSent: bigint;
    /** What the trader keeps from the last hop. */
    amountOut: bigint;
    integratorFee: bigint;
};

export function getRouteResultCodec(): Codec<RouteResult> {
    return getStructCodec([
        ['netSent', u64()],
        ['amountOut', u64()],
        ['integratorFee', u64()],
    ]);
}

// ---------------------------------------------------------------------------------------
// Router accounts
// ---------------------------------------------------------------------------------------

export type Config = {
    admin: Address;
    /** Unused, always 0 (the place of a pause flag the router once had). */
    reserved: number;
    listingFeeLamports: bigint;
    listingFeeRecipient: Address;
    treasuryOwner: Address;
    bump: number;
};

export function getConfigCodec(): Codec<Config> {
    return getStructCodec([
        ['admin', pubkey()],
        ['reserved', u8()],
        ['listingFeeLamports', u64()],
        ['listingFeeRecipient', pubkey()],
        ['treasuryOwner', pubkey()],
        ['bump', u8()],
    ]);
}

export type Strategy = {
    programId: Address;
    metadataUri: string;
    interfaceVersion: number;
    registeredAt: bigint;
    marketCount: bigint;
    bump: number;
};

export function getStrategyCodec(): Codec<Strategy> {
    return getStructCodec([
        ['programId', pubkey()],
        ['metadataUri', string()],
        ['interfaceVersion', u8()],
        ['registeredAt', i64()],
        ['marketCount', u64()],
        ['bump', u8()],
    ]);
}

export type Market = {
    strategyProgram: Address;
    marketState: Address;
    mintCount: number;
    /** Always four entries; only the first `mintCount` are used. See {@link marketMints}. */
    mints: Address[];
    /** Input vault for each mint, in the same order as `mints`. */
    vaults: Address[];
    supportsExactOut: boolean;
    createdBy: Address;
    createdAt: bigint;
    bump: number;
    feeAuthorityBump: number;
    /** Index into `mints` of the market's fee mint, or {@link NO_FEE_MINT}. See {@link marketFeeMint}. */
    feeMintIndex: number;
};

export function getMarketCodec(): Codec<Market> {
    return getStructCodec([
        ['strategyProgram', pubkey()],
        ['marketState', pubkey()],
        ['mintCount', u8()],
        ['mints', getArrayCodec(pubkey(), { size: MAX_MARKET_MINTS })],
        ['vaults', getArrayCodec(pubkey(), { size: MAX_MARKET_MINTS })],
        ['supportsExactOut', bool()],
        ['createdBy', pubkey()],
        ['createdAt', i64()],
        ['bump', u8()],
        ['feeAuthorityBump', u8()],
        ['feeMintIndex', u8()],
    ]);
}

/** The mints a market trades. */
export function marketMints(market: Market): Address[] {
    return market.mints.slice(0, market.mintCount);
}

/**
 * The mint Pipeline takes its fee in when it is one side of a trade, or `null`
 * ({@link NO_FEE_MINT}, or an index past the market's mints).
 */
export function marketFeeMint(market: Market): Address | null {
    if (market.feeMintIndex === NO_FEE_MINT) {
        return null;
    }
    return marketMints(market)[market.feeMintIndex] ?? null;
}

/** The input vault for `mint`, or `null` if the market does not trade it. */
export function marketVaultFor(market: Market, mint: Address): Address | null {
    const index = marketMints(market).indexOf(mint);
    return index < 0 ? null : (market.vaults[index] ?? null);
}

/** A market's recorded address lookup table (PDA `["lookup", market]`). */
export type MarketLookupTable = {
    market: Address;
    table: Address;
    bump: number;
};

export function getMarketLookupTableCodec(): Codec<MarketLookupTable> {
    return getStructCodec([
        ['market', pubkey()],
        ['table', pubkey()],
        ['bump', u8()],
    ]);
}

export type RouterAccountName = keyof typeof ACCOUNT_DISCRIMINATORS;

/**
 * Decodes a router account, checking its 8-byte Anchor discriminator. Trailing bytes are
 * ignored, because Anchor allocates accounts at their maximum size.
 */
export function decodeAccount<T>(name: RouterAccountName, decoder: Decoder<T>, data: ReadonlyUint8Array): T | null {
    const discriminator = ACCOUNT_DISCRIMINATORS[name];
    if (!startsWith(data, discriminator)) {
        return null;
    }
    return decodePrefix(decoder, data.subarray(discriminator.length));
}

/** Encodes a router account as stored on chain: discriminator, then Borsh. */
export function encodeAccount<T>(name: RouterAccountName, codec: Codec<T>, value: T): Uint8Array {
    return instructionData(ACCOUNT_DISCRIMINATORS[name], codec, value);
}

export function decodeConfig(data: ReadonlyUint8Array): Config | null {
    return decodeAccount('Config', getConfigCodec(), data);
}

export function decodeStrategy(data: ReadonlyUint8Array): Strategy | null {
    return decodeAccount('Strategy', getStrategyCodec(), data);
}

export function decodeMarket(data: ReadonlyUint8Array): Market | null {
    return decodeAccount('Market', getMarketCodec(), data);
}

export function decodeMarketLookupTable(data: ReadonlyUint8Array): MarketLookupTable | null {
    return decodeAccount('MarketLookupTable', getMarketLookupTableCodec(), data);
}
