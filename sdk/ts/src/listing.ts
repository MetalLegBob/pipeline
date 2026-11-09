// Instruction sequences that make a market visible and sellable through Pipeline.
//
// A market trades through the router only once the router has recorded it, either by
// `create_market` (which calls the strategy's `pipeline_init_market`) or, for a pool the
// strategy created with its own instruction, by `register_existing_market`. Selling a
// mint also needs the market's fee account for that mint, created by `init_fee_account`.
//
// A strategy can't record its own pool by calling `register_existing_market` through a
// CPI: the router calls back into the strategy's `pipeline_market_info`, and Solana
// forbids a program re-entering itself through another (A → B → A). Put the register
// instruction in the same transaction, after the strategy's own pool-creating
// instruction.

import type { AccountMeta, Address, ReadonlyUint8Array } from '@solana/kit';

import { createMarket, initFeeAccount, registerExistingMarket, type PipelineInstruction } from './instructions.js';
import { marketPda } from './pda.js';

/** A market mint and the token program that owns it. */
export type ListingMint = {
    mint: Address;
    tokenProgram: Address;
};

async function feeAccounts(payer: Address, market: Address, mints: readonly ListingMint[]) {
    const instructions: PipelineInstruction[] = [];
    for (const { mint, tokenProgram } of mints) {
        instructions.push(await initFeeAccount({ payer, market, mint, tokenProgram }));
    }
    return instructions;
}

/**
 * `[register_existing_market, init_fee_account × mints]` for a pool the strategy has
 * already created (or creates earlier in the same transaction). `mints` should be every
 * mint the market trades, in any order; `infoAccounts` are passed to the strategy's
 * `pipeline_market_info`.
 */
export async function registerAndListMarketInstructions(input: {
    payer: Address;
    strategyProgram: Address;
    marketState: Address;
    mints: readonly ListingMint[];
    infoAccounts?: readonly AccountMeta[];
}): Promise<PipelineInstruction[]> {
    const market = await marketPda(input.strategyProgram, input.marketState);
    return [
        await registerExistingMarket({
            payer: input.payer,
            strategyProgram: input.strategyProgram,
            marketState: input.marketState,
            infoAccounts: input.infoAccounts,
        }),
        ...(await feeAccounts(input.payer, market, input.mints)),
    ];
}

/**
 * `[create_market, init_fee_account × mints]`: creates a market through the router and
 * makes every mint sellable. The creator pays for the fee accounts.
 */
export async function createAndListMarketInstructions(input: {
    creator: Address;
    strategyProgram: Address;
    marketState: Address;
    marketStateSigns?: boolean;
    initData: ReadonlyUint8Array;
    maxCreatorLamports: bigint;
    strategyAccounts?: readonly AccountMeta[];
    mints: readonly ListingMint[];
}): Promise<PipelineInstruction[]> {
    const market = await marketPda(input.strategyProgram, input.marketState);
    const { mints, ...create } = input;
    return [await createMarket(create), ...(await feeAccounts(input.creator, market, mints))];
}
