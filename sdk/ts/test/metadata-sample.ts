// A complete strategy metadata document shared by the tests.

/** The guide's constant-product example, extended with the optional fields. */
export function sampleMetadata(): Record<string, any> {
    return {
        schema: 'pipeline-strategy-metadata/1',
        name: 'Constant product',
        description: 'x*y=k pool with a configurable fee.',
        image: 'https://example.com/logo.png',
        category: 'amm',
        tags: ['cpmm', 'fees'],
        links: {
            website: 'https://example.com',
            source: 'https://github.com/example/cp',
            docs: 'ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi/docs.html',
            idl: 'ar://abc123/idl.json',
            twitter: 'https://x.com/example',
            github: 'https://github.com/example',
            discord: 'https://discord.gg/example',
            telegram: 'https://t.me/example',
        },
        source: { repository: 'https://github.com/example/cp', commit: 'dac82fef2b88', path: 'programs/cp' },
        init: {
            description: 'Creates a pool for two mints.',
            idl_type: 'InitParams',
            market_state: 'pda:strategy:str:pool,key:input:mint_a,key:input:mint_b,arg:fee_bps',
            args: [{ name: 'fee_bps', type: 'u16', label: 'Pool fee (basis points)' }],
            accounts: [
                { name: 'mint_a', source: 'input:mint_a' },
                { name: 'mint_b', source: 'input:mint_b' },
                { name: 'vault_a', source: 'ata:market_state,input:mint_a', writable: true },
                { name: 'vault_b', source: 'ata:market_state,input:mint_b', writable: true },
                { name: 'token_program_a', source: 'token_program_of:input:mint_a' },
                { name: 'token_program_b', source: 'token_program_of:input:mint_b' },
                { name: 'associated_token_program', source: 'associated_token_program' },
            ],
        },
        actions: [
            {
                name: 'deposit',
                label: 'Add liquidity',
                kind: 'add_liquidity',
                discriminator: 'f223c68952e1f2b6',
                args: [
                    { name: 'amount_a', type: 'u64', label: 'Amount of token A', decimals_of: 'mint:0' },
                    { name: 'amount_b', type: 'u64', label: 'Amount of token B', decimals_of: 'mint:1' },
                ],
                accounts: [
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
                ],
            },
        ],
        conformance: {
            report: 'https://example.com/report.json',
            sha256: '9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08',
        },
        security: {
            contact: 'security@example.com',
            policy: 'https://example.com/security',
            audits: [{ by: 'Example Audits', url: 'https://example.com/audit.pdf', date: '2026-10-01' }],
        },
    };
}
