// Router instruction builders. Each mirrors the builder of the same name in the Rust crate
// `pipeline-client`: same accounts, order, signer and writable flags, and data bytes.
// Builders are async only because kit derives PDAs with WebCrypto.

import {
    AccountRole,
    getAddressEncoder,
    getProgramDerivedAddress,
    getStructCodec,
    getU64Codec,
    getU8Codec,
    type AccountMeta,
    type Address,
    type Instruction,
    type InstructionWithAccounts,
    type InstructionWithData,
    type ReadonlyUint8Array,
} from '@solana/kit';

import {
    getBorshBytesCodec,
    getBorshStringCodec,
    getConfigArgsCodec,
    getRouteArgsCodec,
    getSwapArgsCodec,
    getUpdateConfigArgsCodec,
    instructionData,
    marketFeeMint,
    marketMints,
    type ConfigArgs,
    type Market,
    type RouteHop,
    type SwapArgs,
    type UpdateConfigArgs,
} from './codecs.js';
import {
    ADDRESS_LOOKUP_TABLE_PROGRAM_ADDRESS,
    ASSOCIATED_TOKEN_PROGRAM_ADDRESS,
    INTERFACE_VERSION,
    LOOKUP_TABLE_ONE_TRANSACTION_ADDRESSES,
    MAX_ROUTE_HOPS,
    NATIVE_MINT_ADDRESS,
    PIPELINE_PROGRAM_ADDRESS,
    ROUTER_INSTRUCTION_DISCRIMINATORS as DISCRIMINATORS,
    SYSTEM_PROGRAM_ADDRESS,
} from './constants.js';
import {
    associatedTokenAddress,
    configPda,
    eventAuthorityPda,
    feeAccountPda,
    feeAuthorityPda,
    lookupTableRecordPda,
    setupPayerPda,
    marketPda,
    programDataPda,
    strategyPda,
} from './pda.js';

/** An instruction with its accounts and data always present. */
export type PipelineInstruction = Instruction &
    InstructionWithAccounts<readonly AccountMeta[]> &
    InstructionWithData<Uint8Array>;

function readonly(address: Address): AccountMeta {
    return { address, role: AccountRole.READONLY };
}

function writable(address: Address): AccountMeta {
    return { address, role: AccountRole.WRITABLE };
}

function readonlySigner(address: Address): AccountMeta {
    return { address, role: AccountRole.READONLY_SIGNER };
}

function writableSigner(address: Address): AccountMeta {
    return { address, role: AccountRole.WRITABLE_SIGNER };
}

function routerInstruction(accounts: AccountMeta[], data: Uint8Array): PipelineInstruction {
    return { programAddress: PIPELINE_PROGRAM_ADDRESS, accounts, data };
}

function discriminatorOnly(discriminator: ReadonlyUint8Array): Uint8Array {
    return Uint8Array.from(discriminator);
}

/** The two accounts `#[event_cpi]` appends to an instruction. */
async function eventCpiAccounts(): Promise<AccountMeta[]> {
    return [readonly(await eventAuthorityPda()), readonly(PIPELINE_PROGRAM_ADDRESS)];
}

/** `initialize_config`, signed by the router's upgrade authority. */
export async function initializeConfig(input: { admin: Address; args: ConfigArgs }): Promise<PipelineInstruction> {
    return routerInstruction(
        [
            writableSigner(input.admin),
            writable(await configPda()),
            readonly(PIPELINE_PROGRAM_ADDRESS),
            readonly(await programDataPda(PIPELINE_PROGRAM_ADDRESS)),
            readonly(SYSTEM_PROGRAM_ADDRESS),
        ],
        instructionData(DISCRIMINATORS.initializeConfig, getConfigArgsCodec(), input.args),
    );
}

export async function updateConfig(input: { admin: Address; args: UpdateConfigArgs }): Promise<PipelineInstruction> {
    return routerInstruction(
        [readonlySigner(input.admin), writable(await configPda()), ...(await eventCpiAccounts())],
        instructionData(DISCRIMINATORS.updateConfig, getUpdateConfigArgsCodec(), input.args),
    );
}

const registerStrategyArgsCodec = getStructCodec([
    ['metadataUri', getBorshStringCodec()],
    ['interfaceVersion', getU8Codec()],
    ['maxListingFeeLamports', getU64Codec()],
]);

/**
 * `register_strategy`. `registrar` is the program's upgrade authority or its registration
 * PDA ({@link registerPda}). Fails if the listing fee is above `maxListingFeeLamports`.
 */
export async function registerStrategy(input: {
    payer: Address;
    registrar: Address;
    strategyProgram: Address;
    listingFeeRecipient: Address;
    metadataUri: string;
    maxListingFeeLamports: bigint;
}): Promise<PipelineInstruction> {
    return routerInstruction(
        [
            writableSigner(input.payer),
            readonlySigner(input.registrar),
            readonly(input.strategyProgram),
            readonly(await programDataPda(input.strategyProgram)),
            readonly(await configPda()),
            writable(input.listingFeeRecipient),
            writable(await strategyPda(input.strategyProgram)),
            readonly(SYSTEM_PROGRAM_ADDRESS),
            ...(await eventCpiAccounts()),
        ],
        instructionData(DISCRIMINATORS.registerStrategy, registerStrategyArgsCodec, {
            metadataUri: input.metadataUri,
            interfaceVersion: INTERFACE_VERSION,
            maxListingFeeLamports: input.maxListingFeeLamports,
        }),
    );
}

export async function updateMetadata(input: {
    registrar: Address;
    strategyProgram: Address;
    metadataUri: string;
}): Promise<PipelineInstruction> {
    return routerInstruction(
        [
            readonlySigner(input.registrar),
            readonly(input.strategyProgram),
            readonly(await programDataPda(input.strategyProgram)),
            writable(await strategyPda(input.strategyProgram)),
            ...(await eventCpiAccounts()),
        ],
        instructionData(DISCRIMINATORS.updateMetadata, getBorshStringCodec(), input.metadataUri),
    );
}

/**
 * `create_market`. `strategyAccounts` are the strategy's own setup accounts, passed after
 * the router's fixed accounts. `maxCreatorLamports` caps the SOL the strategy's setup may
 * take from the creator; set `marketStateSigns` if the market state is a fresh keypair
 * that signs the transaction.
 */
export async function createMarket(input: {
    creator: Address;
    strategyProgram: Address;
    marketState: Address;
    marketStateSigns?: boolean;
    initData: ReadonlyUint8Array;
    maxCreatorLamports: bigint;
    strategyAccounts?: readonly AccountMeta[];
}): Promise<PipelineInstruction> {
    const market = await marketPda(input.strategyProgram, input.marketState);
    return routerInstruction(
        [
            writableSigner(input.creator),
            writable(await strategyPda(input.strategyProgram)),
            readonly(input.strategyProgram),
            input.marketStateSigns ? writableSigner(input.marketState) : writable(input.marketState),
            writable(market),
            writable(await setupPayerPda(market)),
            readonly(SYSTEM_PROGRAM_ADDRESS),
            ...(await eventCpiAccounts()),
            ...(input.strategyAccounts ?? []),
        ],
        instructionData(DISCRIMINATORS.createMarket, createMarketArgsCodec, {
            initData: input.initData,
            maxCreatorLamports: input.maxCreatorLamports,
        }),
    );
}

const createMarketArgsCodec = getStructCodec([
    ['initData', getBorshBytesCodec()],
    ['maxCreatorLamports', getU64Codec()],
]);

/**
 * `register_existing_market`. `infoAccounts` are passed to the strategy's
 * `pipeline_market_info` after the market state.
 */
export async function registerExistingMarket(input: {
    payer: Address;
    strategyProgram: Address;
    marketState: Address;
    infoAccounts?: readonly AccountMeta[];
}): Promise<PipelineInstruction> {
    return routerInstruction(
        [
            writableSigner(input.payer),
            writable(await strategyPda(input.strategyProgram)),
            readonly(input.strategyProgram),
            readonly(input.marketState),
            writable(await marketPda(input.strategyProgram, input.marketState)),
            readonly(SYSTEM_PROGRAM_ADDRESS),
            ...(await eventCpiAccounts()),
            ...(input.infoAccounts ?? []),
        ],
        discriminatorOnly(DISCRIMINATORS.registerExistingMarket),
    );
}

export async function initFeeAccount(input: {
    payer: Address;
    market: Address;
    mint: Address;
    tokenProgram: Address;
}): Promise<PipelineInstruction> {
    return routerInstruction(
        [
            writableSigner(input.payer),
            readonly(input.market),
            readonly(await feeAuthorityPda(input.market)),
            readonly(input.mint),
            writable(await feeAccountPda(input.market, input.mint, input.tokenProgram)),
            readonly(input.tokenProgram),
            readonly(ASSOCIATED_TOKEN_PROGRAM_ADDRESS),
            readonly(SYSTEM_PROGRAM_ADDRESS),
        ],
        discriminatorOnly(DISCRIMINATORS.initFeeAccount),
    );
}

/** Everything needed to build a `swap` instruction. */
export type SwapAccounts = {
    /** Signs the swap. Never passed to the strategy as a signer. */
    trader: Address;
    strategyProgram: Address;
    marketState: Address;
    traderInput: Address;
    traderOutput: Address;
    inputVault: Address;
    inputMint: Address;
    outputMint: Address;
    inputTokenProgram: Address;
    outputTokenProgram: Address;
    /** The integrator's token account for the input mint, if an integrator fee is charged. */
    integratorFeeAccount?: Address | null;
    /** Strategy-specific accounts, as returned by `pipeline_resolve_accounts`. */
    strategyAccounts?: readonly AccountMeta[];
    /** Transfer-hook accounts for the input mint (empty if it has no hook). */
    inputHookAccounts?: readonly AccountMeta[];
};

/** `swap` arguments; `inputHookAccounts` is always set from the accounts. */
export type SwapInstructionArgs = Omit<SwapArgs, 'inputHookAccounts'> & { inputHookAccounts?: number };

/**
 * Which side of a trade pays Pipeline's fee. The router takes it in the output when the
 * output is the market's fee mint, and in the input otherwise.
 */
export type FeeSide = 'input' | 'output';

/**
 * The side the router uses for a market that declares no fee mint: the output when it is
 * wrapped SOL, the input otherwise.
 */
export function feeSideByDefault(outputMint: Address): FeeSide {
    return outputMint === NATIVE_MINT_ADDRESS ? 'output' : 'input';
}

/** The side a trade into `outputMint` pays on, given the market's record. */
export function feeSideOf(market: Market, outputMint: Address): FeeSide {
    return marketFeeMint(market) === outputMint ? 'output' : 'input';
}

/** The market's fee account a swap pays Pipeline's fee into. */
export async function swapFeeAccount(accounts: SwapAccounts, side: FeeSide): Promise<Address> {
    const market = await marketPda(accounts.strategyProgram, accounts.marketState);
    return side === 'output'
        ? feeAccountPda(market, accounts.outputMint, accounts.outputTokenProgram)
        : feeAccountPda(market, accounts.inputMint, accounts.inputTokenProgram);
}

function hookCount(accounts: SwapAccounts): number {
    const count = (accounts.inputHookAccounts ?? []).length;
    if (count > 0xff) {
        throw new RangeError(`at most 255 input hook accounts, got ${count}`);
    }
    return count;
}

/** The accounts from `strategy_program` to `fee_authority`, shared by `swap` and each `route` hop. */
async function tradeAccounts(accounts: SwapAccounts, market: Address): Promise<AccountMeta[]> {
    return [
        readonly(accounts.strategyProgram),
        readonly(market),
        writable(accounts.marketState),
        writable(accounts.traderInput),
        writable(accounts.traderOutput),
        writable(accounts.inputVault),
        readonly(accounts.inputMint),
        readonly(accounts.outputMint),
        readonly(accounts.inputTokenProgram),
        readonly(accounts.outputTokenProgram),
        readonly(await feeAuthorityPda(market)),
    ];
}

/** Anchor encodes a missing optional account as the program ID. */
function integratorAccount(account: Address | null | undefined): AccountMeta {
    return account ? writable(account) : readonly(PIPELINE_PROGRAM_ADDRESS);
}

/**
 * `swap`, paying Pipeline's fee on `feeSide`. Pass {@link feeSideOf} with the market's
 * record; the default is {@link feeSideByDefault}, the side the router uses for a market
 * that declares no fee mint. The router refuses the swap (`WrongFeeAccount`) if the side
 * doesn't match the market. `args.inputHookAccounts` is set from `accounts.inputHookAccounts`.
 */
export async function swap(
    accounts: SwapAccounts,
    args: SwapInstructionArgs,
    feeSide: FeeSide = feeSideByDefault(accounts.outputMint),
): Promise<PipelineInstruction> {
    const market = await marketPda(accounts.strategyProgram, accounts.marketState);
    const hooks = hookCount(accounts);
    return routerInstruction(
        [
            readonlySigner(accounts.trader),
            ...(await tradeAccounts(accounts, market)),
            writable(await swapFeeAccount(accounts, feeSide)),
            integratorAccount(accounts.integratorFeeAccount),
            ...(await eventCpiAccounts()),
            ...(accounts.strategyAccounts ?? []),
            ...(accounts.inputHookAccounts ?? []),
        ],
        instructionData(DISCRIMINATORS.swap, getSwapArgsCodec(), { ...args, inputHookAccounts: hooks }),
    );
}

/**
 * One hop of a `route`: the same accounts as a single swap (its `trader` and
 * `integratorFeeAccount` are ignored; the route's apply), the side the hop pays Pipeline's
 * fee on (default {@link feeSideByDefault}), and the strategy's `extra` data.
 */
export type RouteHopInput = {
    accounts: SwapAccounts;
    feeSide?: FeeSide;
    extra?: ReadonlyUint8Array;
};

/**
 * `route`: up to {@link MAX_ROUTE_HOPS} exact-in hops in one instruction. Each hop's input
 * account and mint must be the previous hop's output account and mint (the trader's own
 * accounts). `amountIn` is what the first hop sends; later hops spend what the previous
 * one left the trader. `minAmountOut` applies to what the trader keeps from the last hop;
 * an integrator fee is charged once, on the first hop's input.
 */
export async function route(input: {
    trader: Address;
    integratorFeeAccount?: Address | null;
    hops: readonly RouteHopInput[];
    amountIn: bigint;
    minAmountOut: bigint;
    integratorFeeBps: number;
}): Promise<PipelineInstruction> {
    if (input.hops.length === 0 || input.hops.length > MAX_ROUTE_HOPS) {
        throw new RangeError(`a route has 1 to ${MAX_ROUTE_HOPS} hops, got ${input.hops.length}`);
    }
    const accounts: AccountMeta[] = [
        readonlySigner(input.trader),
        integratorAccount(input.integratorFeeAccount),
        ...(await eventCpiAccounts()),
    ];
    const hops: RouteHop[] = [];
    for (const hop of input.hops) {
        const a = hop.accounts;
        const market = await marketPda(a.strategyProgram, a.marketState);
        const strategyAccounts = a.strategyAccounts ?? [];
        if (strategyAccounts.length > 0xff) {
            throw new RangeError(`at most 255 strategy accounts per hop, got ${strategyAccounts.length}`);
        }
        const hooks = hookCount(a);
        accounts.push(
            ...(await tradeAccounts(a, market)),
            writable(await swapFeeAccount(a, hop.feeSide ?? feeSideByDefault(a.outputMint))),
            ...strategyAccounts,
            ...(a.inputHookAccounts ?? []),
        );
        hops.push({
            strategyAccounts: strategyAccounts.length,
            inputHookAccounts: hooks,
            extra: hop.extra ?? new Uint8Array(),
        });
    }
    return routerInstruction(
        accounts,
        instructionData(DISCRIMINATORS.route, getRouteArgsCodec(), {
            amountIn: input.amountIn,
            minAmountOut: input.minAmountOut,
            integratorFeeBps: input.integratorFeeBps,
            hops,
        }),
    );
}

/**
 * `record_lookup_table`: records `table` as the market's address lookup table. `mints` are
 * the market's mints in order (the router reads their token programs). The table must hold
 * {@link lookupTableAddresses}.
 */
export async function recordLookupTable(input: {
    payer: Address;
    market: Address;
    table: Address;
    mints: readonly Address[];
}): Promise<PipelineInstruction> {
    return routerInstruction(
        [
            writableSigner(input.payer),
            readonly(input.market),
            readonly(input.table),
            writable(await lookupTableRecordPda(input.market)),
            readonly(SYSTEM_PROGRAM_ADDRESS),
            ...(await eventCpiAccounts()),
            ...input.mints.map(readonly),
        ],
        discriminatorOnly(DISCRIMINATORS.recordLookupTable),
    );
}

/** A new lookup table's address, derived from its authority and the recent slot it is created at. */
export async function lookupTableAddress(authority: Address, recentSlot: bigint): Promise<[Address, number]> {
    const [table, bump] = await getProgramDerivedAddress({
        programAddress: ADDRESS_LOOKUP_TABLE_PROGRAM_ADDRESS,
        seeds: [getAddressEncoder().encode(authority), getU64Codec().encode(recentSlot)],
    });
    return [table, bump];
}

/**
 * A market's lookup table, created, filled, frozen and recorded: the table's address and the
 * transactions to send in order, each a list of instructions that fits in one legacy
 * transaction. `addresses` is what the table holds (at least {@link lookupTableAddresses}; a
 * strategy's static accounts may follow), `mints` the market's mints in order, and
 * `recentSlot` a slot from the last 150 or so, which picks the table's address (so one payer can
create one table per slot). `payer` pays
 * the rent and is the table's authority until it is frozen. Send this after creating a
 * market, so integrators can build v0 transactions for it.
 */
export async function createMarketLookupTable(input: {
    payer: Address;
    market: Address;
    mints: readonly Address[];
    addresses: readonly Address[];
    recentSlot: bigint;
}): Promise<{ table: Address; transactions: PipelineInstruction[][] }> {
    const [table, bump] = await lookupTableAddress(input.payer, input.recentSlot);
    const tableAccounts = (extend: boolean): AccountMeta[] =>
        extend
            ? [
                  writable(table),
                  readonlySigner(input.payer),
                  writableSigner(input.payer),
                  readonly(SYSTEM_PROGRAM_ADDRESS),
              ]
            : [writable(table), readonlySigner(input.payer)];
    const u32 = (n: number) => new Uint8Array([n, 0, 0, 0]);
    const create: PipelineInstruction = {
        programAddress: ADDRESS_LOOKUP_TABLE_PROGRAM_ADDRESS,
        accounts: tableAccounts(true),
        data: concat([u32(0), getU64Codec().encode(input.recentSlot), new Uint8Array([bump])]),
    };
    const extend: PipelineInstruction = {
        programAddress: ADDRESS_LOOKUP_TABLE_PROGRAM_ADDRESS,
        accounts: tableAccounts(true),
        data: concat([
            u32(2),
            getU64Codec().encode(BigInt(input.addresses.length)),
            ...input.addresses.map((a) => getAddressEncoder().encode(a)),
        ]),
    };
    const freeze: PipelineInstruction = {
        programAddress: ADDRESS_LOOKUP_TABLE_PROGRAM_ADDRESS,
        accounts: tableAccounts(false),
        data: u32(1),
    };
    const record = await recordLookupTable({ payer: input.payer, market: input.market, table, mints: input.mints });
    const transactions =
        input.addresses.length <= LOOKUP_TABLE_ONE_TRANSACTION_ADDRESSES
            ? [[create, extend, freeze, record]]
            : [
                  [create, extend],
                  [freeze, record],
              ];
    return { table, transactions };
}

function concat(parts: readonly ReadonlyUint8Array[]): Uint8Array {
    const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
    let offset = 0;
    for (const part of parts) {
        out.set(part, offset);
        offset += part.length;
    }
    return out;
}

/**
 * The addresses a market's lookup table must hold for `record_lookup_table` to accept it:
 * the router, its event authority, the market, its state, strategy program and fee
 * authority, then each mint's address, vault, token program and fee account, without
 * duplicates. `tokenPrograms` gives each mint's owning program, in mint order.
 */
export async function lookupTableAddresses(
    marketAddress: Address,
    market: Market,
    tokenPrograms: readonly Address[],
): Promise<Address[]> {
    const feeAuthority = await feeAuthorityPda(marketAddress);
    const addresses: Address[] = [
        PIPELINE_PROGRAM_ADDRESS,
        await eventAuthorityPda(),
        marketAddress,
        market.marketState,
        market.strategyProgram,
        feeAuthority,
    ];
    const mints = marketMints(market);
    for (const [i, mint] of mints.entries()) {
        const tokenProgram = tokenPrograms[i];
        if (tokenProgram === undefined) {
            break;
        }
        addresses.push(
            mint,
            market.vaults[i]!,
            tokenProgram,
            await associatedTokenAddress(feeAuthority, mint, tokenProgram),
        );
    }
    return [...new Set(addresses)];
}

/**
 * `migrate_market`: moves a market recorded before fee mints existed to the current
 * layout. Anyone can call it; `payer` covers the extra rent. A market in the old layout
 * can't be traded (or decoded by {@link decodeMarket}) until it is migrated.
 */
export async function migrateMarket(input: { payer: Address; market: Address }): Promise<PipelineInstruction> {
    return routerInstruction(
        [writableSigner(input.payer), writable(input.market), readonly(SYSTEM_PROGRAM_ADDRESS)],
        discriminatorOnly(DISCRIMINATORS.migrateMarket),
    );
}

/**
 * `sweep_fees`, moving a market's fees in `mint` to the treasury owner's associated token
 * account (which must already exist).
 */
export async function sweepFees(input: {
    market: Address;
    mint: Address;
    tokenProgram: Address;
    treasuryOwner: Address;
    hookAccounts?: readonly AccountMeta[];
}): Promise<PipelineInstruction> {
    return routerInstruction(
        [
            readonly(await configPda()),
            readonly(input.market),
            readonly(await feeAuthorityPda(input.market)),
            readonly(input.mint),
            writable(await feeAccountPda(input.market, input.mint, input.tokenProgram)),
            writable(await associatedTokenAddress(input.treasuryOwner, input.mint, input.tokenProgram)),
            readonly(input.tokenProgram),
            ...(await eventCpiAccounts()),
            ...(input.hookAccounts ?? []),
        ],
        discriminatorOnly(DISCRIMINATORS.sweepFees),
    );
}
