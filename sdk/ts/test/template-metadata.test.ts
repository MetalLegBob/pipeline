// The strategy template's metadata.json must stay valid and must build the same
// `create_market` and `deposit` instructions the template program and its tests use.

import { readFileSync } from 'node:fs';

import { AccountRole, getAddressEncoder, getProgramDerivedAddress } from '@solana/kit';
import { describe, expect, it } from 'vitest';

import {
    anchorDiscriminator,
    ASSOCIATED_TOKEN_PROGRAM_ADDRESS,
    associatedTokenAddress,
    buildActionInstruction,
    buildCreateMarketFromMetadata,
    createMarket,
    marketPda,
    parseStrategyMetadata,
    TOKEN_2022_PROGRAM_ADDRESS,
    TOKEN_PROGRAM_ADDRESS,
    type MarketView,
    type StrategyMetadata,
} from '../src/index.js';
import { hex, key } from './helpers.js';

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

    it('builds the deposit action the template program expects', async () => {
        const metadata = templateMetadata();
        const deposit = metadata.actions?.find((action) => action.name === 'deposit');
        expect(deposit).toBeDefined();
        const marketState = await pool(30);
        const market: MarketView = {
            market: await marketPda(strategyProgram, marketState),
            strategyProgram,
            marketState,
            mints: [mintA, mintB],
            vaults: [key(16), key(17)],
            tokenPrograms: [TOKEN_PROGRAM_ADDRESS, TOKEN_2022_PROGRAM_ADDRESS],
            supportsExactOut: true,
        };
        const instruction = await buildActionInstruction({
            action: deposit!,
            market,
            wallet,
            args: { amount_a: 1_000n, amount_b: 2_000n },
        });
        expect(instruction.programAddress).toBe(strategyProgram);
        expect(hex(instruction.data)).toBe(
            hex(await anchorDiscriminator('deposit')) + 'e803000000000000' + 'd007000000000000',
        );
        // The order of the `Deposit` accounts struct.
        expect(instruction.accounts).toEqual([
            { address: wallet, role: AccountRole.READONLY_SIGNER },
            { address: marketState, role: AccountRole.WRITABLE },
            { address: await associatedTokenAddress(wallet, mintA, TOKEN_PROGRAM_ADDRESS), role: AccountRole.WRITABLE },
            {
                address: await associatedTokenAddress(wallet, mintB, TOKEN_2022_PROGRAM_ADDRESS),
                role: AccountRole.WRITABLE,
            },
            { address: key(16), role: AccountRole.WRITABLE },
            { address: key(17), role: AccountRole.WRITABLE },
            { address: mintA, role: AccountRole.READONLY },
            { address: mintB, role: AccountRole.READONLY },
            { address: TOKEN_PROGRAM_ADDRESS, role: AccountRole.READONLY },
            { address: TOKEN_2022_PROGRAM_ADDRESS, role: AccountRole.READONLY },
        ]);
    });
});
