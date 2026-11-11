// Read-only trust facts about strategies, markets and mints, and machine-readable flags
// for filtering on them. Facts only: no scores and no safe/unsafe judgement. Pipeline
// admits any strategy; front ends decide what to show and filter.

import {
    getAddressDecoder,
    getBase64Encoder,
    type Address,
    type GetAccountInfoApi,
    type GetEpochInfoApi,
    type Rpc,
} from '@solana/kit';

import { decodeMarket, decodeStrategy, marketMints, type Market, type Strategy } from './codecs.js';
import {
    BPF_LOADER_UPGRADEABLE_PROGRAM_ADDRESS,
    PIPELINE_PROGRAM_ADDRESS,
    TOKEN_2022_PROGRAM_ADDRESS,
    TOKEN_PROGRAM_ADDRESS,
} from './constants.js';
import { boundedFetch, parseJsonBody, type FetchLike } from './http.js';
import {
    fetchStrategyMetadata,
    resolveMetadataUri,
    type FetchMetadataOptions,
    type MetadataGateways,
    type StrategyMetadata,
} from './metadata.js';
import { strategyPda } from './pda.js';

// ---------------------------------------------------------------------------------------
// Flags
// ---------------------------------------------------------------------------------------

/**
 * Every flag, with its exact meaning. Flag names are stable: new flags may be added, but
 * existing ones keep their meaning.
 */
export const FLAG_DESCRIPTIONS = {
    // Strategy program
    upgradeable:
        'The strategy program can be upgraded: it has an upgrade authority (loader v3) or is not finalized (loader v4).',
    frozen: 'The strategy program can no longer be upgraded: no upgrade authority (loader v3), finalized (loader v4), or a non-upgradeable loader.',
    'program-missing': 'No executable code at the strategy program address (never deployed, closed, or retracted).',
    'verified-build':
        "OtterSec's verified-builds API reports the program as built from public source, and its hash matches the deployed code. Mainnet only; needs the SDK to hash the program (the default).",
    'unverified-build':
        "OtterSec's verified-builds API answered but does not report a verified build of the deployed code. Not set when the API could not be reached, off mainnet, or when the program wasn't hashed.",
    // Registry and metadata
    unregistered: 'The program has no Strategy account in the router.',
    'metadata-missing': "The strategy's metadata URI is empty.",
    'metadata-unreachable':
        'The metadata URI is unsupported, or could not be fetched (network error, timeout, HTTP error, too large).',
    'metadata-invalid': 'The metadata document was fetched but is not valid strategy metadata version 1.',
    'conformance-report':
        'Valid metadata links a pipeline-check conformance report for the deployed code: its program id and executable hash match.',
    'conformance-stale':
        'Valid metadata links a pipeline-check conformance report for other code: its program id or executable hash differs from what is deployed.',
    // Hosted checks (set by the hosted API, which runs pipeline-check on every market)
    'checks-passed':
        "The hosted checks' latest run against the strategy's current code had no failures (warnings and skips allowed).",
    'checks-failed': "The hosted checks' latest run against the strategy's current code had at least one failure.",
    'checks-pending':
        "No hosted checks result for the strategy's current code yet: never run, or the strategy changed since the last run.",
    // Mints
    'mint-unreadable': 'A market mint account is missing or is not an SPL Token or Token-2022 mint.',
    'token-2022': 'A mint is owned by Token-2022.',
    'mint-authority': 'A mint has a mint authority, so its supply can grow.',
    'freeze-authority':
        'A mint has a freeze authority, which can freeze token accounts (including market vaults and fee accounts).',
    'transfer-hook': 'A mint has a transfer hook program set: every transfer calls it.',
    'transfer-hook-authority': "A mint's transfer hook has an authority, which can set or change the hook program.",
    'transfer-fee': 'A mint charges a non-zero transfer fee now, or from a scheduled epoch.',
    'transfer-fee-authority': "A mint's transfer fee has an authority, which can set or change the fee.",
    'permanent-delegate': 'A mint has a permanent delegate, which can transfer or burn tokens from any account.',
    'non-transferable': 'A mint is non-transferable.',
    'default-frozen': 'New token accounts for a mint start frozen (DefaultAccountState).',
    pausable: 'A mint is pausable: an authority can pause minting, burning and transfers.',
    paused: 'A mint is currently paused.',
    'closable-mint': 'A mint has a close authority.',
    'interest-bearing': 'A mint is interest-bearing: its UI amount differs from the raw amount.',
    'scaled-ui-amount': 'A mint scales its UI amount by a multiplier an authority can change.',
    'confidential-transfers': 'A mint supports confidential transfers.',
    'permissioned-burn': 'Burning a mint requires an authority’s approval.',
    'unknown-extension': 'A mint has a Token-2022 extension this SDK does not decode.',
} as const;

export type Flag = keyof typeof FLAG_DESCRIPTIONS;

/** All flags, in a stable order. */
export const FLAGS = Object.keys(FLAG_DESCRIPTIONS) as Flag[];

function orderedFlags(flags: Iterable<Flag>): Flag[] {
    const set = new Set(flags);
    return FLAGS.filter((flag) => set.has(flag));
}

/**
 * Keeps the items that have every `require` flag and none of the `exclude` flags. Works on
 * anything with `flags`: market, strategy or mint inspections.
 */
export function filterMarkets<T extends { flags: readonly Flag[] }>(
    items: readonly T[],
    filter: { exclude?: readonly Flag[]; require?: readonly Flag[] },
): T[] {
    return items.filter(
        (item) =>
            (filter.require ?? []).every((flag) => item.flags.includes(flag)) &&
            !(filter.exclude ?? []).some((flag) => item.flags.includes(flag)),
    );
}

// ---------------------------------------------------------------------------------------
// Account reading
// ---------------------------------------------------------------------------------------

export type InspectRpc = Rpc<GetAccountInfoApi & GetEpochInfoApi>;

type RawAccount = { owner: Address; executable: boolean; data: Uint8Array };

const base64 = getBase64Encoder();
const addressDecoder = getAddressDecoder();

async function getAccount(
    rpc: Rpc<GetAccountInfoApi>,
    address: Address,
    dataSlice?: { offset: number; length: number },
): Promise<RawAccount | null> {
    const { value } = await rpc
        .getAccountInfo(address, { encoding: 'base64', ...(dataSlice ? { dataSlice } : {}) })
        .send();
    if (!value) {
        return null;
    }
    return { owner: value.owner, executable: value.executable, data: base64.encode(value.data[0]) as Uint8Array };
}

function view(data: Uint8Array): DataView {
    return new DataView(data.buffer, data.byteOffset, data.byteLength);
}

function readAddress(data: Uint8Array, offset: number): Address {
    return addressDecoder.decode(data.subarray(offset, offset + 32));
}

/** Token-2022's `OptionalNonZeroPubkey`: all zeros means none. */
function optionalAddress(data: Uint8Array, offset: number): Address | null {
    const bytes = data.subarray(offset, offset + 32);
    return bytes.every((b) => b === 0) ? null : readAddress(data, offset);
}

/** SPL Token's `COption<Pubkey>`: a u32 tag, then 32 bytes. */
function cOptionAddress(data: Uint8Array, offset: number): Address | null {
    return view(data).getUint32(offset, true) === 0 ? null : readAddress(data, offset + 4);
}

async function sha256Hex(bytes: Uint8Array): Promise<string> {
    const digest = new Uint8Array(await crypto.subtle.digest('SHA-256', bytes as Uint8Array<ArrayBuffer>));
    return Array.from(digest, (b) => b.toString(16).padStart(2, '0')).join('');
}

/** `bytes` without trailing zero bytes, as `solana-verify` hashes them. */
function trimTrailingZeros(bytes: Uint8Array): Uint8Array {
    let end = bytes.length;
    while (end > 0 && bytes[end - 1] === 0) {
        end--;
    }
    return bytes.subarray(0, end);
}

// ---------------------------------------------------------------------------------------
// Programs
// ---------------------------------------------------------------------------------------

export const LOADER_ADDRESSES = {
    v1: 'BPFLoader1111111111111111111111111111111111' as Address,
    v2: 'BPFLoader2111111111111111111111111111111111' as Address,
    upgradeable: BPF_LOADER_UPGRADEABLE_PROGRAM_ADDRESS,
    v4: 'LoaderV411111111111111111111111111111111111' as Address,
    native: 'NativeLoader1111111111111111111111111111111' as Address,
} as const;

export type ProgramLoader = 'upgradeable' | 'v4' | 'v2' | 'v1' | 'native' | 'other' | 'none';

/** Bytes before the ELF in a loader-v3 ProgramData account. */
export const PROGRAMDATA_METADATA_LEN = 45;
/** Bytes before the ELF in a loader-v4 program account. */
export const LOADER_V4_METADATA_LEN = 48;

export type ProgramInspection = {
    programId: Address;
    /** `none` when there is no account at the address. */
    loader: ProgramLoader;
    /** The loader (owner) address, or `null` with no account. */
    loaderAddress: Address | null;
    /** Executable code is deployed. */
    deployed: boolean;
    upgradeable: boolean;
    /** `null` when the program is frozen (or for loaders without one). */
    upgradeAuthority: Address | null;
    /** Loader v3 only. */
    programDataAddress: Address | null;
    /** Slot of the last deployment or upgrade (loaders v3 and v4). */
    lastDeploySlot: bigint | null;
    /** Loader v4 only. */
    /** `unknown` is a status number this SDK doesn't know; it sets neither program flag. */
    loaderV4Status: 'retracted' | 'deployed' | 'finalized' | 'unknown' | null;
    /**
     * Lowercase hex SHA-256 of the program's ELF with trailing zero bytes removed. For
     * loader v3 the ELF region is the ProgramData account after its 45-byte header; for
     * loaders v1 and v2 it is the whole program account. This equals
     * `solana-verify get-program-hash` and OtterSec's `on_chain_hash`. For loader v4
     * (which `solana-verify` doesn't support) the program account after its 48-byte
     * header, trimmed the same way. `null` when not computed or not deployed.
     */
    executableSha256: string | null;
    flags: Flag[];
};

export type InspectProgramOptions = {
    /**
     * Download the program's code to compute {@link ProgramInspection.executableSha256}.
     * Defaults to `true`; ProgramData accounts can be several megabytes.
     */
    hashExecutable?: boolean;
};

/** Loader, upgradeability, upgrade authority, last deploy slot and code hash of a program. */
export async function inspectProgram(
    rpc: Rpc<GetAccountInfoApi>,
    programId: Address,
    options: InspectProgramOptions = {},
): Promise<ProgramInspection> {
    const hash = options.hashExecutable ?? true;
    const result: ProgramInspection = {
        programId,
        loader: 'none',
        loaderAddress: null,
        deployed: false,
        upgradeable: false,
        upgradeAuthority: null,
        programDataAddress: null,
        lastDeploySlot: null,
        loaderV4Status: null,
        executableSha256: null,
        flags: [],
    };
    const account = await getAccount(rpc, programId);
    if (account) {
        result.loaderAddress = account.owner;
        switch (account.owner) {
            case LOADER_ADDRESSES.upgradeable:
                result.loader = 'upgradeable';
                await inspectUpgradeable(rpc, account, result, hash);
                break;
            case LOADER_ADDRESSES.v4:
                result.loader = 'v4';
                await inspectLoaderV4(account, result, hash);
                break;
            case LOADER_ADDRESSES.v1:
            case LOADER_ADDRESSES.v2:
                result.loader = account.owner === LOADER_ADDRESSES.v1 ? 'v1' : 'v2';
                result.deployed = account.executable && account.data.length > 0;
                if (hash && result.deployed) {
                    result.executableSha256 = await sha256Hex(trimTrailingZeros(account.data));
                }
                break;
            case LOADER_ADDRESSES.native:
                result.loader = 'native';
                result.deployed = account.executable;
                break;
            default:
                result.loader = 'other';
                result.deployed = account.executable;
        }
    }
    const flags: Flag[] = [];
    if (!result.deployed) {
        flags.push('program-missing');
    } else if (result.upgradeable) {
        flags.push('upgradeable');
    } else if (result.loader !== 'native' && result.loader !== 'other' && result.loaderV4Status !== 'unknown') {
        flags.push('frozen');
    }
    result.flags = orderedFlags(flags);
    return result;
}

async function inspectUpgradeable(
    rpc: Rpc<GetAccountInfoApi>,
    account: RawAccount,
    result: ProgramInspection,
    hash: boolean,
): Promise<void> {
    // UpgradeableLoaderState::Program { programdata_address }: u32 tag 2, then 32 bytes.
    if (account.data.length < 36 || view(account.data).getUint32(0, true) !== 2) {
        return;
    }
    const programData = readAddress(account.data, 4);
    result.programDataAddress = programData;
    const data = await getAccount(rpc, programData, hash ? undefined : { offset: 0, length: PROGRAMDATA_METADATA_LEN });
    // UpgradeableLoaderState::ProgramData { slot: u64, upgrade_authority: Option<Pubkey> }.
    if (
        !data ||
        data.owner !== LOADER_ADDRESSES.upgradeable ||
        data.data.length < PROGRAMDATA_METADATA_LEN ||
        view(data.data).getUint32(0, true) !== 3
    ) {
        return;
    }
    result.deployed = account.executable;
    result.lastDeploySlot = view(data.data).getBigUint64(4, true);
    result.upgradeAuthority = data.data[12] === 1 ? readAddress(data.data, 13) : null;
    result.upgradeable = result.upgradeAuthority !== null;
    if (hash) {
        result.executableSha256 = await sha256Hex(trimTrailingZeros(data.data.subarray(PROGRAMDATA_METADATA_LEN)));
    }
}

async function inspectLoaderV4(account: RawAccount, result: ProgramInspection, hash: boolean): Promise<void> {
    // LoaderV4State { slot: u64, authority_address_or_next_version: Pubkey, status: u64 }.
    if (account.data.length < LOADER_V4_METADATA_LEN) {
        return;
    }
    const status = view(account.data).getBigUint64(40, true);
    result.loaderV4Status =
        status === 0n ? 'retracted' : status === 1n ? 'deployed' : status === 2n ? 'finalized' : 'unknown';
    result.lastDeploySlot = view(account.data).getBigUint64(0, true);
    result.deployed = result.loaderV4Status !== 'retracted';
    if (result.loaderV4Status === 'retracted' || result.loaderV4Status === 'deployed') {
        result.upgradeable = true;
        result.upgradeAuthority = readAddress(account.data, 8);
    }
    if (hash && result.deployed) {
        result.executableSha256 = await sha256Hex(trimTrailingZeros(account.data.subarray(LOADER_V4_METADATA_LEN)));
    }
}

// ---------------------------------------------------------------------------------------
// Verified builds (OtterSec)
// ---------------------------------------------------------------------------------------

export const DEFAULT_VERIFY_API_URL = 'https://verify.osec.io';

export type VerifiedBuild = {
    /**
     * `unknown` when the API could not be reached or answered unexpectedly;
     * `not-applicable` when the program is not on mainnet (see {@link Cluster}).
     */
    status: 'verified' | 'unverified' | 'unknown' | 'not-applicable';
    verified: boolean;
    repository: string | null;
    commit: string | null;
    /** OtterSec's hash of the deployed code (as {@link ProgramInspection.executableSha256}). */
    onchainHash: string | null;
    /** OtterSec's hash of the build from source. */
    executableHash: string | null;
    /** ISO timestamp, without a time zone, as the API returns it. */
    lastVerifiedAt: string | null;
    message: string | null;
    /** Why the status is `unknown`. */
    error?: string;
};

export type VerifiedBuildOptions = {
    fetch?: FetchLike;
    /** Defaults to `https://verify.osec.io`. */
    apiUrl?: string;
    /** Defaults to 10 seconds. */
    timeoutMs?: number;
    signal?: AbortSignal;
};

function noVerifiedBuild(status: 'unknown' | 'not-applicable'): VerifiedBuild {
    return {
        status,
        verified: false,
        repository: null,
        commit: null,
        onchainHash: null,
        executableHash: null,
        lastVerifiedAt: null,
        message: null,
    };
}

function apiString(value: unknown): string | null {
    // The API reports a missing commit as the string "None", and other gaps as "".
    return typeof value === 'string' && value !== '' && value !== 'None' ? value : null;
}

/**
 * The program's status in OtterSec's verified-builds API
 * (`GET <apiUrl>/status/<programId>`). OtterSec only covers mainnet programs: a devnet
 * address is looked up as if it were on mainnet. Never throws: network or format errors
 * give status `unknown`.
 */
export async function fetchVerifiedBuild(
    programId: Address,
    options: VerifiedBuildOptions = {},
): Promise<VerifiedBuild> {
    const unknown = (error: string): VerifiedBuild => ({ ...noVerifiedBuild('unknown'), error });
    const base = (options.apiUrl ?? DEFAULT_VERIFY_API_URL).replace(/\/+$/, '');
    const response = await boundedFetch(`${base}/status/${programId}`, {
        fetch: options.fetch,
        maxBytes: 64 * 1024,
        timeoutMs: options.timeoutMs ?? 10_000,
        signal: options.signal,
    });
    if (!response.ok) {
        return unknown(response.message);
    }
    const json = parseJsonBody(response.body);
    if (json === null || typeof json !== 'object' || typeof (json as any).is_verified !== 'boolean') {
        return unknown('unexpected response');
    }
    const body = json as Record<string, unknown>;
    const verified = body.is_verified === true;
    return {
        status: verified ? 'verified' : 'unverified',
        verified,
        repository: apiString(body.repo_url),
        commit: apiString(body.commit),
        onchainHash: apiString(body.on_chain_hash),
        executableHash: apiString(body.executable_hash),
        lastVerifiedAt: apiString(body.last_verified_at),
        message: apiString(body.message),
    };
}

// ---------------------------------------------------------------------------------------
// Mints
// ---------------------------------------------------------------------------------------

/** Token-2022 extension type numbers (`ExtensionType`). */
export const EXTENSION_TYPES = {
    TransferFeeConfig: 1,
    MintCloseAuthority: 3,
    ConfidentialTransferMint: 4,
    DefaultAccountState: 6,
    NonTransferable: 9,
    InterestBearingConfig: 10,
    PermanentDelegate: 12,
    TransferHook: 14,
    ConfidentialTransferFeeConfig: 16,
    MetadataPointer: 18,
    TokenMetadata: 19,
    GroupPointer: 20,
    TokenGroup: 21,
    GroupMemberPointer: 22,
    TokenGroupMember: 23,
    ConfidentialMintBurn: 24,
    ScaledUiAmount: 25,
    Pausable: 26,
    PermissionedBurn: 28,
} as const;

export type TransferFee = { epoch: bigint; maximumFee: bigint; basisPoints: number };

/** Decoded Token-2022 mint extensions. Absent keys mean the extension is absent. */
export type MintExtensions = {
    transferFee?: {
        authority: Address | null;
        withdrawWithheldAuthority: Address | null;
        withheldAmount: bigint;
        older: TransferFee;
        newer: TransferFee;
        /** The fee in force at the inspected epoch. */
        current: TransferFee;
    };
    mintCloseAuthority?: { closeAuthority: Address | null };
    confidentialTransfer?: { authority: Address | null; autoApproveNewAccounts: boolean };
    confidentialTransferFee?: { authority: Address | null };
    confidentialMintBurn?: true;
    defaultAccountState?: { state: 'uninitialized' | 'initialized' | 'frozen' };
    nonTransferable?: true;
    interestBearing?: {
        rateAuthority: Address | null;
        initializationTimestamp: bigint;
        preUpdateAverageRateBps: number;
        lastUpdateTimestamp: bigint;
        currentRateBps: number;
    };
    permanentDelegate?: { delegate: Address | null };
    transferHook?: { authority: Address | null; programId: Address | null };
    metadataPointer?: { authority: Address | null; metadataAddress: Address | null };
    tokenMetadata?: {
        updateAuthority: Address | null;
        mint: Address;
        name: string;
        symbol: string;
        uri: string;
        additionalMetadata: [string, string][];
    };
    groupPointer?: { authority: Address | null; groupAddress: Address | null };
    tokenGroup?: { updateAuthority: Address | null; mint: Address; size: bigint; maxSize: bigint };
    groupMemberPointer?: { authority: Address | null; memberAddress: Address | null };
    tokenGroupMember?: { mint: Address; group: Address; memberNumber: bigint };
    scaledUiAmount?: {
        authority: Address | null;
        multiplier: number;
        newMultiplierEffectiveTimestamp: bigint;
        newMultiplier: number;
    };
    pausable?: { authority: Address | null; paused: boolean };
    permissionedBurn?: { authority: Address | null };
};

export type MintInspection = {
    mint: Address;
    tokenProgram: Address;
    decimals: number;
    supply: bigint;
    mintAuthority: Address | null;
    freezeAuthority: Address | null;
    extensions: MintExtensions;
    /** Every extension type number present, in account order. */
    extensionTypes: number[];
    /** Extension types this SDK does not decode (or could not decode). */
    unknownExtensions: number[];
    flags: Flag[];
};

export type InspectMintOptions = {
    /** Epoch for choosing the transfer fee in force. Fetched with `getEpochInfo` if needed. */
    epoch?: bigint;
};

const MINT_LEN = 82;
/** Token-2022 pads mints with extensions to the token account length before the account type. */
const ACCOUNT_TYPE_OFFSET = 165;
const ACCOUNT_TYPE_MINT = 1;

/** Token program, supply, authorities and decoded Token-2022 extensions of a mint. `null` if not a mint. */
export async function inspectMint(
    rpc: InspectRpc,
    mint: Address,
    options: InspectMintOptions = {},
): Promise<MintInspection | null> {
    const account = await getAccount(rpc, mint);
    if (!account) {
        return null;
    }
    const epoch = options.epoch ?? (hasTransferFee(account) ? (await rpc.getEpochInfo().send()).epoch : undefined);
    return decodeMint(mint, account.owner, account.data, epoch ?? 0n);
}

function hasTransferFee(account: RawAccount): boolean {
    return (
        account.owner === TOKEN_2022_PROGRAM_ADDRESS &&
        tlvEntries(account.data).some((entry) => entry.type === EXTENSION_TYPES.TransferFeeConfig)
    );
}

function tlvEntries(data: Uint8Array): { type: number; value: Uint8Array }[] {
    const entries: { type: number; value: Uint8Array }[] = [];
    if (data.length <= ACCOUNT_TYPE_OFFSET) {
        return entries;
    }
    const dv = view(data);
    let offset = ACCOUNT_TYPE_OFFSET + 1;
    while (offset + 4 <= data.length) {
        const type = dv.getUint16(offset, true);
        const length = dv.getUint16(offset + 2, true);
        if (type === 0) {
            break;
        }
        const value = data.subarray(offset + 4, offset + 4 + length);
        if (value.length < length) {
            break;
        }
        entries.push({ type, value });
        offset += 4 + length;
    }
    return entries;
}

/**
 * Decodes mint account bytes owned by `tokenProgram`. `epoch` picks the transfer fee in
 * force. `null` if the bytes are not an initialized mint.
 */
export function decodeMint(mint: Address, tokenProgram: Address, data: Uint8Array, epoch = 0n): MintInspection | null {
    if (tokenProgram !== TOKEN_PROGRAM_ADDRESS && tokenProgram !== TOKEN_2022_PROGRAM_ADDRESS) {
        return null;
    }
    if (data.length < MINT_LEN || data[45] !== 1) {
        return null;
    }
    if (data.length > MINT_LEN) {
        if (tokenProgram !== TOKEN_2022_PROGRAM_ADDRESS || data[ACCOUNT_TYPE_OFFSET] !== ACCOUNT_TYPE_MINT) {
            return null;
        }
    }
    const result: MintInspection = {
        mint,
        tokenProgram,
        mintAuthority: cOptionAddress(data, 0),
        supply: view(data).getBigUint64(36, true),
        decimals: data[44]!,
        freezeAuthority: cOptionAddress(data, 46),
        extensions: {},
        extensionTypes: [],
        unknownExtensions: [],
        flags: [],
    };
    for (const { type, value } of tlvEntries(data)) {
        result.extensionTypes.push(type);
        if (!decodeExtension(result.extensions, type, value, epoch)) {
            result.unknownExtensions.push(type);
        }
    }
    result.flags = mintFlags(result);
    return result;
}

const textDecoder = new TextDecoder();

function readTransferFee(value: Uint8Array, offset: number): TransferFee {
    const dv = view(value);
    return {
        epoch: dv.getBigUint64(offset, true),
        maximumFee: dv.getBigUint64(offset + 8, true),
        basisPoints: dv.getUint16(offset + 16, true),
    };
}

function decodeTokenMetadata(value: Uint8Array): MintExtensions['tokenMetadata'] {
    const dv = view(value);
    let offset = 64;
    const readString = (): string => {
        const length = dv.getUint32(offset, true);
        const bytes = value.subarray(offset + 4, offset + 4 + length);
        if (bytes.length !== length) {
            throw new RangeError('truncated string');
        }
        offset += 4 + length;
        return textDecoder.decode(bytes);
    };
    const name = readString();
    const symbol = readString();
    const uri = readString();
    const count = dv.getUint32(offset, true);
    offset += 4;
    const additionalMetadata: [string, string][] = [];
    for (let i = 0; i < count; i++) {
        additionalMetadata.push([readString(), readString()]);
    }
    return {
        updateAuthority: optionalAddress(value, 0),
        mint: readAddress(value, 32),
        name,
        symbol,
        uri,
        additionalMetadata,
    };
}

/** Decodes one extension into `out`. Returns `false` for unknown or malformed ones. */
function decodeExtension(out: MintExtensions, type: number, value: Uint8Array, epoch: bigint): boolean {
    const dv = view(value);
    const need = (length: number) => value.length >= length;
    try {
        switch (type) {
            case EXTENSION_TYPES.TransferFeeConfig: {
                if (!need(108)) return false;
                const older = readTransferFee(value, 72);
                const newer = readTransferFee(value, 90);
                out.transferFee = {
                    authority: optionalAddress(value, 0),
                    withdrawWithheldAuthority: optionalAddress(value, 32),
                    withheldAmount: dv.getBigUint64(64, true),
                    older,
                    newer,
                    current: epoch >= newer.epoch ? newer : older,
                };
                return true;
            }
            case EXTENSION_TYPES.MintCloseAuthority:
                if (!need(32)) return false;
                out.mintCloseAuthority = { closeAuthority: optionalAddress(value, 0) };
                return true;
            case EXTENSION_TYPES.ConfidentialTransferMint:
                if (!need(33)) return false;
                out.confidentialTransfer = {
                    authority: optionalAddress(value, 0),
                    autoApproveNewAccounts: value[32] === 1,
                };
                return true;
            case EXTENSION_TYPES.ConfidentialTransferFeeConfig:
                if (!need(32)) return false;
                out.confidentialTransferFee = { authority: optionalAddress(value, 0) };
                return true;
            case EXTENSION_TYPES.ConfidentialMintBurn:
                out.confidentialMintBurn = true;
                return true;
            case EXTENSION_TYPES.DefaultAccountState: {
                if (!need(1)) return false;
                const state = (['uninitialized', 'initialized', 'frozen'] as const)[value[0]!];
                if (!state) return false;
                out.defaultAccountState = { state };
                return true;
            }
            case EXTENSION_TYPES.NonTransferable:
                out.nonTransferable = true;
                return true;
            case EXTENSION_TYPES.InterestBearingConfig:
                if (!need(52)) return false;
                out.interestBearing = {
                    rateAuthority: optionalAddress(value, 0),
                    initializationTimestamp: dv.getBigInt64(32, true),
                    preUpdateAverageRateBps: dv.getInt16(40, true),
                    lastUpdateTimestamp: dv.getBigInt64(42, true),
                    currentRateBps: dv.getInt16(50, true),
                };
                return true;
            case EXTENSION_TYPES.PermanentDelegate:
                if (!need(32)) return false;
                out.permanentDelegate = { delegate: optionalAddress(value, 0) };
                return true;
            case EXTENSION_TYPES.TransferHook:
                if (!need(64)) return false;
                out.transferHook = { authority: optionalAddress(value, 0), programId: optionalAddress(value, 32) };
                return true;
            case EXTENSION_TYPES.MetadataPointer:
                if (!need(64)) return false;
                out.metadataPointer = {
                    authority: optionalAddress(value, 0),
                    metadataAddress: optionalAddress(value, 32),
                };
                return true;
            case EXTENSION_TYPES.TokenMetadata:
                if (!need(64)) return false;
                out.tokenMetadata = decodeTokenMetadata(value);
                return true;
            case EXTENSION_TYPES.GroupPointer:
                if (!need(64)) return false;
                out.groupPointer = { authority: optionalAddress(value, 0), groupAddress: optionalAddress(value, 32) };
                return true;
            case EXTENSION_TYPES.TokenGroup:
                if (!need(80)) return false;
                out.tokenGroup = {
                    updateAuthority: optionalAddress(value, 0),
                    mint: readAddress(value, 32),
                    size: dv.getBigUint64(64, true),
                    maxSize: dv.getBigUint64(72, true),
                };
                return true;
            case EXTENSION_TYPES.GroupMemberPointer:
                if (!need(64)) return false;
                out.groupMemberPointer = {
                    authority: optionalAddress(value, 0),
                    memberAddress: optionalAddress(value, 32),
                };
                return true;
            case EXTENSION_TYPES.TokenGroupMember:
                if (!need(72)) return false;
                out.tokenGroupMember = {
                    mint: readAddress(value, 0),
                    group: readAddress(value, 32),
                    memberNumber: dv.getBigUint64(64, true),
                };
                return true;
            case EXTENSION_TYPES.ScaledUiAmount:
                if (!need(56)) return false;
                out.scaledUiAmount = {
                    authority: optionalAddress(value, 0),
                    multiplier: dv.getFloat64(32, true),
                    newMultiplierEffectiveTimestamp: dv.getBigInt64(40, true),
                    newMultiplier: dv.getFloat64(48, true),
                };
                return true;
            case EXTENSION_TYPES.Pausable:
                if (!need(33)) return false;
                out.pausable = { authority: optionalAddress(value, 0), paused: value[32] === 1 };
                return true;
            case EXTENSION_TYPES.PermissionedBurn:
                if (!need(32)) return false;
                out.permissionedBurn = { authority: optionalAddress(value, 0) };
                return true;
            default:
                return false;
        }
    } catch {
        return false;
    }
}

function mintFlags(mint: MintInspection): Flag[] {
    const flags: Flag[] = [];
    const e = mint.extensions;
    if (mint.tokenProgram === TOKEN_2022_PROGRAM_ADDRESS) flags.push('token-2022');
    if (mint.mintAuthority) flags.push('mint-authority');
    if (mint.freezeAuthority) flags.push('freeze-authority');
    if (e.transferHook?.programId) flags.push('transfer-hook');
    if (e.transferHook?.authority) flags.push('transfer-hook-authority');
    // A fee in force now, or a non-zero fee scheduled for a later epoch. A fee that was
    // lowered to zero and is now in force doesn't count.
    const scheduled = e.transferFee && e.transferFee.newer.epoch > e.transferFee.current.epoch;
    if (
        e.transferFee &&
        (e.transferFee.current.basisPoints > 0 || (scheduled && e.transferFee.newer.basisPoints > 0))
    ) {
        flags.push('transfer-fee');
    }
    if (e.transferFee?.authority) flags.push('transfer-fee-authority');
    if (e.permanentDelegate?.delegate) flags.push('permanent-delegate');
    if (e.nonTransferable) flags.push('non-transferable');
    if (e.defaultAccountState?.state === 'frozen') flags.push('default-frozen');
    if (e.pausable && (e.pausable.authority || e.pausable.paused)) flags.push('pausable');
    if (e.pausable?.paused) flags.push('paused');
    if (e.mintCloseAuthority?.closeAuthority) flags.push('closable-mint');
    if (e.interestBearing) flags.push('interest-bearing');
    if (e.scaledUiAmount) flags.push('scaled-ui-amount');
    if (e.confidentialTransfer || e.confidentialMintBurn) flags.push('confidential-transfers');
    if (e.permissionedBurn) flags.push('permissioned-burn');
    if (mint.unknownExtensions.length > 0) flags.push('unknown-extension');
    return orderedFlags(flags);
}

// ---------------------------------------------------------------------------------------
// Conformance reports
// ---------------------------------------------------------------------------------------

/** The largest conformance report fetched. */
export const CONFORMANCE_REPORT_MAX_BYTES = 1024 * 1024;
/** The `schema` of a `pipeline-check` JSON report. */
export const CHECK_REPORT_SCHEMA = 'pipeline-check-report/1';

/**
 * Why a conformance report isn't `current`: `unreachable` (unsupported URL or failed
 * fetch), `sha256-mismatch` (the body isn't the report `conformance.sha256` pins),
 * `not-json`, `not-a-report` (no `pipeline-check-report/1` strategy section),
 * `code-not-hashed` (the deployed code's hash is unknown), `other-program` or `other-code`.
 */
export type ConformanceReason =
    | 'unreachable'
    | 'sha256-mismatch'
    | 'not-json'
    | 'not-a-report'
    | 'code-not-hashed'
    | 'other-program'
    | 'other-code';

/** Whether a strategy's self-published conformance report is about its deployed code. */
export type ConformanceFacts = {
    /** `current`: flag `conformance-report`; `stale`: `conformance-stale`; `unverified`: neither. */
    status: 'current' | 'stale' | 'unverified';
    reason: ConformanceReason | null;
    /** The URL fetched (gateway-resolved for `ipfs://` and `ar://`). */
    url: string | null;
    /** Lowercase hex SHA-256 of the report as fetched. */
    reportSha256: string | null;
    programId: string | null;
    executableSha256: string | null;
    /** The report's overall result (`pass`, `warn`, `fail`), as published. */
    result: string | null;
    /** Unix seconds when the report was produced, as published. */
    generatedAt: number | null;
    /** The fetch error, when `unreachable`. */
    error?: string;
};

function noConformance(reason: ConformanceReason, url: string | null, error: string): ConformanceFacts {
    return {
        status: 'unverified',
        reason,
        url,
        reportSha256: null,
        programId: null,
        executableSha256: null,
        result: null,
        generatedAt: null,
        error,
    };
}

/**
 * Checks a fetched conformance report against the deployed program. `pinnedSha256` is the
 * metadata's `conformance.sha256`, if any; `deployedSha256` is
 * {@link ProgramInspection.executableSha256}, if known. Never throws.
 */
export async function evaluateConformanceReport(
    body: Uint8Array,
    pinnedSha256: string | null,
    programId: Address,
    deployedSha256: string | null,
): Promise<ConformanceFacts> {
    const reportSha256 = await sha256Hex(body);
    const facts: ConformanceFacts = {
        status: 'unverified',
        reason: null,
        url: null,
        reportSha256,
        programId: null,
        executableSha256: null,
        result: null,
        generatedAt: null,
    };
    const withReason = (reason: ConformanceReason): ConformanceFacts => ({ ...facts, reason });
    if (pinnedSha256 !== null && pinnedSha256.toLowerCase() !== reportSha256) {
        return withReason('sha256-mismatch');
    }
    const report = parseJsonBody(body);
    if (report === undefined) {
        return withReason('not-json');
    }
    const json = report !== null && typeof report === 'object' ? (report as Record<string, any>) : {};
    const strategy = json.strategy !== null && typeof json.strategy === 'object' ? json.strategy : {};
    if (
        typeof strategy.program_id !== 'string' ||
        typeof strategy.executable_sha256 !== 'string' ||
        json.schema !== CHECK_REPORT_SCHEMA
    ) {
        return withReason('not-a-report');
    }
    facts.programId = strategy.program_id;
    facts.executableSha256 = strategy.executable_sha256;
    const result = json.summary !== null && typeof json.summary === 'object' ? json.summary.result : undefined;
    facts.result = typeof result === 'string' ? result : null;
    const generatedAt = json.generated_at;
    facts.generatedAt = Number.isSafeInteger(generatedAt) && generatedAt >= 0 ? generatedAt : null;
    if (strategy.program_id !== programId) {
        facts.status = 'stale';
        return withReason('other-program');
    }
    if (deployedSha256 === null) {
        return withReason('code-not-hashed');
    }
    if (strategy.executable_sha256.toLowerCase() !== deployedSha256.toLowerCase()) {
        facts.status = 'stale';
        return withReason('other-code');
    }
    facts.status = 'current';
    return facts;
}

export type FetchConformanceOptions = {
    fetch?: FetchLike;
    gateways?: MetadataGateways;
    /** Defaults to 1 MiB. */
    maxBytes?: number;
    /** Defaults to 10 seconds. */
    timeoutMs?: number;
    signal?: AbortSignal;
};

/**
 * Fetches a conformance report (`https://`, `ipfs://` or `ar://`) with the same rules as
 * metadata, and checks it with {@link evaluateConformanceReport}. Never throws.
 */
export async function fetchConformanceReport(
    report: string,
    pinnedSha256: string | null,
    programId: Address,
    deployedSha256: string | null,
    options: FetchConformanceOptions = {},
): Promise<ConformanceFacts> {
    const url = resolveMetadataUri(report, options.gateways);
    if (url === null) {
        return noConformance('unreachable', null, `unsupported URI "${report}"`);
    }
    const response = await boundedFetch(url, {
        fetch: options.fetch,
        maxBytes: options.maxBytes ?? CONFORMANCE_REPORT_MAX_BYTES,
        timeoutMs: options.timeoutMs ?? 10_000,
        signal: options.signal,
    });
    if (!response.ok) {
        return noConformance('unreachable', url, response.message);
    }
    const facts = await evaluateConformanceReport(response.body, pinnedSha256, programId, deployedSha256);
    return { ...facts, url };
}

// ---------------------------------------------------------------------------------------
// Strategies and markets
// ---------------------------------------------------------------------------------------

export type MetadataFacts =
    | { status: 'missing'; uri: '' }
    | { status: 'not-fetched'; uri: string }
    | { status: 'unreachable' | 'invalid'; uri: string; errors: string[] }
    | { status: 'valid'; uri: string; value: StrategyMetadata; warnings: string[] };

export type StrategyInspection = {
    programId: Address;
    /** The router's Strategy account address. */
    strategyAccount: Address;
    /** `null` when the program is not registered. */
    registration: Strategy | null;
    program: ProgramInspection;
    metadata: MetadataFacts;
    /** `null` when not requested. */
    verifiedBuild: VerifiedBuild | null;
    /**
     * The conformance report valid metadata links, checked against the deployed code.
     * `null` when there is none, or it was not fetched.
     */
    conformance: ConformanceFacts | null;
    flags: Flag[];
};

/** The cluster the RPC client talks to. */
export type Cluster = 'mainnet' | 'devnet' | 'testnet' | 'localnet';

export type InspectStrategyOptions = InspectProgramOptions & {
    /**
     * The RPC's cluster, if known. OtterSec's verified-builds API only covers mainnet, so
     * on any other cluster the lookup is skipped, `verifiedBuild.status` is
     * `not-applicable` and neither build flag is set. Unset means the lookup runs.
     */
    cluster?: Cluster;
    /** Shared by the metadata and verified-build requests unless overridden there. */
    fetch?: FetchLike;
    /** Fetch and validate the metadata document. Defaults to `true`. */
    metadata?: boolean | FetchMetadataOptions;
    /** Ask OtterSec's verified-builds API. Defaults to `true`. */
    verifiedBuild?: boolean | VerifiedBuildOptions;
    /**
     * Fetch the conformance report valid metadata links and check it against the deployed
     * code. Defaults to `true`. Without it neither conformance flag is set.
     */
    conformanceReport?: boolean | FetchConformanceOptions;
};

function enabled<T extends object>(option: boolean | T | undefined, fetch: FetchLike | undefined): T | null {
    if (option === false) {
        return null;
    }
    const base = option === true || option === undefined ? ({} as T) : option;
    return { fetch, ...base };
}

/**
 * A strategy's registration (registered_at, market_count, metadata_uri), its program
 * facts, its metadata document and its verified-build status, with flags.
 */
export async function inspectStrategy(
    rpc: Rpc<GetAccountInfoApi>,
    programId: Address,
    options: InspectStrategyOptions = {},
): Promise<StrategyInspection> {
    const strategyAccount = await strategyPda(programId);
    const metadataOptions = enabled<FetchMetadataOptions>(options.metadata, options.fetch);
    const verifyOptions = enabled<VerifiedBuildOptions>(options.verifiedBuild, options.fetch);
    const offMainnet = options.cluster !== undefined && options.cluster !== 'mainnet';
    const [raw, program, verifiedBuild] = await Promise.all([
        getAccount(rpc, strategyAccount),
        inspectProgram(rpc, programId, options),
        !verifyOptions
            ? Promise.resolve(null)
            : offMainnet
              ? Promise.resolve(noVerifiedBuild('not-applicable'))
              : fetchVerifiedBuild(programId, verifyOptions),
    ]);
    const registration = raw && raw.owner === PIPELINE_PROGRAM_ADDRESS ? decodeStrategy(raw.data) : null;

    let metadata: MetadataFacts;
    const uri = registration?.metadataUri ?? '';
    if (uri.trim() === '') {
        metadata = { status: 'missing', uri: '' };
    } else if (!metadataOptions) {
        metadata = { status: 'not-fetched', uri };
    } else {
        const fetched = await fetchStrategyMetadata(uri, metadataOptions);
        if (fetched.ok) {
            metadata = { status: 'valid', uri, value: fetched.value, warnings: fetched.warnings };
        } else {
            metadata = {
                status: fetched.reason === 'invalid' || fetched.reason === 'not-json' ? 'invalid' : 'unreachable',
                uri,
                errors: fetched.errors,
            };
        }
    }

    let conformance: ConformanceFacts | null = null;
    const conformanceOptions = enabled<FetchConformanceOptions>(options.conformanceReport, options.fetch);
    if (metadata.status === 'valid' && metadata.value.conformance && conformanceOptions) {
        const linked = metadata.value.conformance;
        conformance = await fetchConformanceReport(
            linked.report,
            linked.sha256 ?? null,
            programId,
            program.deployed ? program.executableSha256 : null,
            { gateways: metadataOptions?.gateways, ...conformanceOptions },
        );
    }

    const flags: Flag[] = [...program.flags];
    if (!registration) {
        flags.push('unregistered');
    } else if (metadata.status === 'missing') {
        flags.push('metadata-missing');
    }
    if (metadata.status === 'unreachable') flags.push('metadata-unreachable');
    if (metadata.status === 'invalid') flags.push('metadata-invalid');
    if (metadata.status === 'valid' && conformance?.status === 'current') flags.push('conformance-report');
    if (metadata.status === 'valid' && conformance?.status === 'stale') flags.push('conformance-stale');
    // Build flags need our own hash of the deployed code: OtterSec's record can be stale
    // (the program upgraded since), so its hash must equal what is deployed now. Without
    // a hash (`hashExecutable: false`) neither flag is set.
    const answered = verifiedBuild && (verifiedBuild.status === 'verified' || verifiedBuild.status === 'unverified');
    if (answered && program.deployed && program.executableSha256 !== null) {
        const matches = verifiedBuild.onchainHash === program.executableSha256;
        flags.push(verifiedBuild.verified && matches ? 'verified-build' : 'unverified-build');
    }
    return {
        programId,
        strategyAccount,
        registration,
        program,
        metadata,
        verifiedBuild,
        conformance,
        flags: orderedFlags(flags),
    };
}

export type MarketInspection = {
    address: Address;
    market: Market;
    mints: Address[];
    createdBy: Address;
    createdAt: bigint;
    strategy: StrategyInspection;
    /** One per mint, in market order; `null` for an unreadable mint. */
    mintInspections: (MintInspection | null)[];
    /** The strategy's flags and every mint's flags. */
    flags: Flag[];
};

export type InspectMarketOptions = InspectStrategyOptions &
    InspectMintOptions & {
        /** A strategy inspection to reuse, for example across many markets of one strategy. */
        strategy?: StrategyInspection;
    };

/**
 * A recorded market (mints, created_by, created_at), its strategy's inspection and each
 * mint's inspection, with the union of their flags. `null` if `market` is not a router
 * Market account.
 */
export async function inspectMarket(
    rpc: InspectRpc,
    market: Address,
    options: InspectMarketOptions = {},
): Promise<MarketInspection | null> {
    const raw = await getAccount(rpc, market);
    const decoded = raw && raw.owner === PIPELINE_PROGRAM_ADDRESS ? decodeMarket(raw.data) : null;
    if (!decoded) {
        return null;
    }
    const mints = marketMints(decoded);
    const [strategy, mintInspections] = await Promise.all([
        options.strategy && options.strategy.programId === decoded.strategyProgram
            ? Promise.resolve(options.strategy)
            : inspectStrategy(rpc, decoded.strategyProgram, options),
        Promise.all(mints.map((mint) => inspectMint(rpc, mint, options))),
    ]);
    const flags: Flag[] = [...strategy.flags];
    for (const mint of mintInspections) {
        if (mint) {
            flags.push(...mint.flags);
        } else {
            flags.push('mint-unreadable');
        }
    }
    return {
        address: market,
        market: decoded,
        mints,
        createdBy: decoded.createdBy,
        createdAt: decoded.createdAt,
        strategy,
        mintInspections,
        flags: orderedFlags(flags),
    };
}
