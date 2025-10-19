use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use solana_program::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
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
            accounts: ix
                .accounts
                .into_iter()
                .map(|m| AccountMetaSer {
                    pubkey: m.pubkey,
                    is_signer: m.is_signer,
                    is_writable: m.is_writable,
                })
                .collect(),
            data: ix.data,
        }
    }
}

impl From<InstructionSer> for Instruction {
    fn from(s: InstructionSer) -> Self {
        Instruction {
            program_id: s.program_id,
            accounts: s
                .accounts
                .into_iter()
                .map(|m| AccountMeta {
                    pubkey: m.pubkey,
                    is_signer: m.is_signer,
                    is_writable: m.is_writable,
                })
                .collect(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;

    #[derive(Serialize, Deserialize, Debug)]
    struct PkWrap {
        #[serde(with = "crate::serde::serde_pubkey")]
        key: Pubkey,
    }

    #[derive(Serialize, Deserialize, Debug)]
    struct PkOptWrap {
        #[serde(with = "crate::serde::serde_pubkey_opt")]
        key: Option<Pubkey>,
    }

    #[derive(Serialize, Deserialize, Debug)]
    struct IxWrap {
        #[serde(with = "crate::serde::serde_instruction")]
        ix: Instruction,
    }

    fn k() -> Pubkey {
        Pubkey::new_unique()
    }

    #[test]
    fn pubkey_base58_roundtrip() {
        let key = k();
        let wrap = PkWrap { key };
        let j = serde_json::to_string(&wrap).unwrap();
    assert!(j.contains(&format!("\"{key}\"")));
        let back: PkWrap = serde_json::from_str(&j).unwrap();
        assert_eq!(back.key, key);
    }

    #[test]
    fn pubkey_opt_some_and_none() {
        let wrap_some = PkOptWrap { key: Some(k()) };
        let j = serde_json::to_string(&wrap_some).unwrap();
        let back_some: PkOptWrap = serde_json::from_str(&j).unwrap();
        assert_eq!(back_some.key, wrap_some.key);

        let wrap_none = PkOptWrap { key: None };
        let j2 = serde_json::to_string(&wrap_none).unwrap();
        let back_none: PkOptWrap = serde_json::from_str(&j2).unwrap();
        assert!(back_none.key.is_none());
    }

    #[test]
    fn instructionser_roundtrip() {
        let ix = Instruction {
            program_id: k(),
            accounts: vec![
                AccountMeta::new(k(), true),
                AccountMeta::new_readonly(k(), false),
            ],
            data: vec![1, 2, 3, 4, 5],
        };
        let ser = InstructionSer::from(ix.clone());
        let j = serde_json::to_string(&ser).unwrap();
        let back_ser: InstructionSer = serde_json::from_str(&j).unwrap();
        let back_ix: Instruction = back_ser.into();

        assert_eq!(back_ix.program_id, ix.program_id);
        assert_eq!(back_ix.data, ix.data);
        assert_eq!(back_ix.accounts.len(), ix.accounts.len());
        for (a, b) in back_ix.accounts.iter().zip(ix.accounts.iter()) {
            assert_eq!(a.pubkey, b.pubkey);
            assert_eq!(a.is_signer, b.is_signer);
            assert_eq!(a.is_writable, b.is_writable);
        }
    }

    #[test]
    fn serde_instruction_attr_roundtrip() {
        let ix = Instruction {
            program_id: k(),
            accounts: vec![AccountMeta::new(k(), false)],
            data: vec![9, 9, 9],
        };
        let wrap = IxWrap { ix: ix.clone() };
        let j = serde_json::to_string(&wrap).unwrap();
        let back: IxWrap = serde_json::from_str(&j).unwrap();

        assert_eq!(back.ix.program_id, ix.program_id);
        assert_eq!(back.ix.data, ix.data);
        assert_eq!(back.ix.accounts.len(), 1);
        assert_eq!(back.ix.accounts[0].pubkey, ix.accounts[0].pubkey);
    }
}
