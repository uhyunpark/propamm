use anchor_lang::prelude::*;
use propamm_pricing::PricingError;

#[error_code]
pub enum PropAmmError {
    #[msg("Signer is not authorized for this action")]
    Unauthorized,
    #[msg("Invalid market parameters")]
    InvalidParams,
    #[msg("Mint is not one of the market's mints")]
    InvalidMint,
    #[msg("Vault does not match the market's vault for this mint")]
    InvalidVault,
    #[msg("Arithmetic overflow")]
    MathOverflow,
    #[msg("Input amount is zero")]
    ZeroInput,
    #[msg("Quote rounds to zero output")]
    ZeroOutput,
    #[msg("Not enough depth left on this side")]
    InsufficientDepth,
    #[msg("Updater must not be the default key or the admin")]
    InvalidUpdater,
    #[msg("No price yet: the admin must call force_price first")]
    NotInitializedPrice,
    #[msg("New mid is outside the allowed move band for this slot window")]
    PriceMoveTooLarge,
    #[msg("Market is paused")]
    Paused,
    #[msg("Clock slot is earlier than the last update slot")]
    SlotRegression,
    #[msg("The price was already updated in this slot")]
    AlreadyUpdatedThisSlot,
    #[msg("Price is stale: no update within max_age_slots")]
    StalePrice,
    #[msg("Output is below min_out")]
    SlippageExceeded,
    #[msg("Output vault holds less than the swap output")]
    InsufficientVaultBalance,
}

impl From<PricingError> for PropAmmError {
    fn from(e: PricingError) -> Self {
        match e {
            PricingError::ZeroInput => Self::ZeroInput,
            PricingError::ZeroOutput => Self::ZeroOutput,
            PricingError::InsufficientDepth => Self::InsufficientDepth,
            PricingError::InvalidParams => Self::InvalidParams,
            PricingError::Overflow => Self::MathOverflow,
            PricingError::SlotRegression => Self::SlotRegression,
            PricingError::AlreadyUpdatedThisSlot => Self::AlreadyUpdatedThisSlot,
        }
    }
}
