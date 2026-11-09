// Lists this strategy on Pipeline's devnet router and trades it once, step by step:
//
//   1. check the program is deployed and the wallet is its upgrade authority;
//   2. check metadata.json, and the published copy at METADATA_URI;
//   3. register the strategy (0.01 devnet SOL, once) or update its metadata URI;
//   4. create two test tokens;
//   5. create a market and record it with the router, with fee accounts, in one transaction;
//   6. add liquidity through the `deposit` action described in metadata.json;
//   7. quote a swap, then swap through the router.
//
// Deploy the program itself first (`solana program deploy`, see the README).
//
// Usage, from this directory after `npm install`:
//   node deploy-devnet.ts <metadata URI>
// Environment:
//   METADATA_URI  where metadata.json is published (instead of the argument)
//   KEYPAIR       the wallet that pays and signs; it must be the program's upgrade
//                 authority (default ~/.config/solana/id.json)
//   PROGRAM_ID    the strategy program (default: the address in declare_id!)
//   RPC_URL       default https://api.devnet.solana.com

import { readFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

import {
    address,
    addSignersToTransactionMessage,
    appendTransactionMessageInstructions,
    createDefaultRpcTransport,
    createKeyPairSignerFromBytes,
    createSolanaRpcFromTransport,
    createTransactionMessage,
    generateKeyPairSigner,
    getBase64EncodedWireTransaction,
    getSignatureFromTransaction,
    pipe,
    setTransactionMessageFeePayerSigner,
    setTransactionMessageLifetimeUsingBlockhash,
    signTransactionMessageWithSigners,
    type Address,
    type Instruction,
    type KeyPairSigner,
} from '@solana/kit';
import { getCreateAccountInstruction } from '@solana-program/system';
import {
    findAssociatedTokenPda,
    getCreateAssociatedTokenIdempotentInstructionAsync,
    getInitializeMint2Instruction,
    getMintSize,
    getMintToInstruction,
    TOKEN_PROGRAM_ADDRESS,
} from '@solana-program/token';
import * as pipeline from '@pipeline-placeholder/sdk';

// Written by scripts/new-strategy.sh; the same address as `declare_id!`.
const DEFAULT_PROGRAM_ID = 'MyStrategy111111111111111111111111111111111';

const POOL_FEE_BPS = 30;
const MINTED = 1_000_000_000_000n;
const DEPOSIT_A = 100_000_000_000n;
const DEPOSIT_B = 200_000_000_000n;
const SWAP_AMOUNT = 1_000_000n;
// The most SOL the strategy's setup may spend on rent; the router returns the rest.
const MAX_CREATOR_LAMPORTS = 100_000_000n;

const rpcUrl = process.env.RPC_URL ?? 'https://api.devnet.solana.com';
const onDevnet = rpcUrl.includes('devnet');
// What to pass to `pipeline-check --url`.
const checkUrl = onDevnet ? 'devnet' : rpcUrl;

// Public RPC endpoints rate-limit bursts (HTTP 429); retry those with back-off.
const baseTransport = createDefaultRpcTransport({ url: rpcUrl });
const retryingTransport = (async (...args: Parameters<typeof baseTransport>) => {
    for (let attempt = 0; ; attempt++) {
        try {
            return await baseTransport(...args);
        } catch (error) {
            if (attempt >= 8 || !String(error).includes('429')) throw error;
            await new Promise((resolve) => setTimeout(resolve, 500 * 2 ** attempt));
        }
    }
}) as typeof baseTransport;
const rpc = createSolanaRpcFromTransport(retryingTransport);

function step(title: string) {
    console.log(`\n== ${title}`);
}

function info(message: string) {
    console.log(`   ${message}`);
}

function fail(message: string): never {
    console.error(`\nStopped: ${message}`);
    process.exit(1);
}

function explorer(kind: 'tx' | 'address', value: string) {
    const suffix = onDevnet ? '?cluster=devnet' : '';
    return `https://orbmarkets.io/${kind}/${value}${suffix}`;
}

async function loadWallet(): Promise<KeyPairSigner> {
    const path = process.env.KEYPAIR ?? join(homedir(), '.config/solana/id.json');
    const bytes = new Uint8Array(JSON.parse(readFileSync(path, 'utf8')));
    return createKeyPairSignerFromBytes(bytes);
}

/** Sends a transaction and waits for confirmation by polling (public RPCs often refuse WebSockets). */
async function send(label: string, payer: KeyPairSigner, instructions: Instruction[], extra: KeyPairSigner[] = []) {
    const { value: blockhash } = await rpc.getLatestBlockhash({ commitment: 'confirmed' }).send();
    const message = pipe(
        createTransactionMessage({ version: 0 }),
        (m) => setTransactionMessageFeePayerSigner(payer, m),
        (m) => setTransactionMessageLifetimeUsingBlockhash(blockhash, m),
        (m) => appendTransactionMessageInstructions(instructions, m),
        (m) => addSignersToTransactionMessage(extra, m),
    );
    const signed = await signTransactionMessageWithSigners(message);
    const signature = getSignatureFromTransaction(signed);
    await rpc.sendTransaction(getBase64EncodedWireTransaction(signed), { encoding: 'base64' }).send();
    for (let i = 0; ; i++) {
        const { value } = await rpc.getSignatureStatuses([signature]).send();
        const status = value[0];
        if (status?.err) fail(`${label} failed: ${JSON.stringify(status.err)} (${explorer('tx', signature)})`);
        if (status?.confirmationStatus === 'confirmed' || status?.confirmationStatus === 'finalized') break;
        if (i > 120) fail(`${label}: not confirmed after 60 s (${explorer('tx', signature)})`);
        await new Promise((resolve) => setTimeout(resolve, 500));
    }
    info(`${label}: ${explorer('tx', signature)}`);
    return signature;
}

async function accountData(account: Address): Promise<Uint8Array | null> {
    const { value } = await rpc.getAccountInfo(account, { encoding: 'base64', commitment: 'confirmed' }).send();
    return value ? new Uint8Array(Buffer.from(value.data[0], 'base64')) : null;
}

async function tokenBalance(account: Address): Promise<bigint> {
    const { value } = await rpc.getTokenAccountBalance(account, { commitment: 'confirmed' }).send();
    return BigInt(value.amount);
}

async function walletTokenAccount(owner: Address, mint: Address): Promise<Address> {
    const [account] = await findAssociatedTokenPda({ owner, mint, tokenProgram: TOKEN_PROGRAM_ADDRESS });
    return account;
}

/** A new SPL Token mint, with `amount` minted to the wallet. */
async function createTestToken(wallet: KeyPairSigner, label: string, decimals: number, amount: bigint) {
    const mint = await generateKeyPairSigner();
    const space = BigInt(getMintSize());
    const rent = await rpc.getMinimumBalanceForRentExemption(space).send();
    const holder = await walletTokenAccount(wallet.address, mint.address);
    await send(
        `create ${label}`,
        wallet,
        [
            getCreateAccountInstruction({
                payer: wallet,
                newAccount: mint,
                lamports: rent,
                space,
                programAddress: TOKEN_PROGRAM_ADDRESS,
            }),
            getInitializeMint2Instruction({ mint: mint.address, decimals, mintAuthority: wallet.address }),
            await getCreateAssociatedTokenIdempotentInstructionAsync({
                payer: wallet,
                owner: wallet.address,
                mint: mint.address,
            }),
            getMintToInstruction({ mint: mint.address, token: holder, mintAuthority: wallet, amount }),
        ],
        [mint],
    );
    info(`${label}: ${mint.address}`);
    return mint.address;
}

async function main() {
    const metadataUri = process.argv[2] ?? process.env.METADATA_URI;
    if (!metadataUri) {
        fail('pass the URL of your published metadata.json, e.g. `node deploy-devnet.ts https://…/metadata.json`');
    }
    const wallet = await loadWallet();
    const programId = address(process.env.PROGRAM_ID ?? DEFAULT_PROGRAM_ID);
    console.log(`Pipeline router ${pipeline.PIPELINE_PROGRAM_ADDRESS} on ${rpcUrl}`);
    console.log(`Strategy ${programId}, wallet ${wallet.address}`);

    step('1. Checking the program');
    const program = await pipeline.inspectProgram(rpc, programId, { hashExecutable: false });
    if (!program.deployed) {
        fail(`nothing is deployed at ${programId}. Deploy it first (see the README).`);
    }
    if (program.upgradeAuthority !== wallet.address) {
        fail(
            `registration must be signed by the program's upgrade authority (${program.upgradeAuthority ?? 'none: the program is frozen'}), not ${wallet.address}. Set KEYPAIR.`,
        );
    }
    info(`deployed (${program.loader} loader), upgrade authority ${program.upgradeAuthority}`);

    step('2. Checking metadata');
    const local = pipeline.parseStrategyMetadata(
        JSON.parse(readFileSync(new URL('../metadata.json', import.meta.url), 'utf8')),
    );
    if (!local.ok) {
        fail(`metadata.json is invalid:\n   ${local.errors.join('\n   ')}`);
    }
    for (const warning of local.warnings) info(`warning: ${warning}`);
    info(`metadata.json is valid: "${local.value.name}"`);
    const published = await pipeline.fetchStrategyMetadata(metadataUri);
    if (published.ok) {
        info(`${metadataUri} is valid`);
    } else {
        info(`warning: ${metadataUri} can't be used yet (${published.reason}: ${published.errors.join('; ')})`);
        info('desks will show your strategy without metadata until it can be fetched');
    }
    const metadata = local.value;
    const deposit = metadata.actions?.find((action) => action.name === 'deposit');
    if (!metadata.init || !deposit) {
        fail('metadata.json needs `init` and a `deposit` action for this script');
    }

    step('3. Registering the strategy');
    const config = pipeline.decodeConfig((await accountData(await pipeline.configPda())) ?? new Uint8Array());
    if (!config) {
        fail('the Pipeline router has no config on this cluster');
    }
    const strategyData = await accountData(await pipeline.strategyPda(programId));
    if (!strategyData) {
        info(`registering, which costs the listing fee: ${Number(config.listingFeeLamports) / 1e9} SOL`);
        await send('register_strategy', wallet, [
            await pipeline.registerStrategy({
                payer: wallet.address,
                registrar: wallet.address,
                strategyProgram: programId,
                listingFeeRecipient: config.listingFeeRecipient,
                metadataUri,
                maxListingFeeLamports: config.listingFeeLamports,
            }),
        ]);
    } else if (pipeline.decodeStrategy(strategyData)?.metadataUri !== metadataUri) {
        info('already registered; pointing it at the new metadata URI');
        await send('update_metadata', wallet, [
            await pipeline.updateMetadata({ registrar: wallet.address, strategyProgram: programId, metadataUri }),
        ]);
    } else {
        info('already registered with this metadata URI');
    }

    step('4. Creating two test tokens');
    const mintA = await createTestToken(wallet, 'token A', 6, MINTED);
    const mintB = await createTestToken(wallet, 'token B', 9, MINTED);

    step('5. Creating the market and recording it with the router');
    // `init` in metadata.json says how to create a pool: the market state address, the
    // init data and the accounts. A desk does exactly this to show a "create market" form.
    const resolved = await pipeline.resolveInit({
        init: metadata.init,
        strategyProgram: programId,
        creator: wallet.address,
        args: { fee_bps: POOL_FEE_BPS },
        inputs: { mint_a: mintA, mint_b: mintB },
        tokenPrograms: { [mintA]: TOKEN_PROGRAM_ADDRESS, [mintB]: TOKEN_PROGRAM_ADDRESS },
    });
    // create_market, then init_fee_account for each mint, so both tokens can be sold.
    const listing = await pipeline.createAndListMarketInstructions({
        creator: wallet.address,
        strategyProgram: programId,
        marketState: resolved.marketState,
        marketStateSigns: resolved.marketStateSigns,
        initData: resolved.initData,
        maxCreatorLamports: MAX_CREATOR_LAMPORTS,
        strategyAccounts: resolved.strategyAccounts,
        mints: [
            { mint: mintA, tokenProgram: TOKEN_PROGRAM_ADDRESS },
            { mint: mintB, tokenProgram: TOKEN_PROGRAM_ADDRESS },
        ],
    });
    await send('create_market + init_fee_account x2', wallet, listing);
    info(`market ${resolved.market} (${explorer('address', resolved.market)})`);
    info(`pool   ${resolved.marketState}`);

    step('6. Adding liquidity with the deposit action');
    const marketAccount = pipeline.decodeMarket((await accountData(resolved.market)) ?? new Uint8Array());
    if (!marketAccount) {
        fail(`the router has no market at ${resolved.market}`);
    }
    const view = pipeline.marketViewFromMarket(resolved.market, marketAccount, [
        TOKEN_PROGRAM_ADDRESS,
        TOKEN_PROGRAM_ADDRESS,
    ]);
    const depositInstruction = await pipeline.buildActionInstruction({
        action: deposit,
        market: view,
        wallet: wallet.address,
        args: { amount_a: DEPOSIT_A, amount_b: DEPOSIT_B },
    });
    await send('deposit', wallet, [depositInstruction]);
    info(`deposited ${DEPOSIT_A} of token A and ${DEPOSIT_B} of token B`);

    step('7. Quoting and swapping through the router');
    const trade = {
        mode: pipeline.TradeMode.ExactIn,
        inputIndex: 0,
        outputIndex: 1,
        amount: SWAP_AMOUNT,
        trader: wallet.address,
        extra: new Uint8Array(),
    };
    // Simulate against `confirmed` state, like every read here: the market was created
    // moments ago and may not be finalized yet.
    const simulation = { commitment: 'confirmed' } as const;
    const strategyAccounts = await pipeline.resolveAccounts(rpc, view, trade, wallet.address, simulation);
    const { quote, unitsConsumed } = await pipeline.quoteStrategy(
        rpc,
        view,
        trade,
        strategyAccounts,
        wallet.address,
        simulation,
    );
    info(`quote: ${SWAP_AMOUNT} of token A -> ${quote.amountOut} of token B (${unitsConsumed} compute units)`);
    const traderA = await walletTokenAccount(wallet.address, mintA);
    const traderB = await walletTokenAccount(wallet.address, mintB);
    const before = await tokenBalance(traderB);
    await send('swap', wallet, [
        await pipeline.swap(
            {
                trader: wallet.address,
                strategyProgram: programId,
                marketState: resolved.marketState,
                traderInput: traderA,
                traderOutput: traderB,
                inputVault: view.vaults[0]!,
                inputMint: mintA,
                outputMint: mintB,
                inputTokenProgram: TOKEN_PROGRAM_ADDRESS,
                outputTokenProgram: TOKEN_PROGRAM_ADDRESS,
                strategyAccounts,
            },
            {
                mode: pipeline.TradeMode.ExactIn,
                amountIn: SWAP_AMOUNT,
                amountOut: 0n,
                minAmountOut: quote.amountOut,
                integratorFeeBps: 0,
                extra: new Uint8Array(),
            },
        ),
    ]);
    const received = (await tokenBalance(traderB)) - before;
    info(`received ${received} of token B; Pipeline's fee was ${pipeline.pipelineFee(SWAP_AMOUNT)} of token A`);
    if (received !== quote.amountOut) {
        fail(`the swap delivered ${received}, but the quote was ${quote.amountOut}`);
    }

    step('Done. Your market is live on Pipeline.');
    info('Check it the way integrators will, and publish the report:');
    info('');
    info(`   pipeline-check --url ${checkUrl} --market ${resolved.market}`);
    info(`   pipeline-check --url ${checkUrl} --market ${resolved.market} --json > report.json`);
    info('   shasum -a 256 report.json');
    info('');
    info('Publish report.json, then add its URL and SHA-256 to metadata.json under "conformance".');
}

main().catch((error) => {
    console.error(error);
    process.exit(1);
});
