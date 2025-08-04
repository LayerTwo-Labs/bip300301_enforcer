use std::str::FromStr;

use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, AeadCore, KeyInit, OsRng},
};
use argon2::Argon2;
use bdk_wallet::{
    bip39::{Language, Mnemonic},
    keys::{GeneratableKey, GeneratedKey, bip39::WordCount},
    miniscript::miniscript,
};

use crate::wallet::error;

/// Create a cryptographically secure mnemonic.
pub(crate) fn new_mnemonic() -> Result<Mnemonic, bdk_wallet::bip39::Error> {
    // This is cribbed from the official docs: https://bitcoindevkit.org/getting-started/

    let options = (WordCount::Words12, Language::English);
    let generated: GeneratedKey<_, miniscript::Segwitv0> =
        Mnemonic::generate_with_aux_rand(options, &mut OsRng).map_err(|err| err.unwrap())?;

    let words = generated.to_string();
    Mnemonic::parse(words)
}

fn stretch_password(password: &str, key_salt: &[u8]) -> Result<[u8; 32], error::StretchPassword> {
    let mut key_bytes = [0u8; 32];
    Argon2::default().hash_password_into(password.as_bytes(), key_salt, &mut key_bytes)?;
    Ok(key_bytes)
}

/// Encrypted with AES-256-GCM. Password is stretched
/// with argon2 to 32 bytes, before being used as the key.
pub(crate) struct EncryptedMnemonic {
    pub initialization_vector: Vec<u8>,
    pub ciphertext_mnemonic: Vec<u8>,
    pub key_salt: Vec<u8>,
}

// Encryption/decryption is based off of this blog post, with the addition of the argon2 key stretch.
// https://backendengineer.io/aes-encryption-rust
impl EncryptedMnemonic {
    pub(crate) fn encrypt(
        mnemonic: &Mnemonic,
        password: &str,
    ) -> Result<Self, error::EncryptMnemonic> {
        use rand::Rng;
        let key_salt = OsRng.r#gen::<[u8; 16]>();

        let key_bytes = stretch_password(password, &key_salt)?;
        let key = Key::<Aes256Gcm>::from_slice(&key_bytes);

        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let cipher = Aes256Gcm::new(key);

        let ciphered_data = cipher.encrypt(&nonce, mnemonic.to_string().as_bytes())?;

        Ok(Self {
            initialization_vector: nonce.to_vec(),
            ciphertext_mnemonic: ciphered_data,
            key_salt: key_salt.to_vec(),
        })
    }

    pub(crate) fn decrypt(&self, password: &str) -> Result<Mnemonic, error::DecryptMnemonic> {
        let nonce = Nonce::from_slice(self.initialization_vector.as_ref());

        let key_bytes = stretch_password(password, self.key_salt.as_ref())?;
        let key = Key::<Aes256Gcm>::from_slice(&key_bytes);

        let cipher = Aes256Gcm::new(key);

        let plaintext = cipher.decrypt(nonce, self.ciphertext_mnemonic.as_ref())?;

        let raw_mnemonic = String::from_utf8(plaintext)?;

        Mnemonic::from_str(&raw_mnemonic).map_err(|err| error::ParseMnemonic::from(err).into())
    }
}
