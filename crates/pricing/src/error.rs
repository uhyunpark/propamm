//! Error type for the pricing crate.

/// Why a quote or validation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PricingError {
    /// `amount_in` was zero.
    ZeroInput,
    /// The quote rounded down to zero output.
    ZeroOutput,
    /// The trade needs more than the remaining depth on this side.
    InsufficientDepth,
    /// The ladder parameters (or a level index) are invalid.
    InvalidParams,
    /// An intermediate value did not fit its integer type.
    Overflow,
    /// The current slot is earlier than the last update slot.
    SlotRegression,
    /// A price update already happened in the current slot.
    AlreadyUpdatedThisSlot,
}
