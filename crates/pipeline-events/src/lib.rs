//! Decodes events emitted by the Pipeline router.
//!
//! The router emits every event as a self-CPI (Anchor `emit_cpi!`), so events survive
//! log truncation. In a transaction's inner instructions, an event is an instruction to
//! the router program whose data is:
//!
//! ```text
//! EVENT_IX_TAG (8 bytes) | event discriminator (8 bytes) | Borsh-encoded event
//! ```
//!
//! where the event discriminator is `sha256("event:<Name>")[..8]`. Pass that data to
//! [`decode`].

use borsh::{BorshDeserialize, BorshSerialize};
use solana_pubkey::Pubkey;

/// Anchor's prefix for self-CPI event instructions (`0x1d9acb512ea545e4`, little-endian).
pub const EVENT_IX_TAG: [u8; 8] = 0x1d9a_cb51_2ea5_45e4u64.to_le_bytes();

/// `sha256("event:<Name>")[..8]` for each router event.
pub mod discriminator {
    pub const SWAP: [u8; 8] = [64, 198, 205, 232, 38, 8, 113, 226];
    pub const STRATEGY_REGISTERED: [u8; 8] = [167, 54, 3, 14, 221, 37, 211, 214];
    pub const METADATA_UPDATED: [u8; 8] = [132, 36, 215, 246, 166, 90, 189, 44];
    pub const MARKET_CREATED: [u8; 8] = [88, 184, 130, 231, 226, 84, 6, 58];
    pub const FEES_SWEPT: [u8; 8] = [96, 218, 115, 136, 74, 170, 202, 172];
    pub const CONFIG_CHANGED: [u8; 8] = [147, 25, 86, 98, 98, 77, 78, 192];
    pub const LOOKUP_TABLE_RECORDED: [u8; 8] = [163, 143, 140, 130, 113, 26, 74, 133];
}

/// One trade through Pipeline. Amounts are in the smallest units of their mints.
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct SwapEvent {
    pub market: Pubkey,
    pub strategy: Pubkey,
    pub trader: Pubkey,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    /// 0 = exact-in, 1 = exact-out.
    pub mode: u8,
    /// Input the swap is charged for, excluding fees: the trader's net spend, or what
    /// the market's input vault kept if that is more, at most `amount_in`.
    pub net_sent: u64,
    /// Output that arrived in the trader's output account. When the fee is taken in the
    /// output (`fee_mint == output_mint`), the trader keeps `received - pipeline_fee`.
    pub received: u64,
    pub pipeline_fee: u64,
    pub integrator: Option<Pubkey>,
    pub integrator_fee: u64,
    pub slot: u64,
    pub unix_timestamp: i64,
    /// The mint `pipeline_fee` was taken in: the input mint, or the output mint when it is
    /// the market's fee mint.
    pub fee_mint: Pubkey,
}

/// A `SwapEvent` as routers before the fee mint emitted it: no `fee_mint`, because the
/// fee was always taken in the input.
#[derive(BorshDeserialize)]
struct SwapEventV1 {
    market: Pubkey,
    strategy: Pubkey,
    trader: Pubkey,
    input_mint: Pubkey,
    output_mint: Pubkey,
    mode: u8,
    net_sent: u64,
    received: u64,
    pipeline_fee: u64,
    integrator: Option<Pubkey>,
    integrator_fee: u64,
    slot: u64,
    unix_timestamp: i64,
}

impl From<SwapEventV1> for SwapEvent {
    fn from(v1: SwapEventV1) -> Self {
        SwapEvent {
            market: v1.market,
            strategy: v1.strategy,
            trader: v1.trader,
            input_mint: v1.input_mint,
            output_mint: v1.output_mint,
            mode: v1.mode,
            net_sent: v1.net_sent,
            received: v1.received,
            pipeline_fee: v1.pipeline_fee,
            integrator: v1.integrator,
            integrator_fee: v1.integrator_fee,
            slot: v1.slot,
            unix_timestamp: v1.unix_timestamp,
            fee_mint: v1.input_mint,
        }
    }
}

/// Decodes a swap event body in the current layout, or in the layout before the fee mint
/// (its fee is in the input mint), so indexers can read a router's whole history.
fn decode_swap(body: &[u8]) -> Option<SwapEvent> {
    SwapEvent::try_from_slice(body)
        .ok()
        .or_else(|| SwapEventV1::try_from_slice(body).ok().map(SwapEvent::from))
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct StrategyRegistered {
    pub strategy: Pubkey,
    pub fee_paid: u64,
    pub metadata_uri: String,
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct MetadataUpdated {
    pub strategy: Pubkey,
    pub metadata_uri: String,
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct MarketCreated {
    pub market: Pubkey,
    pub strategy: Pubkey,
    pub market_state: Pubkey,
    pub mints: Vec<Pubkey>,
    pub created_by: Pubkey,
}

/// A market's address lookup table was recorded (`record_lookup_table`).
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct LookupTableRecorded {
    pub market: Pubkey,
    pub table: Pubkey,
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct FeesSwept {
    pub market: Pubkey,
    pub mint: Pubkey,
    pub amount: u64,
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct ConfigChanged {
    pub admin: Pubkey,
    pub listing_fee_lamports: u64,
    pub listing_fee_recipient: Pubkey,
    pub treasury_owner: Pubkey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PipelineEvent {
    Swap(SwapEvent),
    StrategyRegistered(StrategyRegistered),
    MetadataUpdated(MetadataUpdated),
    MarketCreated(MarketCreated),
    FeesSwept(FeesSwept),
    ConfigChanged(ConfigChanged),
    LookupTableRecorded(LookupTableRecorded),
}

/// Decodes the data of a router self-CPI event instruction. Returns `None` for any other
/// instruction data, including events from newer router versions this crate does not know.
pub fn decode(instruction_data: &[u8]) -> Option<PipelineEvent> {
    let rest = instruction_data.strip_prefix(&EVENT_IX_TAG)?;
    let (discriminator, body) = rest.split_at_checked(8)?;
    let discriminator: [u8; 8] = discriminator.try_into().ok()?;
    let event = match discriminator {
        discriminator::SWAP => PipelineEvent::Swap(decode_swap(body)?),
        discriminator::STRATEGY_REGISTERED => {
            PipelineEvent::StrategyRegistered(BorshDeserialize::try_from_slice(body).ok()?)
        }
        discriminator::METADATA_UPDATED => {
            PipelineEvent::MetadataUpdated(BorshDeserialize::try_from_slice(body).ok()?)
        }
        discriminator::MARKET_CREATED => {
            PipelineEvent::MarketCreated(BorshDeserialize::try_from_slice(body).ok()?)
        }
        discriminator::FEES_SWEPT => {
            PipelineEvent::FeesSwept(BorshDeserialize::try_from_slice(body).ok()?)
        }
        discriminator::CONFIG_CHANGED => {
            PipelineEvent::ConfigChanged(BorshDeserialize::try_from_slice(body).ok()?)
        }
        discriminator::LOOKUP_TABLE_RECORDED => {
            PipelineEvent::LookupTableRecorded(BorshDeserialize::try_from_slice(body).ok()?)
        }
        _ => return None,
    };
    Some(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn event_discriminator(name: &str) -> [u8; 8] {
        Sha256::digest(format!("event:{name}").as_bytes())[..8]
            .try_into()
            .unwrap()
    }

    #[test]
    fn discriminators_match_anchor() {
        assert_eq!(discriminator::SWAP, event_discriminator("SwapEvent"));
        assert_eq!(
            discriminator::STRATEGY_REGISTERED,
            event_discriminator("StrategyRegistered")
        );
        assert_eq!(
            discriminator::METADATA_UPDATED,
            event_discriminator("MetadataUpdated")
        );
        assert_eq!(
            discriminator::MARKET_CREATED,
            event_discriminator("MarketCreated")
        );
        assert_eq!(discriminator::FEES_SWEPT, event_discriminator("FeesSwept"));
        assert_eq!(
            discriminator::CONFIG_CHANGED,
            event_discriminator("ConfigChanged")
        );
        assert_eq!(
            discriminator::LOOKUP_TABLE_RECORDED,
            event_discriminator("LookupTableRecorded")
        );
    }

    #[test]
    fn decodes_swap_events_from_before_the_fee_mint() {
        let event = SwapEvent {
            market: Pubkey::new_from_array([1; 32]),
            strategy: Pubkey::new_from_array([2; 32]),
            trader: Pubkey::new_from_array([3; 32]),
            input_mint: Pubkey::new_from_array([4; 32]),
            output_mint: Pubkey::new_from_array([5; 32]),
            mode: 0,
            net_sent: 1_000,
            received: 990,
            pipeline_fee: 3,
            integrator: None,
            integrator_fee: 0,
            slot: 7,
            unix_timestamp: 8,
            fee_mint: Pubkey::new_from_array([5; 32]),
        };
        let mut data = EVENT_IX_TAG.to_vec();
        data.extend_from_slice(&discriminator::SWAP);
        let body = borsh::to_vec(&event).unwrap();
        let mut current = data.clone();
        current.extend_from_slice(&body);
        assert_eq!(decode(&current), Some(PipelineEvent::Swap(event.clone())));
        // The old layout is the same without the trailing fee mint, which was the input.
        let mut old = data;
        old.extend_from_slice(&body[..body.len() - 32]);
        let expected = SwapEvent {
            fee_mint: event.input_mint,
            ..event
        };
        assert_eq!(decode(&old), Some(PipelineEvent::Swap(expected)));
    }

    #[test]
    fn decodes_swap_and_rejects_other_data() {
        let event = FeesSwept {
            market: Pubkey::new_from_array([1; 32]),
            mint: Pubkey::new_from_array([2; 32]),
            amount: 5,
        };
        let mut data = EVENT_IX_TAG.to_vec();
        data.extend_from_slice(&discriminator::FEES_SWEPT);
        data.extend_from_slice(&borsh::to_vec(&event).unwrap());
        assert_eq!(decode(&data), Some(PipelineEvent::FeesSwept(event)));
        assert_eq!(decode(&data[1..]), None);
        assert_eq!(decode(&[]), None);
    }
}
