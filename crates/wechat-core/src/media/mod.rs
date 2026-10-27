pub mod dat;
pub mod emoticon;
pub mod xor;

use std::path::Path;

use crate::error::Result;

pub use dat::{
    decrypt_dat_v3, decrypt_dat_v4, detect_dat_version, DatVersion, DAT_V1_AES_KEY, DAT_V1_MAGIC,
    DAT_V2_MAGIC,
};
pub use emoticon::decrypt_emoticon_aes_cbc;
pub use xor::{
    detect_image_media_type, guess_media_type_by_path, is_probably_valid_image,
    try_strip_media_prefix, try_xor_decrypt_by_magic, xor_bytes, WxgfConverter,
};

#[derive(Debug, Clone, Default)]
pub struct MediaKeys {
    pub xor_key: Option<u8>,

    pub aes_key: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct MediaDecryptResult {
    pub data: Vec<u8>,
    pub media_type: String,
    pub dat_version: Option<i32>,
    pub used_magic_fallback: bool,
}

pub fn decrypt_media_bytes<'a>(
    data: &[u8],
    keys: &MediaKeys,
    version_override: Option<i32>,
    wxgf_converter: Option<WxgfConverter<'a>>,
) -> MediaDecryptResult {
    if data.is_empty() {
        return MediaDecryptResult {
            data: Vec::new(),
            media_type: "application/octet-stream".into(),
            dat_version: None,
            used_magic_fallback: false,
        };
    }

    let fast_type = detect_image_media_type(data);
    if fast_type != "application/octet-stream" {
        return MediaDecryptResult {
            data: data.to_vec(),
            media_type: fast_type.into(),
            dat_version: None,
            used_magic_fallback: false,
        };
    }

    let (stripped, stripped_type) = try_strip_media_prefix(data, wxgf_converter);
    if stripped_type != "application/octet-stream"
        && (!stripped_type.starts_with("image/")
            || is_probably_valid_image(&stripped, stripped_type))
    {
        return MediaDecryptResult {
            data: stripped,
            media_type: stripped_type.into(),
            dat_version: None,
            used_magic_fallback: false,
        };
    }

    let version = version_override
        .map(DatVersion::from_i32)
        .unwrap_or_else(|| detect_dat_version(data));
    let version_number = version.as_i32();

    if let Some(xor_key) = keys.xor_key {
        match version {
            DatVersion::V0 => {
                let decrypted = decrypt_dat_v3(data, xor_key);
                if let Some(result) = finish_dat_output(decrypted, wxgf_converter, Some(0), false) {
                    return result;
                }
            }
            DatVersion::V1 => {
                if let Ok(decrypted) = decrypt_dat_v4(data, xor_key, DAT_V1_AES_KEY) {
                    if let Some(result) =
                        finish_dat_output(decrypted.clone(), wxgf_converter, Some(1), false)
                    {
                        return result;
                    }
                    return MediaDecryptResult {
                        data: decrypted,
                        media_type: "application/octet-stream".into(),
                        dat_version: Some(1),
                        used_magic_fallback: false,
                    };
                }
            }
            DatVersion::V2 => {
                if let Some(aes_key) = keys.aes_key.as_deref() {
                    if aes_key.len() >= 16 {
                        if let Ok(decrypted) = decrypt_dat_v4(data, xor_key, aes_key) {
                            if let Some(result) =
                                finish_dat_output(decrypted.clone(), wxgf_converter, Some(2), false)
                            {
                                return result;
                            }
                            return MediaDecryptResult {
                                data: decrypted,
                                media_type: "application/octet-stream".into(),
                                dat_version: Some(2),
                                used_magic_fallback: false,
                            };
                        }
                    }
                }
            }
            DatVersion::Unknown => {}
        }
    }

    if version_number == 0 || version_number == -1 {
        let (decrypted, media_type) = try_xor_decrypt_by_magic(data, wxgf_converter);
        if let (Some(decrypted), Some(media_type)) = (decrypted, media_type) {
            return MediaDecryptResult {
                data: decrypted,
                media_type: media_type.into(),
                dat_version: None,
                used_magic_fallback: true,
            };
        }
    }

    fallback_output(data, None)
}

fn finish_dat_output<'a>(
    data: Vec<u8>,
    wxgf_converter: Option<WxgfConverter<'a>>,
    dat_version: Option<i32>,
    used_magic_fallback: bool,
) -> Option<MediaDecryptResult> {
    let (stripped, media_type) = try_strip_media_prefix(&data, wxgf_converter);
    if media_type != "application/octet-stream" {
        return Some(MediaDecryptResult {
            data: stripped,
            media_type: media_type.into(),
            dat_version,
            used_magic_fallback,
        });
    }
    let mut output = data;
    if output.starts_with(b"wxgf") {
        if let Some(converter) = wxgf_converter {
            if let Some(converted) = converter(&output) {
                output = converted;
            }
        }
    }
    let media_type = detect_image_media_type(&output[..output.len().min(32)]);
    if media_type != "application/octet-stream" {
        return Some(MediaDecryptResult {
            data: output,
            media_type: media_type.into(),
            dat_version,
            used_magic_fallback,
        });
    }
    None
}

fn fallback_output(data: &[u8], media_type: Option<&str>) -> MediaDecryptResult {
    let mut media_type = media_type
        .map(ToString::to_string)
        .unwrap_or_else(|| "application/octet-stream".into());
    if media_type.starts_with("image/") && !is_probably_valid_image(data, &media_type) {
        media_type = "application/octet-stream".into();
    }
    if media_type == "video/mp4" && !(data.len() >= 8 && &data[4..8] == b"ftyp") {
        media_type = "application/octet-stream".into();
    }
    MediaDecryptResult {
        data: data.to_vec(),
        media_type,
        dat_version: None,
        used_magic_fallback: false,
    }
}

pub fn decrypt_media_file<'a>(
    path: &Path,
    keys: &MediaKeys,
    version_override: Option<i32>,
    wxgf_converter: Option<WxgfConverter<'a>>,
) -> Result<MediaDecryptResult> {
    let data = std::fs::read(path)?;
    let mut result = decrypt_media_bytes(&data, keys, version_override, wxgf_converter);
    if result.media_type == "application/octet-stream" {
        let guessed = guess_media_type_by_path(path, "application/octet-stream");
        let valid = if guessed.starts_with("image/") {
            is_probably_valid_image(&result.data, &guessed)
        } else if guessed == "video/mp4" {
            result.data.len() >= 8 && &result.data[4..8] == b"ftyp"
        } else {
            guessed != "application/octet-stream"
        };
        if valid {
            result.media_type = guessed;
        }
    }
    Ok(result)
}
