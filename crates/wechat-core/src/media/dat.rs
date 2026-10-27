use crate::crypto::aes::{aes_ecb_decrypt_128, pkcs7_unpad};
use crate::error::{CoreError, Result};

pub const DAT_V1_MAGIC: &[u8; 6] = b"\x07\x08V1\x08\x07";
pub const DAT_V2_MAGIC: &[u8; 6] = b"\x07\x08V2\x08\x07";
pub const DAT_V1_AES_KEY: &[u8; 16] = b"cfcd208495d565ef";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatVersion {
    Unknown,
    V0,
    V1,
    V2,
}

impl DatVersion {
    pub fn as_i32(self) -> i32 {
        match self {
            Self::Unknown => -1,
            Self::V0 => 0,
            Self::V1 => 1,
            Self::V2 => 2,
        }
    }

    pub fn from_i32(value: i32) -> Self {
        match value {
            0 => Self::V0,
            1 => Self::V1,
            2 => Self::V2,
            _ => Self::Unknown,
        }
    }
}

pub fn detect_dat_version(data: &[u8]) -> DatVersion {
    if data.len() < 6 {
        return DatVersion::Unknown;
    }
    if &data[..6] == DAT_V1_MAGIC {
        DatVersion::V1
    } else if &data[..6] == DAT_V2_MAGIC {
        DatVersion::V2
    } else {
        DatVersion::V0
    }
}

pub fn decrypt_dat_v3(data: &[u8], xor_key: u8) -> Vec<u8> {
    data.iter().map(|byte| byte ^ xor_key).collect()
}

pub fn decrypt_dat_v4(data: &[u8], xor_key: u8, aes_key: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 15 {
        return Err(CoreError::Media("DAT v4 header is too short".into()));
    }
    if aes_key.len() < 16 {
        return Err(CoreError::Media(
            "DAT v4 AES key is shorter than 16 bytes".into(),
        ));
    }

    let aes_size_raw = u32::from_le_bytes(data[6..10].try_into().expect("4 bytes")) as u64;
    let xor_size = u32::from_le_bytes(data[10..14].try_into().expect("4 bytes")) as usize;

    let aes_size = aes_size_raw + 16 - (aes_size_raw % 16);
    let aes_size = usize::try_from(aes_size)
        .map_err(|_| CoreError::Media("DAT v4 AES size is too large".into()))?;

    let rest = &data[15..];
    let aes_end = aes_size.min(rest.len());
    let aes_data = &rest[..aes_end];
    let decrypted = pkcs7_unpad(&aes_ecb_decrypt_128(&aes_key[..16], aes_data)?, 16)?;

    let tail_start = rest.len().saturating_sub(xor_size);
    let (raw_data, xor_data) = if tail_start < aes_end {
        (&rest[rest.len()..], &rest[tail_start..])
    } else {
        (&rest[aes_end..tail_start], &rest[tail_start..])
    };
    let xored = decrypt_dat_v3(xor_data, xor_key);

    let mut output = Vec::with_capacity(decrypted.len() + raw_data.len() + xored.len());
    output.extend_from_slice(&decrypted);
    output.extend_from_slice(raw_data);
    output.extend_from_slice(&xored);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::aes::{aes_ecb_encrypt_128, pkcs7_pad};

    fn build_v4(plain: &[u8], _xor: u8, aes_key: &[u8; 16], magic: &[u8; 6]) -> Vec<u8> {
        let padded = pkcs7_pad(plain, 16);
        let encrypted = aes_ecb_encrypt_128(aes_key, &padded).unwrap();
        let mut output = Vec::new();
        output.extend_from_slice(magic);
        output.extend_from_slice(&(padded.len() as u32).to_le_bytes());
        output.extend_from_slice(&0u32.to_le_bytes());
        output.push(0);
        output.extend_from_slice(&encrypted);
        output
    }

    #[test]
    fn v3_round_trip() {
        let data = b"hello wechat".to_vec();
        let encrypted = decrypt_dat_v3(&data, 0x5A);
        assert_eq!(decrypt_dat_v3(&encrypted, 0x5A), data);
    }

    #[test]
    fn v4_round_trip() {
        let key = b"0123456789abcdef";
        let plain = b"hello wechat dat v4";
        let data = build_v4(plain, 0x33, key, DAT_V2_MAGIC);
        assert_eq!(decrypt_dat_v4(&data, 0x33, key).unwrap(), plain);
    }

    #[test]
    fn version_detection() {
        assert_eq!(detect_dat_version(DAT_V1_MAGIC), DatVersion::V1);
        assert_eq!(detect_dat_version(DAT_V2_MAGIC), DatVersion::V2);
        assert_eq!(detect_dat_version(b"plain"), DatVersion::Unknown);
    }
}
