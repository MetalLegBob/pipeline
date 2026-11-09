//! Reading programs, mints and token accounts out of a LiteSVM, and writing token
//! accounts into it directly (so checks can fund traders without a mint authority).

use litesvm::LiteSVM;
use pipeline_client::{self as client, pda, pubkey, Pubkey};
use sha2::{Digest, Sha256};
use solana_account::Account;
use spl_token_2022_interface::extension::default_account_state::DefaultAccountState;
use spl_token_2022_interface::extension::transfer_hook::TransferHook;
use spl_token_2022_interface::extension::{BaseStateWithExtensions, StateWithExtensions};

pub const LOADER_V1: Pubkey = pubkey!("BPFLoader1111111111111111111111111111111111");
pub const LOADER_V2: Pubkey = pubkey!("BPFLoader2111111111111111111111111111111111");
pub const LOADER_V3: Pubkey = client::BPF_LOADER_UPGRADEABLE_ID;
pub const LOADER_V4: Pubkey = pubkey!("LoaderV411111111111111111111111111111111111");
pub const NATIVE_LOADER: Pubkey = pubkey!("NativeLoader1111111111111111111111111111111");
pub const SYSVAR_OWNER: Pubkey = pubkey!("Sysvar1111111111111111111111111111111111111");
pub const COMPUTE_BUDGET_PROGRAM_ID: Pubkey =
    pubkey!("ComputeBudget111111111111111111111111111111");

/// Loader-v3 ProgramData header: 4-byte state tag, 8-byte deploy slot, 1-byte option
/// flag and a 32-byte upgrade authority. The ELF follows.
pub const PROGRAMDATA_HEADER_LEN: usize = 45;
/// Loader-v4 program header: 8-byte deploy slot, 32-byte authority, 8-byte status.
pub const LOADER_V4_HEADER_LEN: usize = 48;

/// SPL Token account layout offsets shared by both token programs.
const TOKEN_ACCOUNT_LEN: usize = 165;
const TOKEN_OWNER_OFFSET: usize = 32;
const TOKEN_AMOUNT_OFFSET: usize = 64;
const TOKEN_STATE_OFFSET: usize = 108;
const TOKEN_IS_NATIVE_OFFSET: usize = 109;
const MINT_LEN: usize = 82;
const MINT_DECIMALS_OFFSET: usize = 44;

/// Neutral facts about a deployed program.
#[derive(Clone, Debug)]
pub struct ProgramFacts {
    pub loader: &'static str,
    /// The executable as stored on chain, including any zero padding after the ELF.
    pub elf: Vec<u8>,
    /// `None` for frozen programs and loaders without upgrades.
    pub upgrade_authority: Option<Pubkey>,
    pub deploy_slot: Option<u64>,
}

pub fn program_facts(svm: &LiteSVM, program: &Pubkey) -> Option<ProgramFacts> {
    let account = svm.get_account(program)?;
    if !account.executable {
        return None;
    }
    if account.owner == LOADER_V3 {
        let data = svm.get_account(&pda::program_data(program))?.data;
        if data.len() < PROGRAMDATA_HEADER_LEN {
            return None;
        }
        let authority =
            (data[12] == 1).then(|| Pubkey::new_from_array(data[13..45].try_into().unwrap()));
        return Some(ProgramFacts {
            loader: "bpf-loader-upgradeable",
            elf: data[PROGRAMDATA_HEADER_LEN..].to_vec(),
            upgrade_authority: authority,
            deploy_slot: Some(u64::from_le_bytes(data[4..12].try_into().unwrap())),
        });
    }
    if account.owner == LOADER_V4 {
        let data = &account.data;
        if data.len() < LOADER_V4_HEADER_LEN {
            return None;
        }
        // Status 2 means finalized: the authority field then names the next version.
        let finalized = u64::from_le_bytes(data[40..48].try_into().unwrap()) == 2;
        return Some(ProgramFacts {
            loader: "loader-v4",
            elf: data[LOADER_V4_HEADER_LEN..].to_vec(),
            upgrade_authority: (!finalized)
                .then(|| Pubkey::new_from_array(data[8..40].try_into().unwrap())),
            deploy_slot: Some(u64::from_le_bytes(data[0..8].try_into().unwrap())),
        });
    }
    if account.owner == LOADER_V2 || account.owner == LOADER_V1 {
        return Some(ProgramFacts {
            loader: if account.owner == LOADER_V2 {
                "bpf-loader-2"
            } else {
                "bpf-loader-1"
            },
            elf: account.data,
            upgrade_authority: None,
            deploy_slot: None,
        });
    }
    None
}

/// SHA-256 of an executable with trailing zero bytes removed, as lowercase hex, and the
/// length hashed. This is the convention `solana-verify get-program-hash` uses, so the
/// result can be compared with a verified build of the same source.
pub fn executable_sha256(elf: &[u8]) -> (String, usize) {
    let len = elf.iter().rposition(|byte| *byte != 0).map_or(0, |i| i + 1);
    let digest = Sha256::digest(&elf[..len]);
    (hex(&digest), len)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn token_amount(svm: &LiteSVM, account: &Pubkey) -> Option<u64> {
    let data = svm.get_account(account)?.data;
    let bytes = data.get(TOKEN_AMOUNT_OFFSET..TOKEN_AMOUNT_OFFSET + 8)?;
    Some(u64::from_le_bytes(bytes.try_into().unwrap()))
}

pub fn token_owner(svm: &LiteSVM, account: &Pubkey) -> Option<Pubkey> {
    let data = svm.get_account(account)?.data;
    let bytes = data.get(TOKEN_OWNER_OFFSET..TOKEN_OWNER_OFFSET + 32)?;
    Some(Pubkey::new_from_array(bytes.try_into().unwrap()))
}

/// What the checks need to know about one of a market's mints.
#[derive(Clone, Debug)]
pub struct MintFacts {
    pub address: Pubkey,
    pub token_program: Pubkey,
    pub decimals: u8,
    pub mint_authority: Option<Pubkey>,
    pub freeze_authority: Option<Pubkey>,
    /// Token-2022 extension names, empty for SPL Token mints.
    pub extensions: Vec<String>,
    pub transfer_hook_program: Option<Pubkey>,
    pub default_frozen: bool,
    pub non_transferable: bool,
}

fn coption_key(bytes: &[u8]) -> Option<Pubkey> {
    (bytes[0] == 1).then(|| Pubkey::new_from_array(bytes[4..36].try_into().unwrap()))
}

pub fn mint_facts(svm: &LiteSVM, mint: &Pubkey) -> Result<MintFacts, String> {
    let account = svm
        .get_account(mint)
        .ok_or_else(|| format!("mint {mint} does not exist"))?;
    if account.owner != client::TOKEN_PROGRAM_ID && account.owner != client::TOKEN_2022_PROGRAM_ID {
        return Err(format!(
            "mint {mint} is owned by {}, not a token program",
            account.owner
        ));
    }
    let data = &account.data;
    if data.len() < MINT_LEN {
        return Err(format!("mint {mint} is too short to be a mint"));
    }
    let mut facts = MintFacts {
        address: *mint,
        token_program: account.owner,
        decimals: data[MINT_DECIMALS_OFFSET],
        mint_authority: coption_key(&data[0..36]),
        freeze_authority: coption_key(&data[46..82]),
        extensions: vec![],
        transfer_hook_program: None,
        default_frozen: false,
        non_transferable: false,
    };
    if account.owner == client::TOKEN_2022_PROGRAM_ID && data.len() > MINT_LEN {
        let state = StateWithExtensions::<spl_token_2022_interface::state::Mint>::unpack(data)
            .map_err(|e| format!("mint {mint} has malformed extensions: {e}"))?;
        let types = state
            .get_extension_types()
            .map_err(|e| format!("mint {mint} has malformed extensions: {e}"))?;
        facts.extensions = types.iter().map(|t| format!("{t:?}")).collect();
        if let Ok(hook) = state.get_extension::<TransferHook>() {
            facts.transfer_hook_program = Option::<Pubkey>::from(hook.program_id);
        }
        if let Ok(default) = state.get_extension::<DefaultAccountState>() {
            // AccountState::Frozen
            facts.default_frozen = default.state == 2;
        }
        facts.non_transferable = facts.extensions.iter().any(|e| e == "NonTransferable");
    }
    Ok(facts)
}

/// Writes `owner`'s associated token account for `mint` straight into the VM with
/// `amount` tokens, replacing any existing account. Token-2022 accounts carry the
/// extensions the mint requires; wrapped SOL gets the matching lamports.
pub fn write_token_account(
    svm: &mut LiteSVM,
    mint: &Pubkey,
    token_program: &Pubkey,
    owner: &Pubkey,
    amount: u64,
) -> Result<Pubkey, String> {
    let address = pda::associated_token(owner, mint, token_program);
    let mint_account = svm
        .get_account(mint)
        .ok_or_else(|| format!("mint {mint} does not exist"))?;
    let mut data = if *token_program == client::TOKEN_2022_PROGRAM_ID {
        token_2022_account_data(&mint_account.data)?
    } else {
        vec![0u8; TOKEN_ACCOUNT_LEN]
    };
    data[..32].copy_from_slice(mint.as_ref());
    data[TOKEN_OWNER_OFFSET..TOKEN_OWNER_OFFSET + 32].copy_from_slice(owner.as_ref());
    data[TOKEN_AMOUNT_OFFSET..TOKEN_AMOUNT_OFFSET + 8].copy_from_slice(&amount.to_le_bytes());
    // AccountState::Initialized, even for mints whose accounts start frozen: the checks
    // need a usable trader account, and the market's own accounts keep their real state.
    data[TOKEN_STATE_OFFSET] = 1;
    let rent = svm.minimum_balance_for_rent_exemption(data.len());
    let mut lamports = rent;
    if client::wallet::is_native(mint, token_program) {
        // is_native: COption<u64> holding the rent-exempt reserve; the balance is the
        // lamports above it.
        data[TOKEN_IS_NATIVE_OFFSET..TOKEN_IS_NATIVE_OFFSET + 4]
            .copy_from_slice(&1u32.to_le_bytes());
        data[TOKEN_IS_NATIVE_OFFSET + 4..TOKEN_IS_NATIVE_OFFSET + 12]
            .copy_from_slice(&rent.to_le_bytes());
        lamports = rent
            .checked_add(amount)
            .ok_or("wrapped SOL amount overflows")?;
    }
    svm.set_account(
        address,
        Account {
            lamports,
            data,
            owner: *token_program,
            executable: false,
            rent_epoch: 0,
        },
    )
    .map_err(|e| format!("cannot write token account {address}: {e}"))?;
    Ok(address)
}

/// Creates `owner`'s associated token account with no tokens unless it already exists.
pub fn ensure_token_account(
    svm: &mut LiteSVM,
    mint: &Pubkey,
    token_program: &Pubkey,
    owner: &Pubkey,
) -> Result<Pubkey, String> {
    let address = pda::associated_token(owner, mint, token_program);
    if svm.get_account(&address).is_some() {
        return Ok(address);
    }
    write_token_account(svm, mint, token_program, owner, 0)
}

/// Gives a system account `lamports` SOL so it can pay fees and rent.
pub fn fund_lamports(svm: &mut LiteSVM, address: &Pubkey, lamports: u64) {
    svm.set_account(
        *address,
        Account {
            lamports,
            data: vec![],
            owner: client::SYSTEM_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .expect("a system account can always be written");
}

/// Token-2022 account data with the extensions a holder of `mint` must carry.
#[allow(deprecated)] // the replacement helper is not public in this interface version
fn token_2022_account_data(mint_data: &[u8]) -> Result<Vec<u8>, String> {
    use spl_token_2022_interface::extension::{
        BaseStateWithExtensionsMut, ExtensionType, StateWithExtensionsMut,
    };
    use spl_token_2022_interface::state::{Account as TokenAccount, Mint};
    let mint = StateWithExtensions::<Mint>::unpack(mint_data).map_err(|e| e.to_string())?;
    let mint_extensions = mint.get_extension_types().map_err(|e| e.to_string())?;
    let account_extensions = ExtensionType::get_required_init_account_extensions(&mint_extensions);
    let len = ExtensionType::try_calculate_account_len::<TokenAccount>(&account_extensions)
        .map_err(|e| e.to_string())?;
    let mut data = vec![0u8; len];
    let mut state = StateWithExtensionsMut::<TokenAccount>::unpack_uninitialized(&mut data)
        .map_err(|e| e.to_string())?;
    for extension in account_extensions {
        state
            .init_account_extension_from_type(extension)
            .map_err(|e| e.to_string())?;
    }
    state.init_account_type().map_err(|e| e.to_string())?;
    Ok(data)
}

/// The text of a failed transaction: the error and the most telling log line.
pub fn failure_text(failed: &litesvm::types::FailedTransactionMetadata) -> String {
    let logs = &failed.meta.logs;
    let telling = logs
        .iter()
        .rev()
        .find(|line| line.contains("Error") || line.contains("error") || line.contains("failed"))
        .or(logs.last());
    match telling {
        Some(line) => format!("{:?}: {line}", failed.err),
        None => format!("{:?}", failed.err),
    }
}

/// Whether a failed transaction ran out of compute.
pub fn out_of_compute(failed: &litesvm::types::FailedTransactionMetadata) -> bool {
    format!("{:?}", failed.err).contains("ComputationalBudgetExceeded")
        || failed.meta.logs.iter().any(|line| {
            line.contains("exceeded CUs meter") || line.contains("Computational budget exceeded")
        })
}

/// The ComputeBudget `SetComputeUnitLimit` instruction.
pub fn compute_limit_instruction(units: u32) -> client::Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    client::Instruction::new_with_bytes(COMPUTE_BUDGET_PROGRAM_ID, &data, vec![])
}
