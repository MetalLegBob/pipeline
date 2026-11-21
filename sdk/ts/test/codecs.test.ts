import type { Codec } from '@solana/kit';
import { describe, expect, it } from 'vitest';

import {
    decodeConfig,
    decodeExact,
    decodeMarket,
    decodeMarketInfo,
    decodeMarketInfoReturnData,
    decodeMarketLookupTable,
    decodeReturnData,
    decodeStrategy,
    encodeAccount,
    encodeMarketInfo,
    getAccountRefCodec,
    getConfigArgsCodec,
    getConfigCodec,
    getMarketCodec,
    getMarketInfoCodec,
    getMarketLookupTableCodec,
    getQuoteResultCodec,
    getResolveRequestCodec,
    getResolveResultCodec,
    getRouteArgsCodec,
    getRouteResultCodec,
    getStrategyCodec,
    getSwapArgsCodec,
    getSwapRequestCodec,
    getSwapResultCodec,
    getTradeRequestCodec,
    getUpdateConfigArgsCodec,
    instructionData,
    marketFeeMint,
    marketMints,
    NO_FEE_MINT,
    marketVaultFor,
    parseInstruction,
    STRATEGY_DISCRIMINATORS,
    TradeMode,
    type Market,
} from '../src/index.js';
import { fixtures, hex, key, toJson, unhex } from './helpers.js';

const CODECS: Record<string, () => Codec<any>> = {
    TradeRequest: getTradeRequestCodec,
    SwapRequest: getSwapRequestCodec,
    QuoteResult: getQuoteResultCodec,
    MarketInfo: getMarketInfoCodec,
    ResolveRequest: getResolveRequestCodec,
    AccountRef: getAccountRefCodec,
    ResolveResult: getResolveResultCodec,
    ConfigArgs: getConfigArgsCodec,
    UpdateConfigArgs: getUpdateConfigArgsCodec,
    SwapArgs: getSwapArgsCodec,
    SwapResult: getSwapResultCodec,
    RouteArgs: getRouteArgsCodec,
    RouteResult: getRouteResultCodec,
};

describe('wire types match Rust Borsh', () => {
    it('cover every type', () => {
        expect(new Set(fixtures.encodings.map((e) => e.type))).toEqual(new Set(Object.keys(CODECS)));
    });

    for (const [i, fixture] of fixtures.encodings.entries()) {
        it(`${fixture.type} #${i} decodes to the Rust value and re-encodes to the Rust bytes`, () => {
            const codec = CODECS[fixture.type]!();
            const decoded = decodeExact(codec, unhex(fixture.hex));
            expect(toJson(decoded)).toEqual(fixture.value);
            expect(hex(codec.encode(decoded))).toBe(fixture.hex);
        });
    }

    it('encode a hand-built value to the Rust bytes', () => {
        const fixture = fixtures.encodings.find((e) => e.type === 'TradeRequest' && e.value.trader !== null)!;
        const bytes = getTradeRequestCodec().encode({
            mode: TradeMode.ExactOut,
            inputIndex: 3,
            outputIndex: 2,
            amount: 2n ** 64n - 1n,
            trader: key(30),
            extra: new Uint8Array([0, 1, 2, 254, 255]),
        });
        expect(hex(bytes)).toBe(fixture.hex);
    });
});

describe('strict Borsh decoding', () => {
    const quote = unhex(fixtures.encodings.find((e) => e.type === 'QuoteResult')!.hex);
    const resolve = unhex(fixtures.encodings.find((e) => e.type === 'ResolveResult')!.hex);
    const update = unhex(fixtures.encodings.find((e) => e.type === 'UpdateConfigArgs')!.hex);

    it('rejects trailing and missing bytes', () => {
        expect(decodeExact(getQuoteResultCodec(), new Uint8Array([...quote, 0]))).toBeNull();
        expect(decodeExact(getQuoteResultCodec(), quote.subarray(1))).toBeNull();
    });

    it('rejects bools other than 0 and 1', () => {
        const bad = Uint8Array.from(resolve);
        bad[bad.length - 1] = 2;
        expect(decodeExact(getResolveResultCodec(), bad)).toBeNull();
    });

    it('rejects option tags other than 0 and 1', () => {
        const bad = Uint8Array.from(update);
        bad[0] = 2;
        expect(decodeExact(getUpdateConfigArgsCodec(), bad)).toBeNull();
    });

    it('rejects unknown trade modes', () => {
        expect(
            decodeExact(getTradeRequestCodec(), new Uint8Array([2, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])),
        ).toBeNull();
    });

    it('rejects invalid UTF-8 in strings', () => {
        const strategy = fixtures.accounts.find((e) => e.type === 'Strategy')!;
        const bad = unhex(strategy.hex);
        // The URI starts after the discriminator, program id and u32 length.
        bad[8 + 32 + 4] = 0xff;
        expect(decodeStrategy(bad)).toBeNull();
    });
});

describe('strategy instruction data', () => {
    it('round-trips and rejects the wrong discriminator', () => {
        const request = {
            trade: {
                mode: TradeMode.ExactOut,
                inputIndex: 1,
                outputIndex: 0,
                amount: 42n,
                trader: key(7),
                extra: new Uint8Array([1, 2, 3]),
            },
            amountSent: 100n,
            minAmountOut: 40n,
        };
        const data = instructionData(STRATEGY_DISCRIMINATORS.swap, getSwapRequestCodec(), request);
        expect(toJson(parseInstruction(STRATEGY_DISCRIMINATORS.swap, getSwapRequestCodec(), data))).toEqual(
            toJson(request),
        );
        expect(parseInstruction(STRATEGY_DISCRIMINATORS.quote, getSwapRequestCodec(), data)).toBeNull();
    });
});

describe('return data', () => {
    it('restores trailing zero bytes stripped by the runtime', () => {
        // { amountIn: 5, amountOut: 0 } is recorded as the single byte 0x05.
        expect(decodeReturnData(getQuoteResultCodec(), new Uint8Array([5]))).toEqual({ amountIn: 5n, amountOut: 0n });
        // { accounts: [], complete: false } is all zeros and recorded as nothing.
        expect(decodeReturnData(getResolveResultCodec(), new Uint8Array())).toEqual({ accounts: [], complete: false });
    });

    it('decodes untruncated data unchanged', () => {
        const fixture = fixtures.encodings.find((e) => e.type === 'ResolveResult')!;
        expect(toJson(decodeReturnData(getResolveResultCodec(), unhex(fixture.hex)))).toEqual(fixture.value);
    });
});

describe('router accounts', () => {
    const decoders: Record<string, (data: Uint8Array) => unknown> = {
        Config: decodeConfig,
        Strategy: decodeStrategy,
        Market: decodeMarket,
        MarketLookupTable: decodeMarketLookupTable,
    };
    const codecs: Record<string, () => Codec<any>> = {
        Config: getConfigCodec,
        Strategy: getStrategyCodec,
        Market: getMarketCodec,
        MarketLookupTable: getMarketLookupTableCodec,
    };

    for (const fixture of fixtures.accounts) {
        const name = fixture.type as 'Config' | 'Strategy' | 'Market' | 'MarketLookupTable';

        it(`${name} decodes to the Rust value and re-encodes to the Rust bytes`, () => {
            const decoded = decoders[name]!(unhex(fixture.hex));
            expect(toJson(decoded)).toEqual(fixture.value);
            expect(hex(encodeAccount(name, codecs[name]!(), decoded))).toBe(fixture.hex);
        });

        it(`${name} ignores Anchor's trailing allocation`, () => {
            const padded = new Uint8Array([...unhex(fixture.hex), ...new Uint8Array(64)]);
            expect(toJson(decoders[name]!(padded))).toEqual(fixture.value);
        });

        it(`${name} rejects another account's discriminator`, () => {
            const bytes = unhex(fixture.hex);
            bytes[0]! ^= 0xff;
            expect(decoders[name]!(bytes)).toBeNull();
            expect(decoders[name]!(new Uint8Array())).toBeNull();
        });
    }

    it('market helpers use only the first mintCount mints', () => {
        const market = decodeMarket(unhex(fixtures.accounts.find((e) => e.type === 'Market')!.hex)) as Market;
        expect(marketMints(market)).toEqual([key(13), key(14), key(15)]);
        expect(marketVaultFor(market, key(14))).toBe(key(17));
        expect(marketVaultFor(market, key(0))).toBeNull();
    });

    it('market fee mint is the mint at feeMintIndex, if any', () => {
        const market = decodeMarket(unhex(fixtures.accounts.find((e) => e.type === 'Market')!.hex)) as Market;
        expect(marketFeeMint(market)).toBe(key(14));
        expect(marketFeeMint({ ...market, feeMintIndex: NO_FEE_MINT })).toBeNull();
        // An index past the market's mints declares nothing.
        expect(marketFeeMint({ ...market, feeMintIndex: 3 })).toBeNull();
    });

    it('a Market in the layout before fee mints does not decode', () => {
        const bytes = unhex(fixtures.accounts.find((e) => e.type === 'Market')!.hex);
        expect(decodeMarket(bytes.subarray(0, bytes.length - 1))).toBeNull();
    });
});

describe('market info with an optional fee mint matches Rust', () => {
    for (const [i, fixture] of fixtures.marketInfos.entries()) {
        it(`#${i} (fee mint ${fixture.feeMint}) decodes and re-encodes`, () => {
            const decoded = decodeMarketInfo(unhex(fixture.hex))!;
            expect(toJson(decoded.info)).toEqual(fixture.info);
            expect(decoded.feeMint).toBe(fixture.feeMint);
            expect(hex(encodeMarketInfo(decoded.info, decoded.feeMint))).toBe(fixture.hex);
        });
    }

    const bare = unhex(fixtures.marketInfos.find((f) => f.feeMint === null)!.hex);

    it('accepts an explicit None and rejects anything else after the info', () => {
        expect(decodeMarketInfo(new Uint8Array([...bare, 0]))?.feeMint).toBeNull();
        expect(decodeMarketInfo(new Uint8Array([...bare, 0, 0]))).toBeNull();
        expect(decodeMarketInfo(new Uint8Array([...bare, 1]))).toBeNull();
        expect(decodeMarketInfo(new Uint8Array([...bare, 1, 0, 0]))).toBeNull();
        expect(decodeMarketInfo(new Uint8Array([...bare, 2, 0]))).toBeNull();
        expect(decodeMarketInfo(bare.subarray(1))).toBeNull();
    });

    it('reads RPC return data with its trailing zeros dropped', () => {
        const trim = (bytes: Uint8Array) => {
            let end = bytes.length;
            while (end > 0 && bytes[end - 1] === 0) end--;
            return bytes.subarray(0, end);
        };
        for (const fixture of fixtures.marketInfos) {
            const decoded = decodeMarketInfoReturnData(trim(unhex(fixture.hex)))!;
            expect(toJson(decoded.info)).toEqual(fixture.info);
            expect(decoded.feeMint).toBe(fixture.feeMint);
        }
        expect(decodeMarketInfoReturnData(new Uint8Array([...bare, 2]))).toBeNull();
    });
});
