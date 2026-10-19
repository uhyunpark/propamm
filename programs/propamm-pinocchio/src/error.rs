//! Error codes, numerically identical to what the Anchor program returns.
//!
//! - Account and instruction validation failures reuse the Anchor framework codes
//!   (`anchor-lang` 1.2 `ErrorCode`: 101/102, 2xxx constraints, 3xxx accounts, 41xx misc).
//! - Program errors keep `PropAmmError`'s `6000 + declaration index`.
//! - Token-data format errors are the builtin `InvalidAccountData` / `UninitializedAccount`,
//!   as Anchor surfaces them from `spl_token`'s `unpack`.
//!
//! So clients and the parity tests see the same `Custom(code)` from both programs.

use pinocchio::error::ProgramError;
use propamm_pricing::PricingError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Error {
    InstructionFallbackNotFound = 101,
    InstructionDidNotDeserialize = 102,
    ConstraintMut = 2000,
    ConstraintRentExempt = 2005,
    ConstraintSeeds = 2006,
    ConstraintTokenMint = 2014,
    ConstraintTokenOwner = 2015,
    ConstraintDuplicateMutableAccount = 2040,
    AccountDiscriminatorNotFound = 3001,
    AccountDiscriminatorMismatch = 3002,
    AccountDidNotDeserialize = 3003,
    AccountNotEnoughKeys = 3005,
    AccountOwnedByWrongProgram = 3007,
    InvalidProgramId = 3008,
    InvalidProgramExecutable = 3009,
    AccountNotSigner = 3010,
    AccountNotInitialized = 3012,
    DeclaredProgramIdMismatch = 4100,
    TryingToInitPayerAsProgramAccount = 4101,
    // `PropAmmError`, in declaration order.
    Unauthorized = 6000,
    InvalidParams,
    InvalidMint,
    InvalidVault,
    MathOverflow,
    ZeroInput,
    ZeroOutput,
    InsufficientDepth,
    InvalidUpdater,
    NotInitializedPrice,
    PriceMoveTooLarge,
    Paused,
    SlotRegression,
    AlreadyUpdatedThisSlot,
    StalePrice,
    SlippageExceeded,
    InsufficientVaultBalance,
}

impl From<Error> for ProgramError {
    fn from(e: Error) -> Self {
        ProgramError::Custom(e as u32)
    }
}

/// Same mapping as the Anchor program's `From<PricingError> for PropAmmError`.
impl From<PricingError> for Error {
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
