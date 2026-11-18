// The strategy template's metadata.json must stay valid and must build the same
// `create_market` instruction the template program and its tests use.

import { readFileSync } from 'node:fs';

import { AccountRole, getAddressEncoder, getProgramDerivedAddress } from '@solana/kit';
import { describe, expect, it } from 'vitest';

import {
    ASSOCIATED_TOKEN_PROGRAM_ADDRESS,
    associatedTokenAddress,
    buildCreateMarketFromMetadata,
    createMarket,
    marketPda,
    parseStrategyMetadata,
    TOKEN_2022_PROGRAM_ADDRESS,
    TOKEN_PROGRAM_ADDRESS,
    type StrategyMetadata,
} from '../src/index.js';
import { key } from './helpers.js';

const TEMPLATE_METADATA = new URL('../../../templates/strategy/metadata.json', import.meta.url);

const strategyProgram = key(7);
const wallet = key(30);
const mintA = key(13);
const mintB = key(14);
const tokenPrograms = { [mintA]: TOKEN_PROGRAM_ADDRESS, [mintB]: TOKEN_2022_PROGRAM_ADDRESS };

function templateMetadata(): StrategyMetadata {
    const result = parseStrategyMetadata(JSON.parse(readFileSync(TEMPLATE_METADATA, 'utf8')));
    if (!result.ok) {
        throw new Error(`template metadata is invalid: ${result.errors.join('; ')}`);
    }
    return result.value;
}

/** The template's pool address: `["pool", mint_a, mint_b, fee_bps as u16 LE]`. */
async function pool(feeBps: number) {
    const encoder = getAddressEncoder();
    const [address] = await getProgramDerivedAddress({
        programAddress: strategyProgram,
        seeds: ['pool', encoder.encode(mintA), encoder.encode(mintB), new Uint8Array([feeBps & 0xff, feeBps >> 8])],
    });
    return address;
}

describe('strategy template metadata.json', () => {
    it('is a valid version 1 document with no warnings', () => {
        const result = parseStrategyMetadata(JSON.parse(readFileSync(TEMPLATE_METADATA, 'utf8')));
        expect(result).toMatchObject({ ok: true, warnings: [] });
    });

    it('builds the create_market the template program expects', async () => {
        const metadata = templateMetadata();
        const { instruction, resolved } = await buildCreateMarketFromMetadata({
            init: metadata.init!,
            strategyProgram,
            creator: wallet,
            args: { fee_bps: 30 },
            inputs: { mint_a: mintA, mint_b: mintB },
            tokenPrograms,
            maxCreatorLamports: 10_000_000n,
        });
        const marketState = await pool(30);
        // InitMarket's accounts after the router's four: mints, vaults (the pool's
        // associated token accounts), token programs, associated token program.
        const strategyAccounts = [
            { address: mintA, role: AccountRole.READONLY },
            { address: mintB, role: AccountRole.READONLY },
            {
                address: await associatedTokenAddress(marketState, mintA, TOKEN_PROGRAM_ADDRESS),
                role: AccountRole.WRITABLE,
            },
            {
                address: await associatedTokenAddress(marketState, mintB, TOKEN_2022_PROGRAM_ADDRESS),
                role: AccountRole.WRITABLE,
            },
            { address: TOKEN_PROGRAM_ADDRESS, role: AccountRole.READONLY },
            { address: TOKEN_2022_PROGRAM_ADDRESS, role: AccountRole.READONLY },
            { address: ASSOCIATED_TOKEN_PROGRAM_ADDRESS, role: AccountRole.READONLY },
        ];
        expect(resolved).toEqual({
            marketState,
            marketStateSigns: false,
            market: await marketPda(strategyProgram, marketState),
            // InitParams { fee_bps: 30 }, Borsh-encoded.
            initData: new Uint8Array([30, 0]),
            strategyAccounts,
        });
        expect(instruction).toEqual(
            await createMarket({
                creator: wallet,
                strategyProgram,
                marketState,
                initData: new Uint8Array([30, 0]),
                maxCreatorLamports: 10_000_000n,
                strategyAccounts,
            }),
        );
    });
});
