# Calling the router from a program

A program can trade through any Pipeline market: a vault, a treasury, a keeper, a smart
wallet or another protocol. It invokes the router's `swap` or `route` instruction with its
own PDA as the trader, signing with `invoke_signed`, and reads back what the swap did. The
router treats it exactly like a wallet: the same checks, the same 0.25% fee on every hop,
the same `SwapEvent`, the same minimum.

`pipeline-cpi` (`programs/pipeline-cpi`, Apache-2.0) does the work: it builds the router's
instructions from `AccountInfo`s in the router's order, invokes them with your signer
seeds, and decodes the result from return data. It needs only `solana-program`; an
optional `anchor` feature adds `CpiContext`-style calls. The worked example is a small
DCA vault, `programs/example-dca-vault`, tested against the real router in
`crates/pipeline-tests/tests/cpi_vault.rs`.

```toml
pipeline-cpi = "0.1"
# Anchor programs:
pipeline-cpi = { version = "0.1", features = ["anchor"] }
```

## 1. The shape of a call

Build the router's accounts off-chain exactly as for a wallet's swap (with
`pipeline_client::swap`, the TypeScript SDK or the hosted API's account lists), with your
PDA as the trader. Append them to your own instruction's accounts with the trader not
signing; your program signs for it.

```rust
use pipeline_cpi::{check_token_account, SwapAccounts, SwapArgs};

// accounts[0] is the program's own state; the router's accounts follow, in its order.
let (vault, router_accounts) = accounts.split_first().ok_or(ProgramError::NotEnoughAccountKeys)?;
let swap = SwapAccounts::from_router_order(router_accounts, input_hook_accounts)?;

// The router checks the market side. Only you can check the trade is yours.
if swap.trader.key != vault.key { return Err(ProgramError::InvalidArgument); }
check_token_account(swap.trader_output, vault.key, &state.output_mint)?;

let result = pipeline_cpi::swap(
    &swap,
    &SwapArgs::exact_in(state.amount, state.min_out),
    &[&[b"vault", state.owner.as_ref(), &[state.bump]]],
)?;
let kept = result.kept(swap.output_mint.key);
```

`pipeline_cpi::route` does the same for a multi-hop `route` (`RouteAccounts`, one
`HopShape` per hop saying how many strategy and input-hook accounts it has).

## 2. Accounts

`swap` takes 16 fixed accounts, then the strategy's accounts, then the input mint's
transfer-hook accounts. `SwapAccounts::from_router_order` reads them in this order and
`SwapAccounts::instruction` writes them back the same way:

| # | Account | Writable | Notes |
|---|---|---|---|
| 0 | trader | no | Your PDA. Signs through your seeds. |
| 1 | strategy program | no | |
| 2 | market | no | `["market", strategy, market_state]` under the router |
| 3 | market state | yes | |
| 4 | trader input | yes | Your token account in the input mint |
| 5 | trader output | yes | Where the output arrives. **The router doesn't check whose it is.** |
| 6 | input vault | yes | The market's vault for the input mint |
| 7, 8 | input mint, output mint | no | |
| 9, 10 | input and output token programs | no | SPL Token or Token-2022 |
| 11 | fee authority | no | `["fees", market]` under the router |
| 12 | fee account | yes | See section 3 |
| 13 | integrator fee account | yes | Or the router's ID when there is none |
| 14 | event authority | no | `["__event_authority"]` under the router |
| 15 | router | no | |
| 16.. | strategy accounts | as listed | From the strategy's `pipeline_resolve_accounts`; an output mint's hook accounts go here too |
| last `n` | input hook accounts | as listed | `n` is `input_hook_accounts` |

`route` takes the trader, the integrator fee account (or the router's ID), the event
authority and the router, then for each hop 12 fixed accounts (strategy program, market,
market state, trader input, trader output, input vault, input mint, output mint, input and
output token programs, fee authority, fee account), its strategy accounts and its input
hook accounts. Each hop's input account must be the previous hop's output account.

Strategy and hook accounts are passed on with the writable flag your program received
them with, and never as signers.

## 3. Which fee account

Pipeline's fee is taken in the market's fee mint when the output is that mint, and in the
input otherwise. A market that declares no fee mint and trades wrapped SOL takes its fee in
SOL. So:

- **Fee on the input:** `fee_account` is the fee authority's associated token account in
  the input mint. You part with `amount_in` plus 0.25% (rounded up) plus any integrator
  fee.
- **Fee on the output:** `fee_account` is the fee authority's associated token account in
  the output mint. You part with `amount_in` (plus any integrator fee, always on the
  input); the fee comes out of what arrived, and `min_amount_out` applies to what you keep.

Off-chain, `pipeline_client::FeeSide::of(&market, &output_mint)` and the SDK pick the side.
On-chain, `pipeline_cpi::read_market(market)?.fee_side(output_mint)` reads it from the
router's `Market` record. The wrong account fails with `WrongFeeAccount`.

## 4. What your program must check

The router checks everything about the market: the market record, the strategy, the input
vault, the fee account, the token programs and the mints. It measures the trade on the
accounts it is given and refuses a swap that takes more than `amount_in` plus fees, lowers
the output balance, or leaves less than the minimum.

It does not know which output account is yours. A wallet chooses its own; a program that
lets someone else build the transaction (a permissionless crank, a keeper) must check:

- the trader is its PDA;
- the input and output mints are the ones it means to trade;
- the output account is its own: `check_token_account(output, pda, mint)`. Without this,
  whoever builds the transaction can have your swap pay into their account (the
  `the_vault_refuses_output_sent_to_someone_else` test shows the vault refusing it);
- the amount and minimum come from your program's state, not from the caller.

Other things to know:

- **Never pass your PDA as a signer to `create_market`.** The router can't tell a PDA with
  no SOL from a new keypair and would pass its signature on to the strategy.
- **CPI Guard.** A Token-2022 account with CPI Guard enabled can't be spent by the router.
  Don't enable it on accounts your program trades from.
- **Reentrancy.** The runtime refuses a call back into a program already on the stack, so
  a strategy can't call your program in the middle of your swap.

## 5. Reading the result

The router sets return data at the end of `swap` and `route`. `pipeline_cpi::swap` and
`pipeline_cpi::route` read it straight after the call and refuse return data from any
other program (`CpiError::NoRouterResult`). If you invoke the router yourself, call
`swap_result()` or `route_result()` before making any other call, which would replace it.

`SwapResult`:

| Field | Meaning |
|---|---|
| `net_sent` | The input the swap was charged for, excluding fees |
| `received` | What arrived in the output account, before any fee taken from it |
| `pipeline_fee` | Pipeline's fee, in `fee_mint` |
| `integrator_fee` | The integrator's fee, in the input mint |
| `fee_mint` | The input mint, or the output mint when the fee was taken in the output |

`result.kept(output_mint)` is what you keep and `result.spent(input_mint)` what you parted
with in the input mint. `RouteResult` has `net_sent` (the first hop's input), `amount_out`
(what you keep from the last hop), `integrator_fee` and `input_pipeline_fee` (Pipeline's
fee taken in the input mint: the first hop's fee if it paid on its input, otherwise 0);
`result.spent()` adds them up. The router also emits a `SwapEvent`
per hop, which indexers find in your transaction like any other swap.

Return data copied from transaction metadata (simulations, `getTransaction`) has its
trailing zero bytes trimmed; `SwapResult::decode` and `RouteResult::decode` accept that.

## 6. Call depth

Solana numbers the instruction stack from 1: a transaction's own instruction runs at height
1, a program it calls at 2, and so on, up to a limit of **5**. (SIMD-0268 raises the limit
to 9 once its feature is activated; until then plan for 5.) A swap uses these levels below
a calling program at height `h`:

| Height | Program |
|---|---|
| `h` | your program |
| `h + 1` | the router |
| `h + 2` | the strategy; the router's input transfer (token program); the router's event |
| `h + 3` | the strategy's output transfer (token program); the input mint's transfer hook |
| `h + 4` | the output mint's transfer hook |

Measured with the example vault as the transaction's instruction (`h = 1`):

| Swap | Deepest program | Height |
|---|---|---|
| Plain SPL tokens, fee on the input or the output | strategy's output transfer | 4 |
| Two-hop route | strategy's output transfer | 4 |
| Hooked input mint | input mint's hook | 4 |
| Hooked output mint | output mint's hook | 5 (the limit) |
| Output hook that calls the token program itself | the hook's call | would be 6: fails with `CallDepth`, nothing moves |

So:

- A swap with no transfer hooks needs three levels below your program
  (`depth::SWAP_LEVELS`): it works from a top-level instruction or from a program called by
  one (`h` up to 2).
- A swap whose output mint has a transfer hook needs four (`depth::HOOKED_SWAP_LEVELS`): it
  works only when your program is the transaction's instruction (`h = 1`).
- A strategy or hook that makes calls of its own needs more. The last test above shows a
  hook whose own call works for a wallet (height 5) and fails under the vault.

`pipeline_cpi::stack_height()` returns the current height and
`pipeline_cpi::log_stack_height("label")` logs it; `depth::swap_fits(output_hook)` says
whether a swap fits from where your program runs. The runtime's own log lines
(`Program <id> invoke [<height>]`) show every program's height; `cpi_vault.rs` prints them
for each test with `--nocapture`.

## 7. Compute

Measured in LiteSVM with the constant-product example strategy:

| Transaction | Compute units |
|---|---|
| A wallet's swap | 33,414 |
| The vault's swap, fee on the input | 45,448 |
| The vault's swap, fee on the output (SOL) | 44,373 |
| The vault's two-hop route | 81,472 |
| The vault's swap, hooked input mint (the input and the fee transfer each run the hook) | 80,016 |
| The vault's swap, hooked output mint | 66,373 |

Calling through a program adds about 12,000 units for the extra invocation and the
caller's own checks. Swaps with transfer hooks can pass the default 200,000-unit budget;
request more with a ComputeBudget instruction.

## 8. The example: a DCA vault

`programs/example-dca-vault` is a program-owned wallet that buys on a schedule. The owner
creates a vault (a PDA of the program, `["vault", owner, input_mint, output_mint]`) with an
amount per purchase, a minimum to accept for it and an interval, and funds the vault's
token accounts. After that anyone can crank it:

- `crank` with a swap: the vault's account, then a router `swap`'s accounts. The cranker
  picks the market.
- `crank` with a route: the vault's account, then a router `route`'s accounts.
- `withdraw`: the owner moves tokens out; the vault signs the transfer.

On each crank the vault checks the mints and that both ends are its own token accounts,
logs its stack height, swaps its fixed amount with its own minimum through
`pipeline-cpi`, and records the `SwapResult` (or `RouteResult`). Whatever market the
cranker picks, the vault never accepts less than its minimum.

Build it with `scripts/build-programs.sh example-dca-vault` and run its tests with
`cargo test -p pipeline-tests --test cpi_vault -- --nocapture`.

## 9. Anchor

With the `anchor` feature, `pipeline_cpi::anchor::{swap, route}` take a `CpiContext` whose
accounts are `pipeline_cpi::anchor::Swap` (or `Route`) and whose remaining accounts are the
strategy's and hook accounts:

```rust
let accounts = pipeline_cpi::anchor::Swap {
    trader: ctx.accounts.vault.to_account_info(),
    // ...the market and token accounts, as `AccountInfo`s...
    integrator_fee_account: None,
    event_authority: ctx.accounts.event_authority.to_account_info(),
    router: ctx.accounts.router.to_account_info(),
};
let cpi = CpiContext::new_with_signer(pipeline_cpi::ROUTER_ID, accounts, signer_seeds)
    .with_remaining_accounts(ctx.remaining_accounts.to_vec());
let result = pipeline_cpi::anchor::swap(cpi, SwapArgs::exact_in(amount, min_out), hook_count)?;
```

Anchor's `AccountInfo` is the same type the plain functions take, so an Anchor program can
also use `SwapAccounts::from_router_order` on its remaining accounts directly.
