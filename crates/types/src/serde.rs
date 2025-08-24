use serde::{Serialize, Deserialize, Deserializer, Serializer};
use serde::de::Error as DeError;
use solana_program::{
    pubkey::Pubkey,
    instruction::{Instruction, AccountMeta},
};
use std::str::FromStr;

/// -------- Pubkey as base58 string (Solana JSON convention) --------
pub mod serde_pubkey {
    use super::*;
    pub fn serialize<S: Serializer>(key: &Pubkey, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&key.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Pubkey, D::Error> {
        let s = String::deserialize(d)?;
        Pubkey::from_str(&s).map_err(D::Error::custom)
    }
}

/// Option<Pubkey> helper
pub mod serde_pubkey_opt {
    use super::*;
    pub fn serialize<S: Serializer>(v: &Option<Pubkey>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(k) => s.serialize_some(&k.to_string()),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Pubkey>, D::Error> {
        let opt = Option::<String>::deserialize(d)?;
        match opt {
            Some(s) => Ok(Some(Pubkey::from_str(&s).map_err(D::Error::custom)?)),
            None => Ok(None),
        }
    }
}

/// -------- Instruction serializer (independent of solana-program’s optional serde) --------
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstructionSer {
    #[serde(with = "crate::serde::serde_pubkey")]
    pub program_id: Pubkey,
    pub accounts: Vec<AccountMetaSer>,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountMetaSer {
    #[serde(with = "crate::serde::serde_pubkey")]
    pub pubkey: Pubkey,
    pub is_signer: bool,
    pub is_writable: bool,
}

impl From<Instruction> for InstructionSer {
    fn from(ix: Instruction) -> Self {
        Self {
            program_id: ix.program_id,
            accounts: ix.accounts.into_iter().map(|m| AccountMetaSer {
                pubkey: m.pubkey,
                is_signer: m.is_signer,
                is_writable: m.is_writable,
            }).collect(),
            data: ix.data,
        }
    }
}

impl From<InstructionSer> for Instruction {
    fn from(s: InstructionSer) -> Self {
        Instruction {
            program_id: s.program_id,
            accounts: s.accounts.into_iter().map(|m| AccountMeta {
                pubkey: m.pubkey,
                is_signer: m.is_signer,
                is_writable: m.is_writable,
            }).collect(),
            data: s.data,
        }
    }
}

/// Use as `#[serde(with="crate::serde::serde_instruction")]`
pub mod serde_instruction {
    use super::*;
    pub fn serialize<S: Serializer>(ix: &Instruction, s: S) -> Result<S::Ok, S::Error> {
        InstructionSer::from(ix.clone()).serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Instruction, D::Error> {
        let ser = InstructionSer::deserialize(d)?;
        Ok(Instruction::from(ser))
    }
}
