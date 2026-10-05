use anyhow::{Context, Result, bail, ensure};
use argon2::Argon2;
use bitcoin::{
    Address, Amount, EcdsaSighashType, Network, OutPoint, PubkeyHash, ScriptBuf, Sequence,
    Transaction, TxIn, TxOut, Witness, absolute, consensus,
    hashes::{Hash, hash160},
    script::Builder,
    secp256k1::{Message, PublicKey, Secp256k1, SecretKey},
    sighash::SighashCache,
    transaction,
};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use clap::ValueEnum;
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Coin {
    Bitcoin,
    Dogecoin,
}

impl Coin {
    pub fn name(self) -> &'static str {
        match self {
            Self::Bitcoin => "bitcoin",
            Self::Dogecoin => "dogecoin",
        }
    }

    pub fn default_fee_per_kb(self) -> u64 {
        match self {
            Self::Bitcoin => 10_000,
            Self::Dogecoin => 1_000_000,
        }
    }

    pub fn dust(self) -> u64 {
        match self {
            Self::Bitcoin => 546,
            Self::Dogecoin => 1_000_000,
        }
    }

    pub fn script(self, address: &str) -> Result<ScriptBuf> {
        if self == Self::Bitcoin {
            return Ok(address
                .parse::<Address<_>>()?
                .require_network(Network::Bitcoin)?
                .script_pubkey());
        }
        let bytes = bs58::decode(address)
            .with_check(None)
            .into_vec()
            .context("Invalid Dogecoin address checksum")?;
        ensure!(bytes.len() == 21, "Invalid Dogecoin address length");
        match bytes[0] {
            30 => Ok(ScriptBuf::new_p2pkh(&PubkeyHash::from_slice(&bytes[1..])?)),
            22 => Ok(ScriptBuf::new_p2sh(&bitcoin::ScriptHash::from_slice(
                &bytes[1..],
            )?)),
            _ => bail!("Expected a Dogecoin mainnet address"),
        }
    }

    pub fn address(self, key: &SecretKey) -> String {
        let public = PublicKey::from_secret_key(&Secp256k1::new(), key);
        let hash = hash160::Hash::hash(&public.serialize());
        let mut payload = vec![if self == Self::Bitcoin { 0 } else { 30 }];
        payload.extend_from_slice(hash.as_byte_array());
        bs58::encode(payload).with_check().into_string()
    }
}

#[derive(Serialize, Deserialize)]
pub struct Wallet {
    pub version: u8,
    pub coin: Coin,
    pub address: String,
    pub birthday: u64,
    salt: [u8; 16],
    nonce: [u8; 12],
    encrypted_key: Vec<u8>,
}

fn password_key(password: &str, salt: &[u8; 16]) -> Result<Zeroizing<[u8; 32]>> {
    let mut key = Zeroizing::new([0; 32]);
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt, &mut *key)
        .map_err(|_| anyhow::anyhow!("Password derivation failed"))?;
    Ok(key)
}

impl Wallet {
    fn aad(&self) -> String {
        format!(
            "wally:{}:{}:{}:{}",
            self.version,
            self.coin.name(),
            self.address,
            self.birthday
        )
    }

    pub fn create(coin: Coin, password: &str) -> Result<Self> {
        ensure!(
            password.chars().count() >= 12,
            "Use a password of at least 12 characters"
        );
        let mut bytes = Zeroizing::new([0; 32]);
        let key = loop {
            OsRng
                .try_fill_bytes(&mut *bytes)
                .context("OS randomness unavailable")?;
            if let Ok(key) = SecretKey::from_slice(&*bytes) {
                break key;
            }
        };
        let mut wallet = Self {
            version: 1,
            coin,
            address: coin.address(&key),
            // Account for block timestamp variation and clock drift when scanning.
            birthday: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs()
                .saturating_sub(2 * 24 * 3600),
            salt: [0; 16],
            nonce: [0; 12],
            encrypted_key: Vec::new(),
        };
        OsRng
            .try_fill_bytes(&mut wallet.salt)
            .context("OS randomness unavailable")?;
        OsRng
            .try_fill_bytes(&mut wallet.nonce)
            .context("OS randomness unavailable")?;
        let encryption_key = password_key(password, &wallet.salt)?;
        let nonce: Nonce = wallet.nonce.into();
        wallet.encrypted_key = ChaCha20Poly1305::new_from_slice(&*encryption_key)
            .map_err(|_| anyhow::anyhow!("Invalid encryption key"))?
            .encrypt(
                &nonce,
                Payload {
                    msg: &*bytes,
                    aad: wallet.aad().as_bytes(),
                },
            )
            .map_err(|_| anyhow::anyhow!("Key encryption failed"))?;
        Ok(wallet)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "Unsupported wallet format");
        ensure!(
            self.encrypted_key.len() == 48,
            "Invalid encrypted key length"
        );
        self.coin.script(&self.address)?;
        Ok(())
    }

    pub fn unlock(&self, password: &str) -> Result<SecretKey> {
        self.validate()?;
        let encryption_key = password_key(password, &self.salt)?;
        let nonce: Nonce = self.nonce.into();
        let bytes = Zeroizing::new(
            ChaCha20Poly1305::new_from_slice(&*encryption_key)
                .map_err(|_| anyhow::anyhow!("Invalid encryption key"))?
                .decrypt(
                    &nonce,
                    Payload {
                        msg: &self.encrypted_key,
                        aad: self.aad().as_bytes(),
                    },
                )
                .map_err(|_| anyhow::anyhow!("Wrong password or damaged wallet"))?,
        );
        let key = SecretKey::from_slice(&bytes)?;
        ensure!(
            self.coin.address(&key) == self.address,
            "Wallet key/address mismatch"
        );
        Ok(key)
    }
}

pub fn parse_amount(text: &str) -> Result<u64> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    ensure!(
        !whole.is_empty() && whole.bytes().all(|c| c.is_ascii_digit()),
        "Invalid amount"
    );
    ensure!(
        fraction.len() <= 8 && fraction.bytes().all(|c| c.is_ascii_digit()),
        "Use at most eight decimal places"
    );
    ensure!(!text.ends_with('.'), "Missing fractional amount");
    let whole = whole.parse::<u64>()?;
    let fractional = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u64>()?
    };
    let value = whole
        .checked_mul(100_000_000)
        .and_then(|v| v.checked_add(fractional * 10_u64.pow(8 - fraction.len() as u32)))
        .context("Amount too large")?;
    ensure!(value > 0, "Amount must be positive");
    Ok(value)
}

pub fn display_amount(value: u64) -> String {
    format!("{}.{:08}", value / 100_000_000, value % 100_000_000)
}

pub struct Utxo {
    pub outpoint: OutPoint,
    pub value: u64,
}

// ponytail: one reused P2PKH address and sequential coin selection; add HD addresses
// and smarter selection when privacy or large UTXO sets become requirements.
pub fn plan(
    utxos: &[Utxo],
    own: &ScriptBuf,
    destination: &ScriptBuf,
    amount: u64,
    fee_per_kb: u64,
    dust: u64,
) -> Result<(Transaction, u64)> {
    ensure!(amount >= dust, "Amount is below the dust limit");
    ensure!(fee_per_kb > 0, "Invalid fee estimate");
    let mut tx = Transaction {
        version: transaction::Version::ONE,
        lock_time: absolute::LockTime::ZERO,
        input: Vec::new(),
        output: vec![
            TxOut {
                value: Amount::from_sat(amount),
                script_pubkey: destination.clone(),
            },
            TxOut {
                value: Amount::ZERO,
                script_pubkey: own.clone(),
            },
        ],
    };
    let mut total = 0_u64;
    let mut seen = std::collections::HashSet::new();
    for utxo in utxos {
        ensure!(seen.insert(utxo.outpoint), "Duplicate UTXO from provider");
        total = total
            .checked_add(utxo.value)
            .context("UTXO total overflow")?;
        tx.input.push(TxIn {
            previous_output: utxo.outpoint,
            sequence: Sequence::MAX,
            witness: Witness::default(),
            // Reserve the maximum DER signature + sighash and compressed public key size.
            script_sig: Builder::new()
                .push_slice([0_u8; 73])
                .push_slice([0_u8; 33])
                .into_script(),
        });
        let size = consensus::serialize(&tx).len() as u64;
        let fee = fee_per_kb
            .checked_mul(size)
            .context("Fee overflow")?
            .div_ceil(1000);
        let required = amount
            .checked_add(fee)
            .context("Amount plus fee overflow")?;
        if total < required {
            continue;
        }
        let change = total - required;
        let actual_fee = if change >= dust {
            tx.output[1].value = Amount::from_sat(change);
            fee
        } else {
            tx.output.pop();
            total - amount
        };
        for input in &mut tx.input {
            input.script_sig = ScriptBuf::new();
        }
        return Ok((tx, actual_fee));
    }
    bail!("Insufficient confirmed funds, including the network fee")
}

pub fn sign(tx: &mut Transaction, own: &ScriptBuf, key: &SecretKey) -> Result<()> {
    let secp = Secp256k1::new();
    let public = PublicKey::from_secret_key(&secp, key);
    for index in 0..tx.input.len() {
        let hash = SighashCache::new(&*tx).legacy_signature_hash(
            index,
            own,
            EcdsaSighashType::All.to_u32(),
        )?;
        let signature = secp.sign_ecdsa(&Message::from_digest(hash.to_byte_array()), key);
        let mut signature = signature.serialize_der().to_vec();
        signature.push(EcdsaSighashType::All.to_u32() as u8);
        let signature = bitcoin::script::PushBytesBuf::try_from(signature)?;
        tx.input[index].script_sig = Builder::new()
            .push_slice(signature)
            .push_slice(public.serialize())
            .into_script();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{Txid, script::Instruction};

    #[test]
    fn wallet_and_transaction_self_check() -> Result<()> {
        for invalid in [
            "0",
            "-1",
            "NaN",
            "1e8",
            "1.000000001",
            "1.",
            ".1",
            "1.2.3",
            "18446744073709551615",
        ] {
            assert!(parse_amount(invalid).is_err(), "{invalid}");
        }
        assert_eq!(parse_amount("0.00000001")?, 1);
        assert_eq!(parse_amount("1.23")?, 123_000_000);
        let password = "a long test password";
        for coin in [Coin::Bitcoin, Coin::Dogecoin] {
            let wallet = Wallet::create(coin, password)?;
            let wallet: Wallet = serde_json::from_slice(&serde_json::to_vec(&wallet)?)?;
            assert!(wallet.unlock("incorrect password").is_err());
            let key = wallet.unlock(password)?;
            let own = coin.script(&wallet.address)?;
            let other_coin = if coin == Coin::Bitcoin {
                Coin::Dogecoin
            } else {
                Coin::Bitcoin
            };
            assert!(other_coin.script(&wallet.address).is_err());
            let amount = 100_000_000;
            let utxos = [Utxo {
                outpoint: OutPoint {
                    txid: Txid::from_byte_array([1; 32]),
                    vout: 0,
                },
                value: 200_000_000,
            }];
            let rate = if coin == Coin::Dogecoin {
                1_000_000
            } else {
                1000
            };
            let (mut tx, fee) = plan(&utxos, &own, &own, amount, rate, coin.dust())?;
            assert_eq!(
                tx.output.iter().map(|o| o.value.to_sat()).sum::<u64>() + fee,
                utxos[0].value
            );
            sign(&mut tx, &own, &key)?;
            assert!(fee >= (consensus::serialize(&tx).len() as u64 * rate).div_ceil(1000));
            let hash = SighashCache::new(&tx).legacy_signature_hash(0, &own, 1)?;
            let pushes: Vec<_> = tx.input[0].script_sig.instructions().collect();
            let Some(Ok(Instruction::PushBytes(bytes))) = pushes.first() else {
                panic!("missing signature")
            };
            let bytes = bytes.as_bytes();
            assert_eq!(bytes.last(), Some(&1));
            let signature =
                bitcoin::secp256k1::ecdsa::Signature::from_der(&bytes[..bytes.len() - 1])?;
            Secp256k1::new().verify_ecdsa(
                &Message::from_digest(hash.to_byte_array()),
                &signature,
                &PublicKey::from_secret_key(&Secp256k1::new(), &key),
            )?;
            let decoded: Transaction = consensus::deserialize(&consensus::serialize(&tx))?;
            assert_eq!(decoded, tx);
            assert!(plan(&[], &own, &own, amount, rate, coin.dust()).is_err());
            assert!(plan(&utxos, &own, &own, coin.dust() - 1, rate, coin.dust()).is_err());
            let tiny_change = [Utxo {
                outpoint: utxos[0].outpoint,
                value: amount + fee + 1,
            }];
            let (no_change, actual_fee) =
                plan(&tiny_change, &own, &own, amount, rate, coin.dust())?;
            assert_eq!(no_change.output.len(), 1);
            assert_eq!(actual_fee, fee + 1);
            let mut tampered = wallet;
            tampered.birthday += 1;
            assert!(tampered.unlock(password).is_err());
            tampered.birthday -= 1;
            tampered.address = coin.address(&SecretKey::from_slice(&[2; 32])?);
            assert!(tampered.unlock(password).is_err());
        }
        Ok(())
    }
}
