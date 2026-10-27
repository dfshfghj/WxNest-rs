use hmac::{Hmac, Mac};
use sha2::Sha512;

use crate::error::{CoreError, Result};

pub const SQLCIPHER_ITERATIONS: u32 = 256_000;
pub const KEY_SIZE: usize = 32;

type HmacSha512 = Hmac<Sha512>;

pub fn pbkdf2_sha512(password: &[u8], salt: &[u8], rounds: u32, key_len: usize) -> Result<Vec<u8>> {
    let mut output = vec![0u8; key_len];
    pbkdf2::pbkdf2::<HmacSha512>(password, salt, rounds, &mut output)
        .map_err(|err| CoreError::Crypto(format!("pbkdf2: {err}")))?;
    Ok(output)
}

pub fn derive_enc_key(key_material: &[u8], salt: &[u8]) -> Result<[u8; KEY_SIZE]> {
    let derived = pbkdf2_sha512(key_material, salt, SQLCIPHER_ITERATIONS, KEY_SIZE)?;
    fixed_key(&derived)
}

pub fn derive_mac_key(enc_key: &[u8], salt: &[u8]) -> Result<[u8; KEY_SIZE]> {
    let mac_salt: Vec<u8> = salt.iter().map(|byte| byte ^ 0x3A).collect();
    let derived = pbkdf2_sha512(enc_key, &mac_salt, 2, KEY_SIZE)?;
    fixed_key(&derived)
}

pub fn hmac_sha512(key: &[u8], data: &[u8]) -> [u8; 64] {
    hmac_sha512_parts(key, &[data])
}

pub fn hmac_sha512_parts(key: &[u8], parts: &[&[u8]]) -> [u8; 64] {
    let mut mac = HmacSha512::new_from_slice(key).expect("HMAC-SHA512 accepts keys of any length");
    for part in parts {
        mac.update(part);
    }
    let digest = mac.finalize().into_bytes();
    let mut output = [0u8; 64];
    output.copy_from_slice(&digest);
    output
}

fn fixed_key(value: &[u8]) -> Result<[u8; KEY_SIZE]> {
    value
        .try_into()
        .map_err(|_| CoreError::Crypto(format!("expected {KEY_SIZE}-byte key")))
}
