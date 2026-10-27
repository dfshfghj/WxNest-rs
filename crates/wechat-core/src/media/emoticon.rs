use crate::crypto::aes::{aes_cbc_decrypt_128, pkcs7_unpad};

pub fn decrypt_emoticon_aes_cbc(data: &[u8], aes_key_hex: &str) -> Option<Vec<u8>> {
    if data.is_empty() || data.len() % 16 != 0 {
        return None;
    }
    let normalized = aes_key_hex.trim().to_ascii_lowercase();
    if normalized.len() != 32 || !normalized.chars().all(|value| value.is_ascii_hexdigit()) {
        return None;
    }
    let key = hex::decode(normalized).ok()?;
    if key.len() != 16 {
        return None;
    }
    let padded = aes_cbc_decrypt_128(&key, &key, data).ok()?;
    pkcs7_unpad(&padded, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::aes::{aes_cbc_encrypt_128, pkcs7_pad};

    #[test]
    fn round_trip_with_key_as_iv() {
        let key = b"0123456789abcdef";
        let plain = b"emoticon payload";
        let padded = pkcs7_pad(plain, 16);
        let encrypted = aes_cbc_encrypt_128(key, key, &padded).unwrap();
        let hex_key = hex::encode(key);
        assert_eq!(
            decrypt_emoticon_aes_cbc(&encrypted, &hex_key).unwrap(),
            plain
        );
    }
}
