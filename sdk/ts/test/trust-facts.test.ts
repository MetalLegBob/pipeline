// The shared trust-facts vectors in `crates/pipeline-client/tests/fixtures/trust-facts.json`,
// which the Rust crate's tests check too, so the two implementations give the same flags
// for the same bytes. Regenerate the file with
// `cargo run -q -p pipeline-client --example ts_facts_fixtures` (see that example).

import { readFileSync } from 'node:fs';

import { getBase64Encoder, type Address } from '@solana/kit';
import { describe, expect, it } from 'vitest';

import { decodeMint, FLAG_DESCRIPTIONS, FLAGS, inspectProgram, parseStrategyMetadata } from '../src/index.js';

type FixtureAccount = { owner: string; executable: boolean; data: string } | null;

type TrustFacts = {
    flags: [string, string][];
    mints: { name: string; owner: string; epoch: string; data: string; expected: Record<string, unknown> | null }[];
    programs: {
        name: string;
        program_id: string;
        program: FixtureAccount;
        program_data: FixtureAccount;
        expected: Record<string, unknown>;
    }[];
    metadata: { name: string; document: unknown; ok: boolean; errors: string[]; warnings: string[] }[];
};

const fixtures: TrustFacts = JSON.parse(
    readFileSync(new URL('../../../crates/pipeline-client/tests/fixtures/trust-facts.json', import.meta.url), 'utf8'),
);

const base64 = getBase64Encoder();
const bytes = (data: string) => base64.encode(data) as Uint8Array;

describe('shared trust-facts vectors', () => {
    it('has the same flags and descriptions, in the same order', () => {
        expect(fixtures.flags).toEqual(FLAGS.map((flag) => [flag, FLAG_DESCRIPTIONS[flag]]));
    });

    for (const fixture of fixtures.mints) {
        it(`decodes mint ${fixture.name}`, () => {
            const decoded = decodeMint(
                '11111111111111111111111111111111' as Address,
                fixture.owner as Address,
                bytes(fixture.data),
                BigInt(fixture.epoch),
            );
            const actual = decoded && {
                flags: decoded.flags,
                decimals: decoded.decimals,
                supply: decoded.supply.toString(),
                mint_authority: decoded.mintAuthority,
                freeze_authority: decoded.freezeAuthority,
                extension_types: decoded.extensionTypes,
                unknown_extensions: decoded.unknownExtensions,
                transfer_fee_current_bps: decoded.extensions.transferFee?.current.basisPoints ?? null,
                token_metadata_name: decoded.extensions.tokenMetadata?.name ?? null,
            };
            expect(actual).toEqual(fixture.expected);
        });
    }

    for (const fixture of fixtures.programs) {
        it(`inspects program ${fixture.name}`, async () => {
            const accounts: Record<string, FixtureAccount> = { [fixture.program_id]: fixture.program };
            // The ProgramData address is whatever the program account points to.
            if (fixture.expected.program_data_address) {
                accounts[fixture.expected.program_data_address as string] = fixture.program_data;
            }
            const rpc = {
                getAccountInfo(address: string) {
                    const account = accounts[address] ?? null;
                    return {
                        send: async () => ({
                            context: { slot: 1n },
                            value: account && {
                                owner: account.owner,
                                executable: account.executable,
                                lamports: 1n,
                                space: 0n,
                                rentEpoch: 0n,
                                data: [account.data, 'base64'],
                            },
                        }),
                    };
                },
            } as any;
            const facts = await inspectProgram(rpc, fixture.program_id as Address);
            expect({
                loader: facts.loader,
                loader_address: facts.loaderAddress,
                deployed: facts.deployed,
                upgradeable: facts.upgradeable,
                upgrade_authority: facts.upgradeAuthority,
                program_data_address: facts.programDataAddress,
                last_deploy_slot: facts.lastDeploySlot === null ? null : facts.lastDeploySlot.toString(),
                loader_v4_status: facts.loaderV4Status,
                executable_sha256: facts.executableSha256,
                flags: facts.flags,
            }).toEqual(fixture.expected);
        });
    }

    for (const fixture of fixtures.metadata) {
        it(`validates metadata ${fixture.name}`, () => {
            const result = parseStrategyMetadata(fixture.document);
            expect(result.ok).toBe(fixture.ok);
            expect(result.ok ? [] : [...result.errors].sort()).toEqual(fixture.errors);
            expect(result.ok ? [...result.warnings].sort() : []).toEqual(fixture.warnings);
        });
    }
});
