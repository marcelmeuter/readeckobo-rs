use std::path::Path;

use aes::Aes128;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use cipher::block_padding::Pkcs7;
use cipher::{BlockEncryptMut, KeyInit};
use sha2::{Digest, Sha256};

const STATIC_SALT: &str = "88b3a2e13";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid AES key")]
    InvalidKey,
    #[error("encryption failed")]
    Encryption,
    #[error("decryption failed")]
    Decryption,
    #[error("decrypted token is not valid UTF-8")]
    Utf8,
    #[error("failed to read the Kobo version file: {0}")]
    ReadVersion(#[from] std::io::Error),
    #[error("could not find a serial number in .kobo/version")]
    MissingSerial,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub plaintext: String,
    pub encrypted: String,
}

pub fn generate(serial: &str) -> Result<Token, Error> {
    let plaintext = uuid::Uuid::new_v4().to_string();
    let key = derive_key(serial);
    let encrypted = encrypt(&plaintext, &key)?;
    Ok(Token {
        plaintext,
        encrypted,
    })
}

fn derive_key(serial: &str) -> [u8; 16] {
    let digest = Sha256::digest(format!("{STATIC_SALT}{serial}").as_bytes());
    let hex = hex::encode(digest);
    let mut key = [0u8; 16];
    key.copy_from_slice(&hex.as_bytes()[..16]);
    key
}

fn encrypt(plaintext: &str, key: &[u8; 16]) -> Result<String, Error> {
    let cipher = Aes128::new_from_slice(key).map_err(|_| Error::InvalidKey)?;
    let message = plaintext.as_bytes();
    let mut buffer = vec![0u8; message.len() + 16];
    buffer[..message.len()].copy_from_slice(message);
    let encrypted = cipher
        .encrypt_padded_mut::<Pkcs7>(&mut buffer, message.len())
        .map_err(|_| Error::Encryption)?;
    Ok(STANDARD.encode(encrypted))
}

pub fn print(serial: &str, public_url: Option<&str>) -> Result<(), Error> {
    let token = generate(serial)?;
    println!();
    println!("✅ New token generated successfully.");
    println!("----------------------------------------------------------------");
    println!();
    println!("1. Add the PLAIN TEXT token to readeckobo's config.yaml:");
    println!();
    println!("users:");
    println!("  - token: \"{}\"", token.plaintext);
    println!("    readeck_access_token: \"<YOUR-READECK-API-TOKEN>\"");
    println!();
    println!("----------------------------------------------------------------");
    println!();
    println!("2. Add these settings to the Kobo's .kobo/Kobo/Kobo eReader.conf:");
    println!();
    if let Some(public_url) = public_url {
        let public_url = public_url.trim_end_matches('/');
        println!("[OneStoreServices]");
        println!("api_endpoint={public_url}/storeapi");
        println!("instapaper_env_url={public_url}/instapaper/api/kobo");
        println!();
    }
    println!("[Instapaper]");
    println!("AccessToken=@ByteArray({})", token.encrypted);
    println!();
    println!("----------------------------------------------------------------");
    Ok(())
}

/// Read the Kobo serial number from a mounted device's `.kobo/version` file.
pub fn read_serial(kobo_path: &Path) -> Result<String, Error> {
    let contents = std::fs::read_to_string(kobo_path.join(".kobo/version"))?;
    parse_serial(&contents)
        .map(str::to_string)
        .ok_or(Error::MissingSerial)
}

fn parse_serial(contents: &str) -> Option<&str> {
    contents
        .split(',')
        .next()
        .map(str::trim)
        .filter(|serial| !serial.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cipher::BlockDecryptMut;

    #[test]
    fn derives_key_from_salt_and_serial() {
        let key = derive_key("N123456789");
        let digest = Sha256::digest(format!("{STATIC_SALT}N123456789").as_bytes());
        let expected = hex::encode(digest);
        assert_eq!(&key[..], &expected.as_bytes()[..16]);
    }

    #[test]
    fn parses_serial_from_version_file() {
        assert_eq!(
            parse_serial("N123456789,4.38.21908,4.38.21908,0"),
            Some("N123456789")
        );
        assert_eq!(parse_serial(",4.38.0"), None);
        assert_eq!(parse_serial(""), None);
    }

    #[test]
    fn generated_token_round_trips() {
        let token = generate("N123456789").unwrap();
        assert!(uuid::Uuid::parse_str(&token.plaintext).is_ok());

        let key = derive_key("N123456789");
        let ciphertext = STANDARD.decode(&token.encrypted).unwrap();
        let cipher = <Aes128 as KeyInit>::new_from_slice(&key).unwrap();
        let plaintext = cipher.decrypt_padded_vec_mut::<Pkcs7>(&ciphertext).unwrap();
        assert_eq!(String::from_utf8(plaintext).unwrap(), token.plaintext);
    }
}
