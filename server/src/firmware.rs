//! Reads the app description out of an ESP-IDF application image (.bin) so the
//! server knows which project and version a file is before pushing it.
//!
//! Layout: esp_image_header_t (24 bytes), esp_image_segment_header_t (8 bytes),
//! then esp_app_desc_t at offset 32.

use anyhow::{bail, Result};
use sha2::{Digest, Sha256};

const IMAGE_MAGIC: u8 = 0xE9;
const APP_DESC_MAGIC: u32 = 0xABCD_5432;
const DESC: usize = 32;

#[derive(Debug, Clone)]
pub struct ImageInfo {
    pub chip_id: u16,
    pub project: String,
    pub version: String,
    pub idf: String,
    pub build: String,
    pub elf_sha256: String,
    pub sha256: String,
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

pub fn chip_name(id: u16) -> &'static str {
    match id {
        0x0000 => "esp32",
        0x0002 => "esp32s2",
        0x0005 => "esp32c3",
        0x0009 => "esp32s3",
        0x000C => "esp32c2",
        0x000D => "esp32c6",
        0x0010 => "esp32h2",
        0x0012 => "esp32p4",
        0x0014 => "esp32c61",
        0x0017 => "esp32c5",
        _ => "unknown",
    }
}

pub fn parse(img: &[u8]) -> Result<ImageInfo> {
    if img.len() < DESC + 256 {
        bail!("file too small to be an ESP-IDF app image");
    }
    if img[0] != IMAGE_MAGIC {
        bail!("not an ESP-IDF app image (bad magic; did you upload the .elf or a merged flash image?)");
    }
    let chip_id = u16::from_le_bytes([img[12], img[13]]);
    let d = &img[DESC..];
    let magic = u32::from_le_bytes([d[0], d[1], d[2], d[3]]);
    if magic != APP_DESC_MAGIC {
        bail!("image has no app description (bootloader or partition table?)");
    }
    let version = cstr(&d[16..48]);
    let project = cstr(&d[48..80]);
    let time = cstr(&d[80..96]);
    let date = cstr(&d[96..112]);
    let idf = cstr(&d[112..144]);
    let elf_sha256 = hex::encode(&d[144..176]);
    if project.is_empty() {
        bail!("image has an empty project name");
    }
    Ok(ImageInfo {
        chip_id,
        project,
        version,
        idf,
        build: format!("{date} {time}"),
        elf_sha256,
        sha256: hex::encode(Sha256::digest(img)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(buf: &mut [u8], off: usize, s: &str) {
        buf[off..off + s.len()].copy_from_slice(s.as_bytes());
    }

    #[test]
    fn parses_app_desc() {
        let mut img = vec![0u8; 1024];
        img[0] = IMAGE_MAGIC;
        img[12..14].copy_from_slice(&0x0012u16.to_le_bytes());
        img[DESC..DESC + 4].copy_from_slice(&APP_DESC_MAGIC.to_le_bytes());
        put(&mut img, DESC + 16, "0.2.0");
        put(&mut img, DESC + 48, "ono_display");
        put(&mut img, DESC + 80, "12:00:00");
        put(&mut img, DESC + 96, "Nov 21 2026");
        put(&mut img, DESC + 112, "v6.1");
        img[DESC + 144] = 0xab;
        let i = parse(&img).unwrap();
        assert_eq!(i.project, "ono_display");
        assert_eq!(i.version, "0.2.0");
        assert_eq!(i.idf, "v6.1");
        assert_eq!(i.build, "Nov 21 2026 12:00:00");
        assert_eq!(chip_name(i.chip_id), "esp32p4");
        assert!(i.elf_sha256.starts_with("ab00"));
        assert_eq!(i.elf_sha256.len(), 64);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse(&[0u8; 2048]).is_err());
        assert!(parse(&[0xE9u8; 10]).is_err());
    }
}
