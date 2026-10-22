//! A picture's width and height from its header, without decoding it: PNG,
//! JPEG, GIF, BMP and WebP. Used to tell thumbnails and icons from pages
//! before anything is sent to OCR (P7). Other formats (TIFF, HEIC, …) give
//! `None`, and a picture of unknown size is always read.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// `(width, height)` in pixels, or `None` when the header is not one of the
/// formats above or is damaged.
pub fn dimensions(path: &Path) -> Option<(u32, u32)> {
    let mut f = BufReader::new(File::open(path).ok()?);
    let mut head = [0u8; 30];
    let n = read_up_to(&mut f, &mut head).ok()?;
    let head = &head[..n];
    if head.starts_with(b"\x89PNG\r\n\x1a\n") && n >= 24 {
        // The first chunk is IHDR: width and height, big-endian.
        return nonzero(be32(&head[16..20]), be32(&head[20..24]));
    }
    if (head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a")) && n >= 10 {
        return nonzero(le16(&head[6..8]) as u32, le16(&head[8..10]) as u32);
    }
    if head.starts_with(b"BM") && n >= 26 {
        // BITMAPINFOHEADER; a negative height means top-down rows.
        let w = i32::from_le_bytes(head[18..22].try_into().ok()?);
        let h = i32::from_le_bytes(head[22..26].try_into().ok()?);
        return nonzero(w.unsigned_abs(), h.unsigned_abs());
    }
    if head.starts_with(b"RIFF") && n >= 30 && &head[8..12] == b"WEBP" {
        return webp(head);
    }
    if head.starts_with(&[0xFF, 0xD8]) {
        f.seek(SeekFrom::Start(2)).ok()?;
        return jpeg(&mut f);
    }
    None
}

fn webp(head: &[u8]) -> Option<(u32, u32)> {
    match &head[12..16] {
        // Lossy: a key frame's start code, then 14-bit sizes.
        b"VP8 " if head[23..26] == [0x9D, 0x01, 0x2A] => nonzero(
            (le16(&head[26..28]) & 0x3FFF) as u32,
            (le16(&head[28..30]) & 0x3FFF) as u32,
        ),
        // Lossless: a signature byte, then 14-bit sizes minus one.
        b"VP8L" if head[20] == 0x2F => {
            let bits = u32::from_le_bytes(head[21..25].try_into().ok()?);
            nonzero((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1)
        }
        // Extended: 24-bit canvas sizes minus one.
        b"VP8X" => nonzero(le24(&head[24..27]) + 1, le24(&head[27..30]) + 1),
        _ => None,
    }
}

/// Walk the JPEG segments to the first start-of-frame marker.
fn jpeg(f: &mut (impl Read + Seek)) -> Option<(u32, u32)> {
    // A picture whose frame header is this far in is damaged, not a JPEG
    // with a big EXIF block (one segment holds at most 64 KB).
    for _ in 0..64 {
        let mut b = [0u8; 1];
        // Markers may be padded with any number of 0xFF bytes.
        f.read_exact(&mut b).ok()?;
        if b[0] != 0xFF {
            return None;
        }
        let mut marker = 0xFF;
        while marker == 0xFF {
            f.read_exact(&mut b).ok()?;
            marker = b[0];
        }
        match marker {
            // Standalone markers carry no length.
            0x01 | 0xD0..=0xD7 => continue,
            0xD9 | 0xDA => return None,
            _ => {}
        }
        let mut len = [0u8; 2];
        f.read_exact(&mut len).ok()?;
        let len = u16::from_be_bytes(len) as i64;
        if len < 2 {
            return None;
        }
        // SOF0..SOF15, except DHT (C4), JPG (C8) and DAC (CC).
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let mut sof = [0u8; 5];
            f.read_exact(&mut sof).ok()?;
            let h = u16::from_be_bytes([sof[1], sof[2]]) as u32;
            let w = u16::from_be_bytes([sof[3], sof[4]]) as u32;
            return nonzero(w, h);
        }
        f.seek(SeekFrom::Current(len - 2)).ok()?;
    }
    None
}

fn read_up_to(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

fn nonzero(w: u32, h: u32) -> Option<(u32, u32)> {
    (w > 0 && h > 0).then_some((w, h))
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn le24(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], 0])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn headers_give_the_size() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        png.extend_from_slice(&260u32.to_be_bytes());
        png.extend_from_slice(&195u32.to_be_bytes());
        png.extend_from_slice(&[8, 2, 0, 0, 0]);
        assert_eq!(dimensions(&write(d, "a.png", &png)), Some((260, 195)));

        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[0x10, 0x01, 0x20, 0x00]);
        gif.extend_from_slice(&[0; 4]);
        assert_eq!(dimensions(&write(d, "a.gif", &gif)), Some((272, 32)));

        let mut bmp = b"BM".to_vec();
        bmp.extend_from_slice(&[0; 16]);
        bmp.extend_from_slice(&640i32.to_le_bytes());
        bmp.extend_from_slice(&(-480i32).to_le_bytes());
        assert_eq!(dimensions(&write(d, "a.bmp", &bmp)), Some((640, 480)));

        // JPEG: an APP1 (EXIF) segment before the frame header, as in photos.
        let mut jpg = vec![0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x08];
        jpg.extend_from_slice(b"Exif\0\0");
        jpg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        jpg.extend_from_slice(&3000u16.to_be_bytes());
        jpg.extend_from_slice(&4000u16.to_be_bytes());
        jpg.extend_from_slice(&[0; 10]);
        assert_eq!(dimensions(&write(d, "a.jpg", &jpg)), Some((4000, 3000)));

        // WebP extended (VP8X): 24-bit sizes minus one.
        let mut webp = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
        webp.extend_from_slice(&[10, 0, 0, 0, 0, 0, 0, 0]);
        webp.extend_from_slice(&[0x3F, 0x06, 0x00, 0xAF, 0x04, 0x00]);
        assert_eq!(dimensions(&write(d, "a.webp", &webp)), Some((1600, 1200)));

        // Lossless WebP (VP8L): 14-bit sizes minus one after the signature.
        let bits: u32 = 303 | (124 << 14);
        let mut vp8l = b"RIFF\0\0\0\0WEBPVP8L\x05\0\0\0\x2F".to_vec();
        vp8l.extend_from_slice(&bits.to_le_bytes());
        vp8l.extend_from_slice(&[0; 5]);
        assert_eq!(dimensions(&write(d, "b.webp", &vp8l)), Some((304, 125)));
    }

    #[test]
    fn unknown_or_damaged_headers_give_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        assert_eq!(dimensions(&write(d, "a.heic", b"\0\0\0\x18ftypheic")), None);
        assert_eq!(dimensions(&write(d, "cut.jpg", &[0xFF, 0xD8, 0xFF])), None);
        assert_eq!(dimensions(&write(d, "empty.png", b"")), None);
        assert_eq!(dimensions(&d.join("missing.png")), None);
    }
}
