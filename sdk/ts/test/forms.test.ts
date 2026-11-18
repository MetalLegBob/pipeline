// Building instructions from metadata `init` and `seed`, checked against addresses derived
// independently, modelled on the constant-product test strategy.

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
    buildCreateMarketFromMetadata,
    buildSeedData,
    createMarket,
    encodeArg,
    marketPda,
    MetadataInstructionError,
    resolveAccountSource,
    resolveInit,
    resolveMetadataAccounts,
    resolveSeedAccounts,
    STRATEGY_DISCRIMINATORS,
    SYSTEM_PROGRAM_ADDRESS,
    ASSOCIATED_TOKEN_PROGRAM_ADDRESS,
    TOKEN_2022_PROGRAM_ADDRESS,
    TOKEN_PROGRAM_ADDRESS,
    type MarketView,
    type MetadataAccount,
    type MetadataInit,
    type MetadataSeed,
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

describe('account sources', () => {
    it('resolves an existing market’s accounts and the wallet’s token accounts', async () => {
        const accounts: MetadataAccount[] = [
            { name: 'depositor', source: 'wallet', signer: true },
            { name: 'pool', source: 'market_state', writable: true },
            { name: 'depositor_a', source: 'wallet_token_account:0', writable: true },
            { name: 'depositor_b', source: 'wallet_token_account:1', writable: true },
            { name: 'vault_a', source: 'vault:0', writable: true },
            { name: 'vault_b', source: 'vault:1', writable: true },
            { name: 'mint_a', source: 'mint:0' },
            { name: 'mint_b', source: 'mint:1' },
            { name: 'token_program_a', source: 'token_program:0' },
            { name: 'token_program_b', source: 'token_program:1' },
        ];
        const view = await market();
        const metas = await resolveMetadataAccounts(accounts, {
            strategyProgram,
            wallet,
            market: view,
            marketState: view.marketState,
        });
        expect(metas).toEqual([
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

    it('resolves pda, ata and input sources', async () => {
        const accounts: MetadataAccount[] = [
            { name: 'position', source: 'pda:strategy:str:position,key:market_state,key:wallet,arg:epoch' },
            { name: 'other', source: `pda:${key(3)}:hex:ff` },
            { name: 'reward', source: 'ata:wallet,input:reward_mint', writable: true },
            { name: 'reward_mint', source: 'input:reward_mint' },
            { name: 'market', source: 'market' },
            { name: 'program', source: 'strategy_program' },
            { name: 'system', source: 'system_program' },
        ];
        const view = await market();
        const metas = await resolveMetadataAccounts(accounts, {
            strategyProgram,
            wallet,
            market: view,
            marketState: view.marketState,
            args: { epoch: encodeArg('u16', 3) },
            inputs: { reward_mint: key(20) },
            tokenPrograms: { [key(20)]: TOKEN_2022_PROGRAM_ADDRESS },
        });
        const [position] = await getProgramDerivedAddress({
            programAddress: strategyProgram,
            seeds: [new TextEncoder().encode('position'), bytes(key(8)), bytes(wallet), new Uint8Array([3, 0])],
        });
        const [other] = await getProgramDerivedAddress({ programAddress: key(3), seeds: [new Uint8Array([0xff])] });
        expect(metas.map((a) => a.address)).toEqual([
            position,
            other,
            await associatedTokenAddress(wallet, key(20), TOKEN_2022_PROGRAM_ADDRESS),
            key(20),
            view.market,
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

describe('seed', () => {
    const seeder = key(31);
    const escrows = [key(32), key(33)];

    async function seededPool(mints: Address[], fee: number): Promise<Address> {
        const [pool] = await getProgramDerivedAddress({
            programAddress: strategyProgram,
            seeds: [
                new TextEncoder().encode('seeded'),
                bytes(seeder),
                bytes(mints[0]!),
                bytes(mints[1]!),
                new Uint8Array([fee & 0xff, fee >> 8]),
            ],
        });
        return pool;
    }

    it('builds the seed data from the args', () => {
        const seed = sampleMetadata().seed as MetadataSeed;
        expect(hex(buildSeedData(seed, { fee_bps: 30 }))).toBe('1e00');
        expect(hex(buildSeedData({}, {}))).toBe('');
        expect(() => buildSeedData(seed, {})).toThrow('missing argument "fee_bps"');
    });

    it('resolves the constant-product pool a seeder opens, with Token and Token-2022 mints', async () => {
        const seed = sampleMetadata().seed as MetadataSeed;
        const resolved = await resolveSeedAccounts({
            seed,
            strategyProgram,
            seeder,
            mints: [mintA, mintB],
            tokenPrograms: [TOKEN_PROGRAM_ADDRESS, TOKEN_2022_PROGRAM_ADDRESS],
            escrows,
            args: { fee_bps: 300 },
        });
        const pool = await seededPool([mintA, mintB], 300);
        expect(resolved.marketState).toBe(pool);
        expect(resolved.market).toBe(await marketPda(strategyProgram, pool));
        expect(hex(resolved.seedData)).toBe('2c01');
        expect(resolved.accounts).toEqual([
            { address: await associatedTokenAddress(pool, mintA, TOKEN_PROGRAM_ADDRESS), role: AccountRole.WRITABLE },
            {
                address: await associatedTokenAddress(pool, mintB, TOKEN_2022_PROGRAM_ADDRESS),
                role: AccountRole.WRITABLE,
            },
            { address: ASSOCIATED_TOKEN_PROGRAM_ADDRESS, role: AccountRole.READONLY },
        ]);

        // Index i is the seeder's mint i, so swapping the mints gives another pool.
        const swapped = await resolveSeedAccounts({
            seed,
            strategyProgram,
            seeder,
            mints: [mintB, mintA],
            tokenPrograms: [TOKEN_2022_PROGRAM_ADDRESS, TOKEN_PROGRAM_ADDRESS],
            args: { fee_bps: 300 },
        });
        const swappedPool = await seededPool([mintB, mintA], 300);
        expect(swapped.marketState).toBe(swappedPool);
        expect(swapped.accounts[0]!.address).toBe(
            await associatedTokenAddress(swappedPool, mintB, TOKEN_2022_PROGRAM_ADDRESS),
        );
    });

    it('resolves the seeder-only sources', async () => {
        const seed: MetadataSeed = {
            accounts: [
                { name: 'seeder', source: 'seeder' },
                { name: 'escrow', source: 'escrow:1', writable: true },
                { name: 'lp', source: 'lp_recipient' },
                { name: 'lp_account', source: 'ata:lp_recipient,mint:0', writable: true },
                { name: 'program_b', source: 'token_program:1' },
                { name: 'program_of_b', source: 'token_program_of:mint:1' },
                { name: 'state', source: 'market_state' },
                { name: 'market', source: 'market' },
            ],
        };
        const resolved = await resolveSeedAccounts({
            seed,
            strategyProgram,
            seeder,
            mints: [mintA, mintB],
            tokenPrograms: [TOKEN_PROGRAM_ADDRESS, TOKEN_2022_PROGRAM_ADDRESS],
            escrows,
            lpRecipient: key(34),
            marketState: key(35),
        });
        expect(resolved.marketState).toBe(key(35));
        expect(hex(resolved.seedData)).toBe('');
        expect(resolved.accounts.map((a) => a.address)).toEqual([
            seeder,
            escrows[1],
            key(34),
            await associatedTokenAddress(key(34), mintA, TOKEN_PROGRAM_ADDRESS),
            TOKEN_2022_PROGRAM_ADDRESS,
            TOKEN_2022_PROGRAM_ADDRESS,
            key(35),
            await marketPda(strategyProgram, key(35)),
        ]);
    });

    it('fails clearly on missing or inconsistent inputs', async () => {
        const seed = sampleMetadata().seed as MetadataSeed;
        const base = {
            strategyProgram,
            seeder,
            mints: [mintA, mintB],
            tokenPrograms: [TOKEN_PROGRAM_ADDRESS, TOKEN_2022_PROGRAM_ADDRESS],
            args: { fee_bps: 30 },
        };
        await expect(resolveSeedAccounts({ ...base, seed, tokenPrograms: [TOKEN_PROGRAM_ADDRESS] })).rejects.toThrow(
            'one token program per seeded mint',
        );
        await expect(resolveSeedAccounts({ ...base, seed: {} })).rejects.toThrow('pass marketState');
        await expect(resolveSeedAccounts({ ...base, seed, marketState: key(35) })).rejects.toThrow('differs');
        await expect(
            resolveSeedAccounts({ ...base, seed: { market_state: 'market' }, marketState: key(35) }),
        ).rejects.toThrow('the market itself');
        await expect(
            resolveSeedAccounts({
                ...base,
                seed: { accounts: [{ name: 'e', source: 'escrow:0' }] },
                marketState: key(35),
            }),
        ).rejects.toThrow("needs the seeder's escrows");
        await expect(resolveAccountSource('seeder', { strategyProgram })).rejects.toThrow('needs a seeder');
    });
});
