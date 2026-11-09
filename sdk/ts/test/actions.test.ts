// Building instructions from metadata `actions` and `init`, checked against addresses
// derived independently, modelled on the constant-product test strategy.

import {
    AccountRole,
    getAddressEncoder,
    getProgramDerivedAddress,
    type Address,
    type GetAccountInfoApi,
    type Rpc,
} from '@solana/kit';
import { describe, expect, it } from 'vitest';

import {
    anchorDiscriminator,
    associatedTokenAddress,
    buildActionInstruction,
    buildCreateMarketFromMetadata,
    createMarket,
    encodeArg,
    marketPda,
    MetadataInstructionError,
    resolveAccountSource,
    resolveInit,
    STRATEGY_DISCRIMINATORS,
    SYSTEM_PROGRAM_ADDRESS,
    ASSOCIATED_TOKEN_PROGRAM_ADDRESS,
    TOKEN_2022_PROGRAM_ADDRESS,
    TOKEN_PROGRAM_ADDRESS,
    type MarketView,
    type MetadataAction,
    type MetadataInit,
} from '../src/index.js';
import { hex, key } from './helpers.js';
import { sampleMetadata } from './metadata-sample.js';

const strategyProgram = key(7);
const wallet = key(30);
const mintA = key(13);
const mintB = key(14);

async function market(): Promise<MarketView> {
    return {
        market: await marketPda(strategyProgram, key(8)),
        strategyProgram,
        marketState: key(8),
        mints: [mintA, mintB],
        vaults: [key(16), key(17)],
        tokenPrograms: [TOKEN_PROGRAM_ADDRESS, TOKEN_2022_PROGRAM_ADDRESS],
        supportsExactOut: true,
    };
}

const bytes = (address: Address) => getAddressEncoder().encode(address) as Uint8Array;

describe('encodeArg', () => {
    it('Borsh-encodes every type', () => {
        expect(hex(encodeArg('u8', 255))).toBe('ff');
        expect(hex(encodeArg('u16', '258'))).toBe('0201');
        expect(hex(encodeArg('u32', 1n))).toBe('01000000');
        expect(hex(encodeArg('u64', '18446744073709551615'))).toBe('ffffffffffffffff');
        expect(hex(encodeArg('i64', -1))).toBe('ffffffffffffffff');
        expect(hex(encodeArg('bool', 'true'))).toBe('01');
        expect(hex(encodeArg('pubkey', key(1)))).toBe('01'.repeat(32));
        expect(hex(encodeArg('bytes', '0xabcd'))).toBe('02000000abcd');
        expect(hex(encodeArg('bytes', new Uint8Array([1])))).toBe('0100000001');
        expect(hex(encodeArg('string', 'hi'))).toBe('020000006869');
    });

    it('rejects out-of-range and mistyped values', () => {
        expect(() => encodeArg('u8', 256)).toThrow(MetadataInstructionError);
        expect(() => encodeArg('u64', -1n)).toThrow(/out of range/);
        expect(() => encodeArg('u64', 1.5)).toThrow(/integer/);
        expect(() => encodeArg('bool', 1)).toThrow(/boolean/);
        expect(() => encodeArg('pubkey', 'nope')).toThrow(/address/);
        expect(() => encodeArg('bytes', 'xyz')).toThrow(/bytes/);
    });
});

describe('anchorDiscriminator', () => {
    it('matches the strategy interface and the guide example', async () => {
        expect(await anchorDiscriminator('pipeline_init_market')).toEqual(STRATEGY_DISCRIMINATORS.initMarket);
        expect(hex(await anchorDiscriminator('deposit'))).toBe('f223c68952e1f2b6');
    });
});

describe('buildActionInstruction', () => {
    it('builds the constant-product deposit', async () => {
        const action = sampleMetadata().actions[0] as MetadataAction;
        const instruction = await buildActionInstruction({
            action,
            market: await market(),
            wallet,
            args: { amount_a: 1_000n, amount_b: '2000' },
        });
        expect(instruction.programAddress).toBe(strategyProgram);
        expect(hex(instruction.data)).toBe('f223c68952e1f2b6' + 'e803000000000000' + 'd007000000000000');
        expect(instruction.accounts).toEqual([
            { address: wallet, role: AccountRole.READONLY_SIGNER },
            { address: key(8), role: AccountRole.WRITABLE },
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

    it('defaults the discriminator to Anchor’s and resolves pda, ata and input sources', async () => {
        const action: MetadataAction = {
            name: 'claim',
            label: 'Claim',
            args: [{ name: 'epoch', type: 'u16' }],
            accounts: [
                { name: 'position', source: 'pda:strategy:str:position,key:market_state,key:wallet,arg:epoch' },
                { name: 'other', source: `pda:${key(3)}:hex:ff` },
                { name: 'reward', source: 'ata:wallet,input:reward_mint', writable: true },
                { name: 'reward_mint', source: 'input:reward_mint' },
                { name: 'market', source: 'market' },
                { name: 'program', source: 'strategy_program' },
                { name: 'system', source: 'system_program' },
            ],
        };
        const instruction = await buildActionInstruction({
            action,
            market: await market(),
            wallet,
            args: { epoch: 3 },
            inputs: { reward_mint: key(20) },
            tokenPrograms: { [key(20)]: TOKEN_2022_PROGRAM_ADDRESS },
        });
        const [position] = await getProgramDerivedAddress({
            programAddress: strategyProgram,
            seeds: [new TextEncoder().encode('position'), bytes(key(8)), bytes(wallet), new Uint8Array([3, 0])],
        });
        const [other] = await getProgramDerivedAddress({ programAddress: key(3), seeds: [new Uint8Array([0xff])] });
        expect(hex(instruction.data)).toBe(hex(await anchorDiscriminator('claim')) + '0300');
        expect(instruction.accounts.map((a) => a.address)).toEqual([
            position,
            other,
            await associatedTokenAddress(wallet, key(20), TOKEN_2022_PROGRAM_ADDRESS),
            key(20),
            (await market()).market,
            strategyProgram,
            SYSTEM_PROGRAM_ADDRESS,
        ]);
    });

    it('looks up an unknown mint’s token program over RPC', async () => {
        const calls: string[] = [];
        const rpc = {
            getAccountInfo(address: Address) {
                calls.push(address);
                return { send: async () => ({ value: { owner: TOKEN_2022_PROGRAM_ADDRESS } }) };
            },
        } as unknown as Rpc<GetAccountInfoApi>;
        const address = await resolveAccountSource('token_program_of:input:m', {
            strategyProgram,
            inputs: { m: key(21) },
            rpc,
        });
        expect(address).toBe(TOKEN_2022_PROGRAM_ADDRESS);
        expect(calls).toEqual([key(21)]);
    });

    it('fails clearly on missing values', async () => {
        const action = sampleMetadata().actions[0] as MetadataAction;
        await expect(buildActionInstruction({ action, market: await market(), wallet, args: {} })).rejects.toThrow(
            'missing argument "amount_a"',
        );
        await expect(resolveAccountSource('input:x', { strategyProgram })).rejects.toThrow('missing input "x"');
        await expect(resolveAccountSource('vault:0', { strategyProgram })).rejects.toThrow('needs a market');
        await expect(
            resolveAccountSource('token_program_of:input:x', { strategyProgram, inputs: { x: key(1) } }),
        ).rejects.toThrow('pass tokenPrograms or rpc');
    });
});

describe('init', () => {
    const tokenPrograms = { [mintA]: TOKEN_PROGRAM_ADDRESS, [mintB]: TOKEN_2022_PROGRAM_ADDRESS };

    it('resolves the constant-product pool: PDA market state, init data and ATA vaults', async () => {
        const init = sampleMetadata().init as MetadataInit;
        const resolved = await resolveInit({
            init,
            strategyProgram,
            creator: wallet,
            args: { fee_bps: 30 },
            inputs: { mint_a: mintA, mint_b: mintB },
            tokenPrograms,
        });
        const [pool] = await getProgramDerivedAddress({
            programAddress: strategyProgram,
            seeds: [new TextEncoder().encode('pool'), bytes(mintA), bytes(mintB), new Uint8Array([30, 0])],
        });
        expect(resolved.marketState).toBe(pool);
        expect(resolved.marketStateSigns).toBe(false);
        expect(resolved.market).toBe(await marketPda(strategyProgram, pool));
        expect(hex(resolved.initData)).toBe('1e00');
        expect(resolved.strategyAccounts).toEqual([
            { address: mintA, role: AccountRole.READONLY },
            { address: mintB, role: AccountRole.READONLY },
            { address: await associatedTokenAddress(pool, mintA, TOKEN_PROGRAM_ADDRESS), role: AccountRole.WRITABLE },
            {
                address: await associatedTokenAddress(pool, mintB, TOKEN_2022_PROGRAM_ADDRESS),
                role: AccountRole.WRITABLE,
            },
            { address: TOKEN_PROGRAM_ADDRESS, role: AccountRole.READONLY },
            { address: TOKEN_2022_PROGRAM_ADDRESS, role: AccountRole.READONLY },
            { address: ASSOCIATED_TOKEN_PROGRAM_ADDRESS, role: AccountRole.READONLY },
        ]);

        const { instruction } = await buildCreateMarketFromMetadata({
            init,
            strategyProgram,
            creator: wallet,
            args: { fee_bps: 30 },
            inputs: { mint_a: mintA, mint_b: mintB },
            tokenPrograms,
            maxCreatorLamports: 5_000_000n,
        });
        expect(instruction).toEqual(
            await createMarket({
                creator: wallet,
                strategyProgram,
                marketState: pool,
                initData: resolved.initData,
                maxCreatorLamports: 5_000_000n,
                strategyAccounts: resolved.strategyAccounts,
            }),
        );
    });

    it('uses a new keypair market state by default, which signs', async () => {
        const init: MetadataInit = { accounts: [{ name: 'state', source: 'market_state' }] };
        const resolved = await resolveInit({ init, strategyProgram, creator: wallet, marketState: key(40) });
        expect(resolved).toMatchObject({ marketState: key(40), marketStateSigns: true, initData: new Uint8Array() });
        expect(resolved.strategyAccounts).toEqual([{ address: key(40), role: AccountRole.READONLY }]);
        await expect(resolveInit({ init, strategyProgram, creator: wallet })).rejects.toThrow('new market state');
        await expect(
            resolveInit({
                init: { accounts: [{ name: 'm', source: 'mint:0' }] },
                strategyProgram,
                creator: wallet,
                marketState: key(40),
            }),
        ).rejects.toThrow('before the market exists');
    });
});
