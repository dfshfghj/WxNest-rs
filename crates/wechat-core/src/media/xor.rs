use std::path::Path;

pub type WxgfConverter<'a> = &'a dyn Fn(&[u8]) -> Option<Vec<u8>>;

const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
const JPEG_MAGIC: &[u8] = b"\xff\xd8\xff";
const GIF87A: &[u8] = b"GIF87a";
const GIF89A: &[u8] = b"GIF89a";
const RIFF_MAGIC: &[u8] = b"RIFF";
const WEBP_MAGIC: &[u8] = b"WEBP";
const MP4_MAGIC: &[u8] = b"ftyp";
const WXGF_MAGIC: &[u8] = b"wxgf";

pub fn xor_bytes(data: &[u8], key: u8) -> Vec<u8> {
    data.iter().map(|byte| byte ^ key).collect()
}

pub fn detect_image_media_type(data: &[u8]) -> &'static str {
    if data.is_empty() {
        return "application/octet-stream";
    }
    if data.starts_with(PNG_MAGIC) {
        return "image/png";
    }
    if data.starts_with(JPEG_MAGIC) && data.len() >= 4 {
        let marker = data[3];
        if marker != 0x00 && marker != 0xFF && marker >= 0xC0 {
            return "image/jpeg";
        }
    }
    if data.starts_with(GIF87A) || data.starts_with(GIF89A) {
        return "image/gif";
    }
    if data.starts_with(RIFF_MAGIC) && data.len() >= 12 && &data[8..12] == WEBP_MAGIC {
        return "image/webp";
    }
    "application/octet-stream"
}

pub fn is_probably_valid_image(data: &[u8], media_type: &str) -> bool {
    if data.is_empty() {
        return false;
    }
    let media_type = media_type.trim().to_ascii_lowercase();
    if !media_type.starts_with("image/") {
        return false;
    }
    match media_type.as_str() {
        "image/jpeg" => {
            if detect_image_media_type(&data[..data.len().min(32)]) != "image/jpeg" {
                return false;
            }
            let trimmed = trim_trailing_zeroes(data);
            if trimmed.len() < 4 || !trimmed.starts_with(JPEG_MAGIC) {
                return false;
            }
            if trimmed.ends_with(&[0xFF, 0xD9]) {
                return true;
            }
            let tail = &trimmed[trimmed.len().saturating_sub(4096)..];
            match rfind(tail, &[0xFF, 0xD9]) {
                Some(index) => index + 2 >= tail.len().saturating_sub(64),
                None => false,
            }
        }
        "image/png" => {
            if !data.starts_with(PNG_MAGIC) {
                return false;
            }
            let trailer = b"\x00\x00\x00\x00IEND\xaeB`\x82";
            let trimmed = trim_trailing_zeroes(data);
            if trimmed.ends_with(trailer) {
                return true;
            }
            let tail = &trimmed[trimmed.len().saturating_sub(256)..];
            match rfind(tail, trailer) {
                Some(index) => index + trailer.len() >= tail.len().saturating_sub(64),
                None => false,
            }
        }
        "image/gif" => {
            if !(data.starts_with(GIF87A) || data.starts_with(GIF89A)) {
                return false;
            }
            let trimmed = trim_trailing_zeroes(data);
            if trimmed.ends_with(&[0x3B]) {
                return true;
            }
            let tail = &trimmed[trimmed.len().saturating_sub(256)..];
            match rfind(tail, &[0x3B]) {
                Some(index) => index + 1 >= tail.len().saturating_sub(16),
                None => false,
            }
        }
        "image/webp" => {
            data.len() >= 12 && data.starts_with(RIFF_MAGIC) && &data[8..12] == WEBP_MAGIC
        }
        _ => detect_image_media_type(&data[..data.len().min(32)]) != "application/octet-stream",
    }
}

pub fn try_strip_media_prefix<'a>(
    data: &[u8],
    wxgf_converter: Option<WxgfConverter<'a>>,
) -> (Vec<u8>, &'static str) {
    if data.is_empty() {
        return (Vec::new(), "application/octet-stream");
    }
    let head = &data[..data.len().min(256 * 1024)];

    if let Some(index) = find_subslice(head, WXGF_MAGIC) {
        if index <= 128 * 1024 {
            if let Some(converter) = wxgf_converter {
                if let Some(converted) = converter(&data[index..]) {
                    let media_type = detect_image_media_type(&converted[..converted.len().min(32)]);
                    if media_type != "application/octet-stream" {
                        return (converted, media_type);
                    }
                }
            }
        }
    }

    for (magic, _media_type) in [
        (PNG_MAGIC, "image/png"),
        (JPEG_MAGIC, "image/jpeg"),
        (GIF87A, "image/gif"),
        (GIF89A, "image/gif"),
    ] {
        if let Some(index) = find_subslice(head, magic) {
            if index <= 128 * 1024 {
                let sliced = &data[index..];
                let detected = detect_image_media_type(&sliced[..sliced.len().min(32)]);
                if detected != "application/octet-stream"
                    && is_probably_valid_image(sliced, detected)
                {
                    return (sliced.to_vec(), detected);
                }
            }
        }
    }

    if let Some(index) = find_subslice(head, RIFF_MAGIC) {
        if index <= 128 * 1024 {
            let sliced = &data[index..];
            if sliced.len() >= 12 && &sliced[8..12] == WEBP_MAGIC {
                return (sliced.to_vec(), "image/webp");
            }
        }
    }

    if let Some(index) = find_subslice(head, MP4_MAGIC) {
        if (4..=128 * 1024).contains(&index) {
            let sliced = &data[index - 4..];
            if sliced.len() >= 8 && &sliced[4..8] == MP4_MAGIC {
                return (sliced.to_vec(), "video/mp4");
            }
        }
    }

    (data.to_vec(), "application/octet-stream")
}

pub fn try_xor_decrypt_by_magic<'a>(
    data: &[u8],
    wxgf_converter: Option<WxgfConverter<'a>>,
) -> (Option<Vec<u8>>, Option<&'static str>) {
    if data.is_empty() {
        return (None, None);
    }

    const CANDIDATES: &[(usize, &[u8], &str)] = &[
        (0, PNG_MAGIC, "image/png"),
        (0, GIF87A, "image/gif"),
        (0, GIF89A, "image/gif"),
        (0, RIFF_MAGIC, "application/octet-stream"),
        (4, MP4_MAGIC, "video/mp4"),
        (0, WXGF_MAGIC, "application/octet-stream"),
        (1, WXGF_MAGIC, "application/octet-stream"),
        (2, WXGF_MAGIC, "application/octet-stream"),
        (3, WXGF_MAGIC, "application/octet-stream"),
        (4, WXGF_MAGIC, "application/octet-stream"),
        (5, WXGF_MAGIC, "application/octet-stream"),
        (6, WXGF_MAGIC, "application/octet-stream"),
        (7, WXGF_MAGIC, "application/octet-stream"),
        (8, WXGF_MAGIC, "application/octet-stream"),
        (9, WXGF_MAGIC, "application/octet-stream"),
        (10, WXGF_MAGIC, "application/octet-stream"),
        (11, WXGF_MAGIC, "application/octet-stream"),
        (12, WXGF_MAGIC, "application/octet-stream"),
        (13, WXGF_MAGIC, "application/octet-stream"),
        (14, WXGF_MAGIC, "application/octet-stream"),
        (15, WXGF_MAGIC, "application/octet-stream"),
        (0, JPEG_MAGIC, "image/jpeg"),
    ];

    for (offset, magic, media_type) in CANDIDATES {
        if data.len() < offset + magic.len() {
            continue;
        }
        let key = data[*offset] ^ magic[0];
        if !magic
            .iter()
            .enumerate()
            .all(|(index, expected)| (data[offset + index] ^ key) == *expected)
        {
            continue;
        }
        let decoded = xor_bytes(data, key);

        if *magic == WXGF_MAGIC {
            if let Some(converter) = wxgf_converter {
                let payload = if *offset > 0 {
                    &decoded[*offset..]
                } else {
                    &decoded[..]
                };
                if let Some(converted) = converter(payload) {
                    let detected = detect_image_media_type(&converted[..converted.len().min(32)]);
                    if detected != "application/octet-stream" {
                        return (Some(converted), Some(detected));
                    }
                }
            }
            continue;
        }

        if *offset == 0 && *magic == RIFF_MAGIC {
            if decoded.len() >= 12
                && &decoded[8..12] == WEBP_MAGIC
                && is_probably_valid_image(&decoded, "image/webp")
            {
                return (Some(decoded), Some("image/webp"));
            }
            continue;
        }

        if *media_type == "video/mp4" {
            if decoded.len() >= 8 && &decoded[4..8] == MP4_MAGIC {
                return (Some(decoded), Some("video/mp4"));
            }
            continue;
        }

        let detected = detect_image_media_type(&decoded[..decoded.len().min(32)]);
        if detected != *media_type || !is_probably_valid_image(&decoded, detected) {
            continue;
        }
        return (Some(decoded), Some(detected));
    }

    let preview_len = data.len().min(8192);
    if preview_len > 0 {
        for key in 0u16..=255 {
            let preview = xor_bytes(&data[..preview_len], key as u8);
            if [
                WXGF_MAGIC, PNG_MAGIC, JPEG_MAGIC, GIF87A, GIF89A, RIFF_MAGIC, MP4_MAGIC,
            ]
            .iter()
            .any(|magic| find_subslice(&preview, magic).is_some())
            {
                let decoded = xor_bytes(data, key as u8);
                let (stripped, media_type) = try_strip_media_prefix(&decoded, wxgf_converter);
                if media_type != "application/octet-stream" {
                    if media_type.starts_with("image/")
                        && !is_probably_valid_image(&stripped, media_type)
                    {
                        continue;
                    }
                    return (Some(stripped), Some(media_type));
                }
            }
        }
    }

    (None, None)
}

pub fn guess_media_type_by_path(path: &Path, fallback: &str) -> String {
    let extension = path
        .extension()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let media_type = match extension.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "silk" => "audio/silk",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        _ => fallback,
    };
    media_type.to_string()
}

fn trim_trailing_zeroes(data: &[u8]) -> &[u8] {
    let end = data
        .iter()
        .rposition(|byte| *byte != 0)
        .map(|index| index + 1)
        .unwrap_or(0);
    &data[..end]
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_png_and_jpeg() {
        assert_eq!(detect_image_media_type(PNG_MAGIC), "image/png");
        assert_eq!(
            detect_image_media_type(b"\xff\xd8\xff\xe0rest"),
            "image/jpeg"
        );
        assert_eq!(detect_image_media_type(b"nope"), "application/octet-stream");
    }

    #[test]
    fn xor_round_trip() {
        let data = b"hello".to_vec();
        let encrypted = xor_bytes(&data, 0x5A);
        assert_eq!(xor_bytes(&encrypted, 0x5A), data);
    }
}
