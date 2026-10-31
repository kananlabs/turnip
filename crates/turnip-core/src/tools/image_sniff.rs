//! Image format sniffing: MIME type and pixel dimensions straight from the
//! file header, without a full decoder.

/// Sniffed image identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SniffedImage {
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
}

/// Recognized formats only; None means "not a known image" and callers fall
/// back to text handling.
pub fn sniff(data: &[u8]) -> Option<SniffedImage> {
    if data.len() < 8 {
        return None;
    }
    if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        // IHDR holds 4-byte big-endian width/height at offset 16.
        if data.len() >= 24 {
            return Some(SniffedImage {
                mime: "image/png",
                width: u32::from_be_bytes(data[16..20].try_into().ok()?),
                height: u32::from_be_bytes(data[20..24].try_into().ok()?),
            });
        }
    }
    if data.starts_with(&[b'G', b'I', b'F']) {
        // Little-endian dimensions at offset 6.
        if data.len() >= 10 {
            return Some(SniffedImage {
                mime: "image/gif",
                width: u16::from_le_bytes(data[6..8].try_into().ok()?) as u32,
                height: u16::from_le_bytes(data[8..10].try_into().ok()?) as u32,
            });
        }
    }
    if data.starts_with(b"RIFF") && data[8..12] == *b"WEBP" {
        return sniff_webp(data);
    }
    if data[0] == 0xFF && data[1] == 0xD8 {
        return sniff_jpeg(data);
    }
    None
}

fn sniff_webp(data: &[u8]) -> Option<SniffedImage> {
    match &data[12..16] {
        b"VP8X" if data.len() >= 30 => Some(SniffedImage {
            mime: "image/webp",
            width: (u32::from_le_bytes(data[24..27].try_into().ok()?) & 0xFFFFFF) + 1,
            height: (u32::from_le_bytes(data[27..30].try_into().ok()?) & 0xFFFFFF) + 1,
        }),
        b"VP8 " if data.len() >= 30 => {
            // Lossy: 14-bit dimensions at a fixed offset in the frame tag.
            let width = u16::from_le_bytes(data[26..28].try_into().ok()?) & 0x3FFF;
            let height = u16::from_le_bytes(data[28..30].try_into().ok()?) & 0x3FFF;
            Some(SniffedImage {
                mime: "image/webp",
                width: width as u32,
                height: height as u32,
            })
        }
        b"VP8L" if data.len() >= 25 => {
            // Lossless: 14-bit dimensions packed after the signature.
            let bits = u32::from_le_bytes(data[21..25].try_into().ok()?);
            Some(SniffedImage {
                mime: "image/webp",
                width: (bits & 0x3FFF) + 1,
                height: ((bits >> 14) & 0x3FFF) + 1,
            })
        }
        _ => None,
    }
}

fn sniff_jpeg(data: &[u8]) -> Option<SniffedImage> {
    // Walk the marker segments to a frame header (SOF0..SOF15 except DHT/DAC).
    let mut offset = 2;
    while offset + 9 < data.len() {
        if data[offset] != 0xFF {
            offset += 1;
            continue;
        }
        let marker = data[offset + 1];
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            offset += 2;
            continue;
        }
        let length = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
        let is_frame = (0xC0..=0xCF).contains(&marker)
            && marker != 0xC4
            && marker != 0xC8
            && marker != 0xCC;
        if is_frame {
            return Some(SniffedImage {
                mime: "image/jpeg",
                height: u16::from_be_bytes([data[offset + 5], data[offset + 6]]) as u32,
                width: u16::from_be_bytes([data[offset + 7], data[offset + 8]]) as u32,
            });
        }
        offset += 2 + length;
    }
    // A JPEG without a frame header yet still counts, dimensions unknown.
    Some(SniffedImage {
        mime: "image/jpeg",
        width: 0,
        height: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_png() {
        let mut png = vec![0x89, b'P', b'N', b'G', 0, 0, 0, 0];
        png.extend_from_slice(&13u32.to_be_bytes()); // IHDR length
        png.extend_from_slice(b"IHDR"); // chunk type
        png.extend_from_slice(&12u32.to_be_bytes()); // width
        png.extend_from_slice(&34u32.to_be_bytes()); // height
        let image = sniff(&png).expect("png");
        assert_eq!((image.mime, image.width, image.height), ("image/png", 12, 34));
    }

    #[test]
    fn sniffs_gif() {
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&100u16.to_le_bytes());
        gif.extend_from_slice(&50u16.to_le_bytes());
        let image = sniff(&gif).expect("gif");
        assert_eq!((image.mime, image.width, image.height), ("image/gif", 100, 50));
    }

    #[test]
    fn rejects_text() {
        assert!(sniff(b"hello world text file!").is_none());
    }
}
