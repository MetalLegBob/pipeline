import { address } from '@solana/kit';
import { describe, expect, it } from 'vitest';

import {
    buildQuoteInstruction,
    buildResolveAccountsInstruction,
    createMarket,
    decodeMarket,
    feeSideByDefault,
    feeSideOf,
    initFeeAccount,
    initializeConfig,
    createMarketLookupTable,
    lookupTableAddresses,
    marketViewFeeSide,
    marketViewFromMarket,
    NATIVE_MINT_ADDRESS,
    recordLookupTable,
    registerExistingMarket,
    registerStrategy,
    route,
    swap,
    swapFeeAccount,
    sweepFees,
    updateConfig,
    updateMetadata,
    type Market,
    type MarketView,
    type PipelineInstruction,
    type SwapAccounts,
    type SwapInstructionArgs,
    type TradeRequest,
} from '../src/index.js';
import {
    expected,
    fixtures,
    instructionJson,
    optionalAddress,
    optionalBigint,
    key,
    toMetas,
    unhex,
    type FixtureInstruction,
} from './helpers.js';

function swapAccounts(a: any): SwapAccounts {
    return {
        trader: address(a.trader),
        strategyProgram: address(a.strategyProgram),
        marketState: address(a.marketState),
        traderInput: address(a.traderInput),
        traderOutput: address(a.traderOutput),
        inputVault: address(a.inputVault),
        inputMint: address(a.inputMint),
        outputMint: address(a.outputMint),
        inputTokenProgram: address(a.inputTokenProgram),
        outputTokenProgram: address(a.outputTokenProgram),
        integratorFeeAccount: optionalAddress(a.integratorFeeAccount),
        strategyAccounts: toMetas(a.strategyAccounts),
        inputHookAccounts: toMetas(a.inputHookAccounts),
    };
}

function swapArgs(args: any): SwapInstructionArgs {
    return {
        mode: args.mode,
        amountIn: BigInt(args.amountIn),
        amountOut: BigInt(args.amountOut),
        minAmountOut: BigInt(args.minAmountOut),
        integratorFeeBps: args.integratorFeeBps,
        // Deliberately wrong: the builder must overwrite it from the accounts.
        inputHookAccounts: args.inputHookAccounts,
        extra: unhex(args.extra),
    };
}

/** Calls the SDK builder for a fixture, converting its JSON inputs. */
function build(fixture: FixtureInstruction): Promise<PipelineInstruction> {
    const i = fixture.inputs;
    switch (fixture.name) {
        case 'initializeConfig':
            return initializeConfig({
                admin: address(i.admin),
                args: {
                    listingFeeLamports: BigInt(i.args.listingFeeLamports),
                    listingFeeRecipient: address(i.args.listingFeeRecipient),
                    treasuryOwner: address(i.args.treasuryOwner),
                },
            });
        case 'updateConfig':
            return updateConfig({
                admin: address(i.admin),
                args: {
                    admin: optionalAddress(i.args.admin),
                    listingFeeLamports: optionalBigint(i.args.listingFeeLamports),
                    listingFeeRecipient: optionalAddress(i.args.listingFeeRecipient),
                    treasuryOwner: optionalAddress(i.args.treasuryOwner),
                },
            });
        case 'registerStrategy':
            return registerStrategy({
                payer: address(i.payer),
                registrar: address(i.registrar),
                strategyProgram: address(i.strategyProgram),
                listingFeeRecipient: address(i.listingFeeRecipient),
                metadataUri: i.metadataUri,
                maxListingFeeLamports: BigInt(i.maxListingFeeLamports),
            });
        case 'updateMetadata':
            return updateMetadata({
                registrar: address(i.registrar),
                strategyProgram: address(i.strategyProgram),
                metadataUri: i.metadataUri,
            });
        case 'createMarket':
            return createMarket({
                creator: address(i.creator),
                strategyProgram: address(i.strategyProgram),
                marketState: address(i.marketState),
                marketStateSigns: i.marketStateSigns,
                initData: unhex(i.initData),
                maxCreatorLamports: BigInt(i.maxCreatorLamports),
                strategyAccounts: toMetas(i.strategyAccounts),
            });
        case 'registerExistingMarket':
            return registerExistingMarket({
                payer: address(i.payer),
                strategyProgram: address(i.strategyProgram),
                marketState: address(i.marketState),
                infoAccounts: toMetas(i.infoAccounts),
            });
        case 'initFeeAccount':
            return initFeeAccount({
                payer: address(i.payer),
                market: address(i.market),
                mint: address(i.mint),
                tokenProgram: address(i.tokenProgram),
            });
        case 'swap':
            return i.feeSide === null
                ? swap(swapAccounts(i.accounts), swapArgs(i.args))
                : swap(swapAccounts(i.accounts), swapArgs(i.args), i.feeSide);
        case 'route':
            return route({
                trader: address(i.trader),
                integratorFeeAccount: optionalAddress(i.integratorFeeAccount),
                amountIn: BigInt(i.amountIn),
                minAmountOut: BigInt(i.minAmountOut),
                integratorFeeBps: i.integratorFeeBps,
                hops: i.hops.map((hop: any) => ({
                    accounts: swapAccounts(hop.accounts),
                    feeSide: hop.feeSide,
                    extra: unhex(hop.extra),
                })),
            });
        case 'recordLookupTable':
            return recordLookupTable({
                payer: address(i.payer),
                market: address(i.market),
                table: address(i.table),
                mints: i.mints.map(address),
            });
        case 'sweepFees':
            return sweepFees({
                market: address(i.market),
                mint: address(i.mint),
                tokenProgram: address(i.tokenProgram),
                treasuryOwner: address(i.treasuryOwner),
                hookAccounts: toMetas(i.hookAccounts),
            });
        default:
            throw new Error(`no builder for ${fixture.name}`);
    }
}

describe('router instruction builders match Rust', () => {
    it('cover every builder', () => {
        expect(new Set(fixtures.instructions.map((f) => f.name)).size).toBe(11);
    });

    for (const [n, fixture] of fixtures.instructions.entries()) {
        it(`${fixture.name} #${n}`, async () => {
            expect(instructionJson(await build(fixture))).toEqual(expected(fixture));
        });
    }

    it('swap sets inputHookAccounts from the accounts, whatever the args say', async () => {
        const fixture = fixtures.instructions.find(
            (f) => f.name === 'swap' && f.inputs.accounts.inputHookAccounts.length > 0,
        )!;
        const instruction = await build(fixture);
        // Data: discriminator (8), mode (1), three u64s (24), u16 (2), then the hook count.
        expect(instruction.data[8 + 1 + 24 + 2]).toBe(fixture.inputs.accounts.inputHookAccounts.length);
    });

    it('swap pays the fee in the output by default only when the output is wrapped SOL', async () => {
        const solOut = fixtures.instructions.find(
            (f) =>
                f.name === 'swap' && f.inputs.feeSide === null && f.inputs.accounts.outputMint === NATIVE_MINT_ADDRESS,
        )!;
        const accounts = swapAccounts(solOut.inputs.accounts);
        const feeAccount = (await swap(accounts, swapArgs(solOut.inputs.args))).accounts[12]!.address;
        expect(feeAccount).toBe(await swapFeeAccount(accounts, 'output'));
        expect(feeSideByDefault(NATIVE_MINT_ADDRESS)).toBe('output');
        expect(feeSideByDefault(accounts.inputMint)).toBe('input');
    });

    it('route rejects zero or more than four hops', async () => {
        const fixture = fixtures.instructions.find((f) => f.name === 'route' && f.inputs.hops.length === 4)!;
        const hop = { accounts: swapAccounts(fixture.inputs.hops[0].accounts) };
        const base = { trader: key(30), amountIn: 1n, minAmountOut: 1n, integratorFeeBps: 0 };
        await expect(route({ ...base, hops: [] })).rejects.toThrow(RangeError);
        await expect(route({ ...base, hops: [hop, hop, hop, hop, hop] })).rejects.toThrow(RangeError);
    });
});

describe('fee side', () => {
    const market = decodeMarket(unhex(fixtures.accounts.find((e) => e.type === 'Market')!.hex)) as Market;

    it('is the output only for trades into the market fee mint', () => {
        expect(feeSideOf(market, key(14))).toBe('output');
        expect(feeSideOf(market, key(13))).toBe('input');
        expect(feeSideOf({ ...market, feeMintIndex: 255 }, key(14))).toBe('input');
    });

    it('carries through a MarketView', () => {
        const view = marketViewFromMarket(key(40), market, [key(2), key(2), key(2)]);
        expect(view.feeMint).toBe(key(14));
        expect(marketViewFeeSide(view, key(14))).toBe('output');
        expect(marketViewFeeSide(view, key(15))).toBe('input');
        expect(marketViewFeeSide({ ...view, feeMint: undefined }, key(14))).toBe('input');
    });
});

describe('lookupTableAddresses matches Rust', () => {
    for (const [n, fixture] of fixtures.lookupTables.entries()) {
        it(`#${n}`, async () => {
            const market = decodeMarket(unhex(fixture.market)) as Market;
            const addresses = await lookupTableAddresses(
                address(fixture.marketAddress),
                market,
                fixture.tokenPrograms.map(address),
            );
            expect(addresses).toEqual(fixture.addresses);
        });
    }
});

function marketView(json: any): MarketView {
    return {
        market: address(json.market),
        strategyProgram: address(json.strategyProgram),
        marketState: address(json.marketState),
        mints: json.mints.map(address),
        vaults: json.vaults.map(address),
        tokenPrograms: json.tokenPrograms.map(address),
        supportsExactOut: json.supportsExactOut,
    };
}

function trade(json: any): TradeRequest {
    return {
        mode: json.mode,
        inputIndex: json.inputIndex,
        outputIndex: json.outputIndex,
        amount: BigInt(json.amount),
        trader: optionalAddress(json.trader),
        extra: unhex(json.extra),
    };
}

describe('strategy read instructions match pipeline-quoter', () => {
    for (const [n, fixture] of fixtures.strategyInstructions.entries()) {
        it(`${fixture.name} #${n}`, async () => {
            const i = fixture.inputs;
            const instruction =
                fixture.name === 'quote'
                    ? await buildQuoteInstruction(marketView(i.market), trade(i.trade), toMetas(i.strategyAccounts))
                    : await buildResolveAccountsInstruction(
                          marketView(i.market),
                          trade(i.trade),
                          i.round,
                          toMetas(i.known),
                      );
            expect(instructionJson(instruction)).toEqual(expected(fixture));
        });
    }

    it('reject trades whose indices are outside the market', async () => {
        const fixture = fixtures.strategyInstructions[0]!;
        const bad = { ...trade(fixture.inputs.trade), inputIndex: 3 };
        await expect(buildQuoteInstruction(marketView(fixture.inputs.market), bad, [])).rejects.toThrow(RangeError);
    });
});

describe('createMarketLookupTable matches Rust', () => {
    for (const [n, fixture] of fixtures.marketLookupTables.entries()) {
        it(`#${n}`, async () => {
            const { table, transactions } = await createMarketLookupTable({
                payer: address(fixture.payer),
                market: address(fixture.market),
                mints: fixture.mints.map(address),
                addresses: fixture.addresses.map(address),
                recentSlot: BigInt(fixture.recentSlot),
            });
            expect(table).toBe(fixture.table);
            expect(transactions.map((t) => t.map(instructionJson))).toEqual(
                fixture.transactions.map((t) => t.map(expected)),
            );
        });
    }
});
