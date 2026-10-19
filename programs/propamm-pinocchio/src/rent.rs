//! Rent-exempt minimum, computed exactly like `solana-rent` for every Rent sysvar shape.
//!
//! Pinocchio's own `Rent` reads only the first `u64` and ignores `exemption_threshold`. On a
//! cluster that still has the legacy shape (`{3480, 2.0, 50}`) that undercounts by half, so the
//! market and vaults would be created below the real minimum. This module reads the threshold
//! too. The sysvar is 17 bytes: `lamports_per_byte(_year)` u64, `exemption_threshold` f64,
//! `burn_percent` u8; only the first 16 are needed.
//!
//! No floating point on-chain, same as `solana-rent` on BPF: threshold `1.0` (SIMD-0194,
//! `{6960, 1.0, 50}`) multiplies by 1, threshold `2.0` (legacy) by 2, any other value fails with
//! `InvalidArgument`.

use pinocchio::error::ProgramError;
use pinocchio::sysvars::get_sysvar;
use pinocchio::sysvars::rent::RENT_ID;

/// `f64::to_le_bytes` of `1.0`.
const THRESHOLD_ONE: [u8; 8] = [0, 0, 0, 0, 0, 0, 240, 63];
/// `f64::to_le_bytes` of `2.0`.
const THRESHOLD_TWO: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 64];

/// `solana_rent::ACCOUNT_STORAGE_OVERHEAD`.
const ACCOUNT_STORAGE_OVERHEAD: u64 = 128;
/// `solana_rent` maximum permitted account data length (10 MiB).
const MAX_PERMITTED_DATA_LENGTH: u64 = 10 * 1024 * 1024;
/// `solana_rent` overflow guards: the largest rate that cannot overflow `u64` for each threshold.
const MAX_RATE_THRESHOLD_ONE: u64 = 1_759_197_129_867;
const MAX_RATE_THRESHOLD_TWO: u64 = 879_598_564_933;

pub struct Rent {
    lamports_per_byte: u64,
    /// Multiplier derived from `exemption_threshold` (1 or 2).
    threshold_multiplier: u64,
}

impl Rent {
    /// Reads the Rent sysvar (`sol_get_sysvar`, 16 bytes).
    pub fn get() -> Result<Self, ProgramError> {
        let mut buf = [0u8; 16];
        get_sysvar(&mut buf, &RENT_ID, 0)?;
        Self::from_bytes(buf)
    }

    fn from_bytes(buf: [u8; 16]) -> Result<Self, ProgramError> {
        let mut rate = [0; 8];
        rate.copy_from_slice(&buf[..8]);
        let mut threshold = [0; 8];
        threshold.copy_from_slice(&buf[8..]);
        let (threshold_multiplier, max_rate) = match threshold {
            THRESHOLD_ONE => (1, MAX_RATE_THRESHOLD_ONE),
            THRESHOLD_TWO => (2, MAX_RATE_THRESHOLD_TWO),
            _ => return Err(ProgramError::InvalidArgument),
        };
        let lamports_per_byte = u64::from_le_bytes(rate);
        if lamports_per_byte > max_rate {
            return Err(ProgramError::InvalidArgument);
        }
        Ok(Self {
            lamports_per_byte,
            threshold_multiplier,
        })
    }

    /// `solana_rent::Rent::try_minimum_balance`.
    pub fn minimum_balance(&self, data_len: usize) -> Result<u64, ProgramError> {
        let len = u64::try_from(data_len).map_err(|_| ProgramError::InvalidArgument)?;
        if len > MAX_PERMITTED_DATA_LENGTH {
            return Err(ProgramError::InvalidArgument);
        }
        ACCOUNT_STORAGE_OVERHEAD
            .checked_add(len)
            .and_then(|n| n.checked_mul(self.lamports_per_byte))
            .and_then(|n| n.checked_mul(self.threshold_multiplier))
            .ok_or(ProgramError::InvalidArgument)
    }
}
