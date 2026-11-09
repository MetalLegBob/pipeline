// Listing sequences are the existing builders in the right order, for the right market.

import { AccountRole } from '@solana/kit';
import { describe, expect, it } from 'vitest';

import {
    createAndListMarketInstructions,
    createMarket,
    initFeeAccount,
    marketPda,
    registerAndListMarketInstructions,
    registerExistingMarket,
    TOKEN_2022_PROGRAM_ADDRESS,
    TOKEN_PROGRAM_ADDRESS,
} from '../src/index.js';
import { key } from './helpers.js';

const strategyProgram = key(7);
const marketState = key(8);
const mints = [
    { mint: key(13), tokenProgram: TOKEN_PROGRAM_ADDRESS },
    { mint: key(14), tokenProgram: TOKEN_2022_PROGRAM_ADDRESS },
];

describe('listing', () => {
    it('registerAndListMarketInstructions: register_existing_market, then a fee account per mint', async () => {
        const infoAccounts = [{ address: key(20), role: AccountRole.READONLY }];
        const instructions = await registerAndListMarketInstructions({
            payer: key(30),
            strategyProgram,
            marketState,
            mints,
            infoAccounts,
        });
        const market = await marketPda(strategyProgram, marketState);
        expect(instructions).toEqual([
            await registerExistingMarket({ payer: key(30), strategyProgram, marketState, infoAccounts }),
            await initFeeAccount({ payer: key(30), market, ...mints[0]! }),
            await initFeeAccount({ payer: key(30), market, ...mints[1]! }),
        ]);
    });

    it('createAndListMarketInstructions: create_market, then a fee account per mint paid by the creator', async () => {
        const create = {
            creator: key(30),
            strategyProgram,
            marketState,
            marketStateSigns: true,
            initData: new Uint8Array([30, 0]),
            maxCreatorLamports: 10_000_000n,
            strategyAccounts: [{ address: key(21), role: AccountRole.WRITABLE }],
        };
        const instructions = await createAndListMarketInstructions({ ...create, mints });
        const market = await marketPda(strategyProgram, marketState);
        expect(instructions).toEqual([
            await createMarket(create),
            await initFeeAccount({ payer: key(30), market, ...mints[0]! }),
            await initFeeAccount({ payer: key(30), market, ...mints[1]! }),
        ]);
    });
});
