//! The call-depth budget for programs that call the router.
//!
//! Solana numbers the instruction stack from 1: a transaction's own instructions run at
//! height 1, a program they invoke at 2, and so on. The runtime refuses an invocation
//! that would run above [`MAX_STACK_HEIGHT`] (five today; SIMD-0268 will raise it to nine
//! once its feature is active).
//!
//! A swap through the router uses these levels below its caller, at height `h`:
//!
//! | height  | program                                                       |
//! |---------|---------------------------------------------------------------|
//! | `h`     | the calling program                                           |
//! | `h + 1` | the router                                                    |
//! | `h + 2` | the strategy; the router's input transfer; the router's event |
//! | `h + 3` | the strategy's output transfer; the input mint's hook         |
//! | `h + 4` | the output mint's transfer hook                               |
//!
//! So a swap with no transfer hooks needs [`SWAP_LEVELS`] (3) levels below the caller
//! and fits up to `h = 2`. A swap whose output mint has a transfer hook needs
//! [`HOOKED_SWAP_LEVELS`] (4) and fits only at `h = 1`: the caller must be the
//! transaction's own instruction. A strategy or hook that makes calls of its own needs
//! more: a hook that itself invokes the token program needs a fifth level and cannot run
//! under a calling program at all today.
//!
//! [`stack_height`] and [`log_stack_height`] report where a program runs, so a developer
//! can see the budget in their own tests.

use solana_program::msg;

/// The height of a transaction's own instructions.
pub const TRANSACTION_LEVEL_STACK_HEIGHT: usize = 1;
/// The highest stack height Solana runs a program at today.
pub const MAX_STACK_HEIGHT: usize = 5;
/// Levels a swap uses below its caller with no transfer hooks: the router, the strategy
/// and the token program.
pub const SWAP_LEVELS: usize = 3;
/// Levels a swap uses below its caller when the output mint has a transfer hook (which
/// runs under the strategy's transfer): one more than [`SWAP_LEVELS`].
pub const HOOKED_SWAP_LEVELS: usize = 4;

/// The current stack height: 1 for a transaction's own instruction, 2 for a program it
/// invokes, and so on. 0 off-chain.
pub fn stack_height() -> usize {
    solana_program::instruction::get_stack_height()
}

/// How many levels can still run below the current program under [`MAX_STACK_HEIGHT`].
pub fn levels_below() -> usize {
    MAX_STACK_HEIGHT.saturating_sub(stack_height())
}

/// Whether a swap called from here fits the stack: [`HOOKED_SWAP_LEVELS`] if a transfer
/// hook runs on the output side, else [`SWAP_LEVELS`]. A strategy that makes calls of its
/// own needs more than this.
pub fn swap_fits(output_hook: bool) -> bool {
    let needed = if output_hook {
        HOOKED_SWAP_LEVELS
    } else {
        SWAP_LEVELS
    };
    levels_below() >= needed
}

/// Logs `"<label>: stack height <h> of <max>"`, for debugging a program's call depth.
pub fn log_stack_height(label: &str) {
    msg!(
        "{}: stack height {} of {}",
        label,
        stack_height(),
        MAX_STACK_HEIGHT
    );
}
