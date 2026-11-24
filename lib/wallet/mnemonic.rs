use std::str::FromStr;

use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, KeyInit},
};
use argon2::{Algorithm, Argon2, Params, Version};
use bdk_wallet::{
    bip39::{Language, Mnemonic},
    keys::{GeneratableKey, GeneratedKey, bip39::WordCount},
    miniscript::miniscript,
};
use rand::{RngExt as _, rand_core::CryptoRng};
use zeroize::Zeroizing;

use crate::wallet::error;

/// Create a cryptographically secure mnemonic.
pub(crate) fn new_mnemonic<R>(rng: &mut R) -> Result<Mnemonic, bdk_wallet::bip39::Error>
where
    R: CryptoRng,
{
    // This is cribbed from the official docs: https://bitcoindevkit.org/getting-started/
    let options = (WordCount::Words12, Language::English);
    let entropy: <Mnemonic as GeneratableKey<miniscript::Segwitv0>>::Entropy = {
        let mut entropy = [0u8; 32];
        rng.fill(&mut entropy);
        entropy
    };
    let generated: GeneratedKey<_, miniscript::Segwitv0> =
        Mnemonic::generate_with_entropy(options, entropy).map_err(|err| err.unwrap())?;

    let words = generated.to_string();
    Mnemonic::parse(words)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KdfParams {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

impl KdfParams {
    /// What seeds encrypted before the cost was recorded used.
    pub(in crate::wallet) const LEGACY: Self = Self {
        memory_kib: 19 * 1024,
        iterations: 2,
        parallelism: 1,
    };

    /// What newly encrypted seeds use.
    pub const CURRENT: Self = Self {
        memory_kib: 64 * 1024,
        iterations: 3,
        parallelism: 1,
    };
}

fn stretch_password(
    password: &str,
    key_salt: &[u8],
    kdf: KdfParams,
) -> Result<Zeroizing<[u8; 32]>, error::StretchPassword> {
    let mut key_bytes = Zeroizing::new([0u8; 32]);
    let params = Params::new(
        kdf.memory_kib,
        kdf.iterations,
        kdf.parallelism,
        Some(key_bytes.len()),
    )?;
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params).hash_password_into(
        password.as_bytes(),
        key_salt,
        &mut key_bytes[..],
    )?;
    Ok(key_bytes)
}

/// Encrypted with AES-256-GCM. Password is stretched
/// with argon2 to 32 bytes, before being used as the key.
pub struct EncryptedMnemonic {
    pub initialization_vector: [u8; 12],
    pub ciphertext_mnemonic: Vec<u8>,
    pub key_salt: Vec<u8>,
    /// The cost that `key_salt` was stretched with
    pub kdf: KdfParams,
}

// Encryption/decryption is based off of this blog post, with the addition of the argon2 key stretch.
// https://backendengineer.io/aes-encryption-rust
impl EncryptedMnemonic {
    pub fn encrypt<R>(
        rng: &mut R,
        mnemonic: &Mnemonic,
        password: &str,
        kdf: KdfParams,
    ) -> Result<Self, error::EncryptMnemonic>
    where
        R: CryptoRng,
    {
        let key_salt = {
            let mut key_salt = [0u8; 16];
            rng.fill(&mut key_salt);
            Zeroizing::new(key_salt)
        };

        let key = {
            let key_bytes = stretch_password(password, key_salt.as_slice(), kdf)?;
            Key::<Aes256Gcm>::from(*key_bytes)
        };

        let nonce = {
            let mut nonce = aes_gcm::Nonce::default();
            rng.fill(nonce.as_mut_slice());
            nonce
        };
        let cipher = Aes256Gcm::new(&key);

        // `Mnemonic`'s `Display` hands out the seed words, so the copy it
        // allocates has to be wiped once the ciphertext exists.
        let plaintext = Zeroizing::new(mnemonic.to_string());
        let ciphered_data = cipher.encrypt(&nonce, plaintext.as_bytes())?;

        Ok(Self {
            initialization_vector: nonce.0,
            ciphertext_mnemonic: ciphered_data,
            key_salt: key_salt.to_vec(),
            kdf,
        })
    }

    pub(crate) fn decrypt(&self, password: &str) -> Result<Mnemonic, error::DecryptMnemonic> {
        let nonce = Nonce::from(self.initialization_vector);

        let key = {
            let key_bytes = stretch_password(password, self.key_salt.as_ref(), self.kdf)?;
            Key::<Aes256Gcm>::from(*key_bytes)
        };

        let cipher = Aes256Gcm::new(&key);

        let plaintext = Zeroizing::new(cipher.decrypt(&nonce, self.ciphertext_mnemonic.as_ref())?);

        // `from_utf8` reuses the buffer it is handed, so wiping the `String`
        // wipes that copy, and `plaintext` wipes the one it was cloned from.
        let raw_mnemonic = Zeroizing::new(String::from_utf8(plaintext.to_vec())?);

        Mnemonic::from_str(&raw_mnemonic).map_err(|err| error::ParseMnemonic::from(err).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_mnemonic_is_12_words_and_parseable() {
        let mnemonic = new_mnemonic(&mut rand::rng()).expect("mnemonic generation");
        let words: Vec<&str> = mnemonic.words().collect();
        assert_eq!(words.len(), 12);
        Mnemonic::parse(mnemonic.to_string()).expect("round-trip parse");
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let mut rng = rand::rng();
        let mnemonic = new_mnemonic(&mut rng).unwrap();
        let encrypted =
            EncryptedMnemonic::encrypt(&mut rng, &mnemonic, "hunter2", KdfParams::CURRENT).unwrap();
        let decrypted = encrypted.decrypt("hunter2").unwrap();
        assert_eq!(mnemonic.to_string(), decrypted.to_string());
    }

    #[test]
    fn decrypt_with_wrong_password_fails() {
        let mut rng = rand::rng();
        let mnemonic = new_mnemonic(&mut rng).unwrap();
        let encrypted =
            EncryptedMnemonic::encrypt(&mut rng, &mnemonic, "correct", KdfParams::CURRENT).unwrap();
        assert!(encrypted.decrypt("wrong").is_err());
    }

    #[test]
    fn fresh_ciphertexts_use_the_current_kdf_cost() {
        assert_ne!(
            KdfParams::CURRENT,
            KdfParams::LEGACY,
            "CURRENT must actually raise the cost over the old default"
        );
        let mut rng = rand::rng();
        let mnemonic = new_mnemonic(&mut rng).unwrap();
        let encrypted =
            EncryptedMnemonic::encrypt(&mut rng, &mnemonic, "pw", KdfParams::CURRENT).unwrap();
        assert_eq!(encrypted.kdf, KdfParams::CURRENT);
    }

    /// A seed encrypted under the pre-bump cost must keep opening, and must
    /// only open under the cost it was written with — proof that the stored
    /// params are what drives the stretch, not a hardcoded constant.
    #[test]
    fn legacy_kdf_cost_still_decrypts() {
        let mut rng = rand::rng();
        let mnemonic = new_mnemonic(&mut rng).unwrap();
        let legacy =
            EncryptedMnemonic::encrypt(&mut rng, &mnemonic, "pw", KdfParams::LEGACY).unwrap();
        assert_eq!(legacy.kdf, KdfParams::LEGACY);
        assert_eq!(
            legacy.decrypt("pw").unwrap().to_string(),
            mnemonic.to_string()
        );

        let mismatched = EncryptedMnemonic {
            initialization_vector: legacy.initialization_vector,
            ciphertext_mnemonic: legacy.ciphertext_mnemonic,
            key_salt: legacy.key_salt,
            kdf: KdfParams::CURRENT,
        };
        assert!(
            mismatched.decrypt("pw").is_err(),
            "the recorded cost must be load-bearing"
        );
    }

    #[test]
    fn encrypting_same_input_twice_yields_fresh_salt_and_iv() {
        let mut rng = rand::rng();
        let mnemonic = new_mnemonic(&mut rng).unwrap();
        let a = EncryptedMnemonic::encrypt(&mut rng, &mnemonic, "pw", KdfParams::CURRENT).unwrap();
        let b = EncryptedMnemonic::encrypt(&mut rng, &mnemonic, "pw", KdfParams::CURRENT).unwrap();
        assert_ne!(a.key_salt, b.key_salt);
        assert_ne!(a.initialization_vector, b.initialization_vector);
        assert_ne!(a.ciphertext_mnemonic, b.ciphertext_mnemonic);
    }
}
