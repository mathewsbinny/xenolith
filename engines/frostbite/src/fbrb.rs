//! Frostbite 1 / 1.5 `FbRB` archive reader and writer.
//!
//! Layout:
//! - `0..4`: ASCII `"FbRB"`
//! - `4..8`: big-endian `u32` byte length of the header block (`cut`)
//! - `8..8+cut`: header block (gzip-compressed if starting with `1F 8B`, else raw)
//!   - `u32` BE version (`2`)
//!   - `u32` BE string-table length (`str_len`)
//!   - `[u8; str_len]` NUL-terminated path/extension strings
//!   - `u32` BE entry count
//!   - `entry_count` × 24-byte records: `(name_off, flags, data_off, data_len, orig_len, ext_off)`
//! - `8+cut..EOF`: payload block (gzip-compressed if starting with `1F 8B`, else raw)

use std::collections::BTreeMap;
use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

/// Errors returned when reading or writing a Frostbite `FbRB` bundle.
#[derive(Debug, thiserror::Error)]
pub enum FbrbError {
    /// Input does not start with `"FbRB"` or is truncated.
    #[error("invalid FbRB header: {0}")]
    InvalidHeader(&'static str),
    /// Gzip decompression or compression failed.
    #[error("gzip error: {0}")]
    Gzip(#[from] std::io::Error),
    /// An entry's byte range exceeds the payload buffer.
    #[error("entry '{0}' out of payload bounds")]
    OutOfBounds(String),
}

/// One file entry inside a Frostbite `FbRB` bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FbrbEntry {
    /// Relative asset path (without extension), e.g. `"menu/main/main_nav"`.
    pub path: String,
    /// Asset extension (without leading dot), e.g. `"dbx"`, `"itexture"`, `"res"`.
    pub ext: String,
    /// Entry flags (usually `0x0001_0000`; `0` marks a deleted/tombstone slot).
    pub flags: u32,
    /// Raw entry payload bytes.
    pub data: Vec<u8>,
}

/// Parsed Frostbite `FbRB` archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FbrbArchive {
    /// Archive format version (normally `2`).
    pub version: u32,
    /// Ordered entries.
    pub entries: Vec<FbrbEntry>,
}

fn read_be_u32(buf: &[u8], off: usize) -> Result<u32, FbrbError> {
    let slice = buf.get(off..off + 4).ok_or(FbrbError::InvalidHeader("truncated u32"))?;
    let mut arr = [0u8; 4];
    arr.copy_from_slice(slice);
    Ok(u32::from_be_bytes(arr))
}

fn maybe_gunzip(data: &[u8]) -> Result<Vec<u8>, FbrbError> {
    if data.len() >= 2 && data.first() == Some(&0x1F) && data.get(1) == Some(&0x8B) {
        let mut dec = GzDecoder::new(data);
        let mut out = Vec::new();
        dec.read_to_end(&mut out)?;
        Ok(out)
    } else {
        Ok(data.to_vec())
    }
}

fn read_cstr(table: &[u8], off: usize) -> String {
    let Some(rest) = table.get(off..) else {
        return String::new();
    };
    let end = memchr::memchr(0, rest).unwrap_or(rest.len());
    let bytes = rest.get(..end).unwrap_or_default();
    String::from_utf8_lossy(bytes).into_owned()
}

impl FbrbArchive {
    /// Parses a Frostbite `FbRB` bundle from raw bytes.
    ///
    /// # Errors
    /// Returns [`FbrbError`] if the magic, gzip streams, or entry bounds are invalid.
    pub fn parse(raw: &[u8]) -> Result<Self, FbrbError> {
        if raw.get(0..4) != Some(b"FbRB") {
            return Err(FbrbError::InvalidHeader("missing FbRB magic"));
        }
        let cut = read_be_u32(raw, 4)? as usize;
        let hdr_raw = raw
            .get(8..8 + cut)
            .ok_or(FbrbError::InvalidHeader("header length exceeds file"))?;
        let body_raw = raw.get(8 + cut..).ok_or(FbrbError::InvalidHeader("missing payload"))?;

        let hdr = maybe_gunzip(hdr_raw)?;
        let body = maybe_gunzip(body_raw)?;

        let version = read_be_u32(&hdr, 0)?;
        let str_len = read_be_u32(&hdr, 4)? as usize;
        let str_table = hdr
            .get(8..8 + str_len)
            .ok_or(FbrbError::InvalidHeader("string table truncated"))?;

        let count_off = 8 + str_len;
        let count = read_be_u32(&hdr, count_off)? as usize;
        let mut entries = Vec::with_capacity(count);
        let mut pos = count_off + 4;

        for _ in 0..count {
            let name_off = read_be_u32(&hdr, pos)? as usize;
            let flags = read_be_u32(&hdr, pos + 4)?;
            let data_off = read_be_u32(&hdr, pos + 8)? as usize;
            let data_len = read_be_u32(&hdr, pos + 12)? as usize;
            let _orig_len = read_be_u32(&hdr, pos + 16)?;
            let ext_off = read_be_u32(&hdr, pos + 20)? as usize;
            pos += 24;

            let path = read_cstr(str_table, name_off);
            let ext = read_cstr(str_table, ext_off);
            let data = if flags == 0 && data_len == 0 {
                Vec::new()
            } else {
                body.get(data_off..data_off + data_len)
                    .ok_or_else(|| FbrbError::OutOfBounds(path.clone()))?
                    .to_vec()
            };
            entries.push(FbrbEntry { path, ext, flags, data });
        }

        Ok(Self { version, entries })
    }

    /// Serializes this archive into a standard gzip-compressed Frostbite `FbRB` bundle.
    ///
    /// # Errors
    /// Returns [`FbrbError::Gzip`] if compression fails.
    pub fn to_bytes(&self, compress_body: bool) -> Result<Vec<u8>, FbrbError> {
        let mut str_table: Vec<u8> = Vec::new();
        let mut interned: BTreeMap<String, u32> = BTreeMap::new();
        let mut intern = |s: &str| -> u32 {
            if let Some(&off) = interned.get(s) {
                return off;
            }
            let off = str_table.len() as u32;
            str_table.extend_from_slice(s.as_bytes());
            str_table.push(0);
            interned.insert(s.to_owned(), off);
            off
        };

        let mut records = Vec::with_capacity(self.entries.len() * 24);
        let mut body = Vec::new();
        for e in &self.entries {
            let name_off = intern(&e.path);
            let ext_off = intern(&e.ext);
            let data_off = body.len() as u32;
            let data_len = e.data.len() as u32;
            body.extend_from_slice(&e.data);

            records.extend_from_slice(&name_off.to_be_bytes());
            records.extend_from_slice(&e.flags.to_be_bytes());
            records.extend_from_slice(&data_off.to_be_bytes());
            records.extend_from_slice(&data_len.to_be_bytes());
            records.extend_from_slice(&data_len.to_be_bytes());
            records.extend_from_slice(&ext_off.to_be_bytes());
        }

        let mut hdr_plain = Vec::with_capacity(17 + str_table.len() + records.len());
        hdr_plain.extend_from_slice(&self.version.to_be_bytes());
        hdr_plain.extend_from_slice(&(str_table.len() as u32).to_be_bytes());
        hdr_plain.extend_from_slice(&str_table);
        hdr_plain.extend_from_slice(&(self.entries.len() as u32).to_be_bytes());
        hdr_plain.extend_from_slice(&records);
        // Frostbite FbRB v2 header trailer: 1 byte (body_is_compressed) + u32 BE (uncompressed body length)
        hdr_plain.push(u8::from(compress_body));
        hdr_plain.extend_from_slice(&(body.len() as u32).to_be_bytes());

        let mut hdr_gz = GzEncoder::new(Vec::new(), Compression::default());
        hdr_gz.write_all(&hdr_plain)?;
        let hdr_comp = hdr_gz.finish()?;

        let body_out = if compress_body {
            let mut body_gz = GzEncoder::new(Vec::new(), Compression::default());
            body_gz.write_all(&body)?;
            body_gz.finish()?
        } else {
            body
        };

        let mut out = Vec::with_capacity(8 + hdr_comp.len() + body_out.len());
        out.extend_from_slice(b"FbRB");
        out.extend_from_slice(&(hdr_comp.len() as u32).to_be_bytes());
        out.extend_from_slice(&hdr_comp);
        out.extend_from_slice(&body_out);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_fbrb() {
        let orig = FbrbArchive {
            version: 2,
            entries: vec![
                FbrbEntry {
                    path: "menu/main/main_nav".into(),
                    ext: "res".into(),
                    flags: 0x0001_0000,
                    data: b"hello frostbite".to_vec(),
                },
                FbrbEntry {
                    path: "menu/main/logo".into(),
                    ext: "itexture".into(),
                    flags: 0x0001_0000,
                    data: vec![1, 2, 3, 4],
                },
            ],
        };
        let bytes = orig.to_bytes(true).ok();
        assert!(bytes.is_some());
        let parsed = FbrbArchive::parse(bytes.as_deref().unwrap_or_default()).ok();
        assert_eq!(parsed, Some(orig));
    }
}
