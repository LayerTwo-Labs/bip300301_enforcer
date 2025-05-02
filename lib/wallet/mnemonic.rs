use std::str::FromStr;

use aes_gcm::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    Aes256Gcm, Key, Nonce,
};
use argon2::Argon2;
use bdk_wallet::{
    bip39::{Language, Mnemonic},
    keys::{bip39::WordCount, GeneratableKey, GeneratedKey},
    miniscript::miniscript,
};
use miette::{miette, IntoDiagnostic, Result};

/// Create a cryptographically secure mnemonic.
pub(crate) fn new_mnemonic() -> Result<Mnemonic, miette::Report> {
    // This is cribbed from the official docs: https://bitcoindevkit.org/getting-started/

    let options = (WordCount::Words12, Language::English);
    let generated: GeneratedKey<_, miniscript::Segwitv0> =
        Mnemonic::generate_with_aux_rand(options, &mut OsRng)
            .map_err(|err| miette!("failed to generate mnemonic: {:#}", err.unwrap()))?;

    let words = generated.to_string();
    Mnemonic::parse(words).into_diagnostic()
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
    pub(crate) fn encrypt(mnemonic: &Mnemonic, password: &str) -> Result<Self, miette::Report> {
        use rand::Rng;
        let key_salt = OsRng.gen::<[u8; 16]>();

        let key_bytes = stretch_password(password, &key_salt)?;
        let key = Key::<Aes256Gcm>::from_slice(&key_bytes);

        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let cipher = Aes256Gcm::new(key);

        let ciphered_data = cipher
            .encrypt(&nonce, mnemonic.to_string().as_bytes())
            .map_err(|err| miette::miette!("failed to encrypt mnemonic: {err:#}"))?;

        Ok(Self {
            initialization_vector: nonce.to_vec(),
            ciphertext_mnemonic: ciphered_data,
            key_salt: key_salt.to_vec(),
        })
    }

    pub(crate) fn decrypt(&self, password: &str) -> Result<Mnemonic, miette::Report> {
        let nonce = Nonce::from_slice(self.initialization_vector.as_ref());

        let key_bytes = stretch_password(password, self.key_salt.as_ref())?;
        let key = Key::<Aes256Gcm>::from_slice(&key_bytes);

        let cipher = Aes256Gcm::new(key);

        let plaintext = cipher
            .decrypt(nonce, self.ciphertext_mnemonic.as_ref())
            .map_err(|err| miette::miette!("failed to decrypt mnemonic: {err:#}"))?;

        let raw_mnemonic = String::from_utf8(plaintext)
            .map_err(|err| miette::miette!("failed to convert mnemonic to string: {err:#}"))?;

        Mnemonic::from_str(&raw_mnemonic)
            .map_err(|err| miette::miette!("failed to parse mnemonic: {err:#}"))
    }
}

fn stretch_password(password: &str, key_salt: &[u8]) -> Result<[u8; 32], miette::Report> {
    let mut key_bytes = [0u8; 32];
    Argon2::default()
        .hash_password_into(password.as_bytes(), key_salt, &mut key_bytes)
        .map_err(|err| miette::miette!("failed to stretch password into AES key: {err:#}"))?;

    Ok(key_bytes)
}
