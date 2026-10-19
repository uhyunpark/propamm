//! Localnet/devnet config file written by `setup-local` and read by every other subcommand.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;

use crate::publish::Decimals;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetConfig {
    #[serde(with = "base58")]
    pub market: Pubkey,
    #[serde(with = "base58")]
    pub base_mint: Pubkey,
    #[serde(with = "base58")]
    pub quote_mint: Pubkey,
    #[serde(with = "base58")]
    pub base_vault: Pubkey,
    #[serde(with = "base58")]
    pub quote_vault: Pubkey,
    pub base_decimals: u8,
    pub quote_decimals: u8,
    pub admin_keypair: PathBuf,
    pub updater_keypair: PathBuf,
    pub trader_keypair: PathBuf,
    pub rpc_url: String,
    pub ws_url: String,
}

/// Pubkeys are stored as base58 strings so the file stays human-readable
/// (`solana-pubkey`'s own serde impl writes a 32-byte array).
mod base58 {
    use std::str::FromStr;

    use serde::{Deserialize, Deserializer, Serializer, de::Error};
    use solana_pubkey::Pubkey;

    pub fn serialize<S: Serializer>(key: &Pubkey, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(key)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Pubkey, D::Error> {
        let text = String::deserialize(d)?;
        Pubkey::from_str(&text).map_err(D::Error::custom)
    }
}

impl NetConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {} (run `setup-local` first)", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)? + "\n")
            .with_context(|| format!("writing {}", path.display()))
    }

    pub fn decimals(&self) -> Decimals {
        Decimals {
            base: self.base_decimals,
            quote: self.quote_decimals,
        }
    }
}

/// Reads a Solana CLI keypair file (JSON array of 64 bytes).
pub fn read_keypair(path: &Path) -> anyhow::Result<Keypair> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading keypair {}", path.display()))?;
    let bytes: Vec<u8> = serde_json::from_str(&text)
        .with_context(|| format!("keypair {} is not a JSON byte array", path.display()))?;
    Keypair::try_from(bytes.as_slice())
        .map_err(|e| anyhow::anyhow!("invalid keypair {}: {e}", path.display()))
}

/// Writes a keypair in the Solana CLI format, readable by the owner only.
pub fn write_keypair(path: &Path, kp: &Keypair) -> anyhow::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string(&kp.to_bytes().to_vec())?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
        .with_context(|| format!("creating keypair {}", path.display()))?
        .write_all(json.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pubkeys_round_trip_as_base58_strings() {
        let key = Pubkey::new_unique();
        let cfg = NetConfig {
            market: key,
            base_mint: key,
            quote_mint: key,
            base_vault: key,
            quote_vault: key,
            base_decimals: 9,
            quote_decimals: 6,
            admin_keypair: PathBuf::from("/a"),
            updater_keypair: PathBuf::from("/u"),
            trader_keypair: PathBuf::from("/t"),
            rpc_url: "http://127.0.0.1:8899".into(),
            ws_url: "ws://127.0.0.1:8900".into(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains(&format!("\"market\":\"{key}\"")));
        let back: NetConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.market, key);
    }
}
