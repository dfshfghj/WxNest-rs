use aes::{Aes128, Aes256};
use cipher::generic_array::GenericArray;
use cipher::{BlockDecrypt, BlockEncrypt, KeyInit};

use crate::error::{CoreError, Result};

const BLOCK_SIZE: usize = 16;

pub fn aes_ecb_decrypt_128(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let cipher = new_aes128(key)?;
    decrypt_ecb(&cipher, data)
}

pub fn aes_ecb_encrypt_128(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let cipher = new_aes128(key)?;
    encrypt_ecb(&cipher, data)
}

pub fn aes_cbc_decrypt_128(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let cipher = new_aes128(key)?;
    decrypt_cbc(&cipher, iv, data)
}

pub fn aes_cbc_encrypt_128(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let cipher = new_aes128(key)?;
    encrypt_cbc(&cipher, iv, data)
}

pub fn aes_cbc_decrypt_256(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let cipher = new_aes256(key)?;
    decrypt_cbc(&cipher, iv, data)
}

pub fn aes_cbc_encrypt_256(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let cipher = new_aes256(key)?;
    encrypt_cbc(&cipher, iv, data)
}

pub fn pkcs7_pad(data: &[u8], block_size: usize) -> Vec<u8> {
    let padding = block_size - (data.len() % block_size);
    let mut output = Vec::with_capacity(data.len() + padding);
    output.extend_from_slice(data);
    output.extend(std::iter::repeat(padding as u8).take(padding));
    output
}

pub fn pkcs7_unpad(data: &[u8], block_size: usize) -> Result<Vec<u8>> {
    if data.is_empty() || data.len() % block_size != 0 {
        return Err(CoreError::Crypto("invalid pkcs7 payload length".into()));
    }
    let padding = *data.last().expect("non-empty") as usize;
    if padding == 0 || padding > block_size || padding > data.len() {
        return Err(CoreError::Crypto("invalid pkcs7 padding".into()));
    }
    if !data[data.len() - padding..]
        .iter()
        .all(|byte| *byte as usize == padding)
    {
        return Err(CoreError::Crypto("invalid pkcs7 padding".into()));
    }
    Ok(data[..data.len() - padding].to_vec())
}

fn new_aes128(key: &[u8]) -> Result<Aes128> {
    if key.len() != 16 {
        return Err(CoreError::Crypto(format!(
            "AES-128 key must be 16 bytes, got {}",
            key.len()
        )));
    }
    Aes128::new_from_slice(key).map_err(|_| CoreError::Crypto("invalid AES-128 key".into()))
}

fn new_aes256(key: &[u8]) -> Result<Aes256> {
    if key.len() != 32 {
        return Err(CoreError::Crypto(format!(
            "AES-256 key must be 32 bytes, got {}",
            key.len()
        )));
    }
    Aes256::new_from_slice(key).map_err(|_| CoreError::Crypto("invalid AES-256 key".into()))
}

fn decrypt_ecb<C>(cipher: &C, data: &[u8]) -> Result<Vec<u8>>
where
    C: BlockDecrypt,
{
    if data.len() % BLOCK_SIZE != 0 {
        return Err(CoreError::Crypto("ECB payload is not block aligned".into()));
    }
    let mut output = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(BLOCK_SIZE) {
        let mut block = GenericArray::clone_from_slice(chunk);
        cipher.decrypt_block(&mut block);
        output.extend_from_slice(&block);
    }
    Ok(output)
}

fn encrypt_ecb<C>(cipher: &C, data: &[u8]) -> Result<Vec<u8>>
where
    C: BlockEncrypt,
{
    if data.len() % BLOCK_SIZE != 0 {
        return Err(CoreError::Crypto("ECB payload is not block aligned".into()));
    }
    let mut output = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(BLOCK_SIZE) {
        let mut block = GenericArray::clone_from_slice(chunk);
        cipher.encrypt_block(&mut block);
        output.extend_from_slice(&block);
    }
    Ok(output)
}

fn decrypt_cbc<C>(cipher: &C, iv: &[u8], data: &[u8]) -> Result<Vec<u8>>
where
    C: BlockDecrypt,
{
    if iv.len() != BLOCK_SIZE {
        return Err(CoreError::Crypto(format!(
            "AES IV must be 16 bytes, got {}",
            iv.len()
        )));
    }
    if data.len() % BLOCK_SIZE != 0 {
        return Err(CoreError::Crypto("CBC payload is not block aligned".into()));
    }
    let mut previous = [0u8; BLOCK_SIZE];
    previous.copy_from_slice(iv);
    let mut output = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(BLOCK_SIZE) {
        let mut block = GenericArray::clone_from_slice(chunk);
        cipher.decrypt_block(&mut block);
        let mut plain = [0u8; BLOCK_SIZE];
        for index in 0..BLOCK_SIZE {
            plain[index] = block[index] ^ previous[index];
        }
        output.extend_from_slice(&plain);
        previous.copy_from_slice(chunk);
    }
    Ok(output)
}

fn encrypt_cbc<C>(cipher: &C, iv: &[u8], data: &[u8]) -> Result<Vec<u8>>
where
    C: BlockEncrypt,
{
    if iv.len() != BLOCK_SIZE {
        return Err(CoreError::Crypto(format!(
            "AES IV must be 16 bytes, got {}",
            iv.len()
        )));
    }
    if data.len() % BLOCK_SIZE != 0 {
        return Err(CoreError::Crypto("CBC payload is not block aligned".into()));
    }
    let mut previous = [0u8; BLOCK_SIZE];
    previous.copy_from_slice(iv);
    let mut output = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(BLOCK_SIZE) {
        let mut block = GenericArray::clone_from_slice(chunk);
        for index in 0..BLOCK_SIZE {
            block[index] ^= previous[index];
        }
        cipher.encrypt_block(&mut block);
        output.extend_from_slice(&block);
        previous.copy_from_slice(&block);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes256_cbc_round_trip() {
        let key = [0x11u8; 32];
        let iv = [0x22u8; 16];
        let plain = [0x33u8; 64];
        let encrypted = aes_cbc_encrypt_256(&key, &iv, &plain).unwrap();
        let decrypted = aes_cbc_decrypt_256(&key, &iv, &encrypted).unwrap();
        assert_eq!(decrypted, plain);
    }

    #[test]
    fn aes128_ecb_round_trip() {
        let key = b"0123456789abcdef";
        let plain = [0x44u8; 32];
        let encrypted = aes_ecb_encrypt_128(key, &plain).unwrap();
        let decrypted = aes_ecb_decrypt_128(key, &encrypted).unwrap();
        assert_eq!(decrypted, plain);
    }

    #[test]
    fn pkcs7_round_trip() {
        for length in 0..48 {
            let data = vec![0x5Au8; length];
            let padded = pkcs7_pad(&data, 16);
            assert_eq!(padded.len() % 16, 0);
            assert_eq!(pkcs7_unpad(&padded, 16).unwrap(), data);
        }
    }
}
