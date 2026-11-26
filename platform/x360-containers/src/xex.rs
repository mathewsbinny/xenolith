//! Read-only parser and page-digest verifier for Xbox 360 `XEX2` executables.
//!
//! Parses the `XEX2` header, optional-header directory (entry point, base address, execution
//! info / Title ID / Media ID), security info, and 24-byte SHA-1 page-descriptor chain.
#![deny(unsafe_code)]

use sha1_smol::Sha1;

/// `XEX2` file magic (`"XEX2"`).
pub const MAGIC: &[u8; 4] = b"XEX2";
/// Standard 64 KiB XEX page size (when `image_flags & 0x1000_0000 == 0`).
pub const PAGE_64K: usize = 0x1_0000;
/// Small 4 KiB XEX page size (when `image_flags & 0x1000_0000 != 0`).
pub const PAGE_4K: usize = 0x1000;

/// Optional header key for `XEX_HEADER_ENTRY_POINT` (inline `u32`).
pub const OPT_ENTRY_POINT: u32 = 0x0001_0100;
/// Optional header key for `XEX_HEADER_IMAGE_BASE_ADDRESS` (inline `u32`).
pub const OPT_IMAGE_BASE: u32 = 0x0001_0201;
/// Optional header key for `XEX_HEADER_EXECUTION_INFO` (file offset to 24-byte struct).
pub const OPT_EXECUTION_INFO: u32 = 0x0004_0006;

/// Errors returned while parsing or verifying an `XEX2` executable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum XexError {
    /// Input does not begin with `b"XEX2"`.
    #[error("not an XEX2 file")]
    BadMagic,
    /// A required header, table, or descriptor extends past the end of the buffer.
    #[error("truncated XEX2: {what} at 0x{offset:x} needs {need} bytes (file is {len} bytes)")]
    Truncated {
        /// Human-readable name of the structure being read.
        what: &'static str,
        /// Starting byte offset.
        offset: usize,
        /// Number of bytes required.
        need: usize,
        /// Actual buffer length.
        len: usize,
    },
    /// Decoded image length does not match the security info's declared `image_size`.
    #[error("decoded image size {actual} does not match declared XEX image size {declared}")]
    ImageSizeMismatch {
        /// Actual decoded image byte length.
        actual: usize,
        /// Declared `image_size` from security info.
        declared: usize,
    },
}

/// One optional-header directory entry from the `XEX2` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptionalHeader {
    /// 32-bit XEX optional header key.
    pub key: u32,
    /// Inline value (when `(key & 0xFF) <= 1`) or file offset (otherwise).
    pub value: u32,
}

/// Execution metadata parsed from `OPT_EXECUTION_INFO` (`0x0004_0006`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionInfo {
    /// 32-bit Media ID.
    pub media_id: u32,
    /// Title version (`major.minor.build.qfe` packed into 32 bits).
    pub version: u32,
    /// Base title version.
    pub base_version: u32,
    /// 32-bit Xbox 360 Title ID (e.g. `0x5841097E` for BF1943).
    pub title_id: u32,
    /// Platform byte.
    pub platform: u8,
    /// Executable type byte.
    pub executable_type: u8,
    /// Disc number.
    pub disc_number: u8,
    /// Total disc count.
    pub disc_count: u8,
}

/// One 24-byte page descriptor from the `XEX2` security info.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageDescriptor {
    /// Number of pages covered by this descriptor (`word >> 4`).
    pub pages: u32,
    /// Page protection/kind nibble (`word & 0xF`).
    pub kind: u8,
    /// 20-byte SHA-1 digest of the *next* page concatenated with the next descriptor's raw 24 bytes.
    pub digest: [u8; 20],
    /// Raw 24-byte descriptor as stored in the XEX (used when hashing the chain).
    pub raw: [u8; 24],
}

/// Parsed `XEX2` security info block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityInfo {
    /// Security info header size in bytes.
    pub header_size: u32,
    /// Declared size of the decoded PE image in bytes.
    pub image_size: u32,
    /// Signed image info size.
    pub info_size: u32,
    /// Image flags (bit `0x1000_0000` selects 4 KiB pages instead of 64 KiB pages).
    pub image_flags: u32,
    /// Virtual load address of the PE image (typically `0x8200_0000`).
    pub load_address: u32,
    /// Anchor 20-byte SHA-1 digest covering `page[0] || descriptor[0]`.
    pub image_digest: [u8; 20],
    /// Ordered list of 24-byte page descriptors.
    pub descriptors: Vec<PageDescriptor>,
}

impl SecurityInfo {
    /// Returns the page size in bytes (`4096` if `image_flags & 0x1000_0000 != 0`, else `65536`).
    #[must_use]
    pub const fn page_size(&self) -> usize {
        if (self.image_flags & 0x1000_0000) != 0 {
            PAGE_4K
        } else {
            PAGE_64K
        }
    }
}

/// Result of verifying a decoded PE image against an `XEX2` SHA-1 page-digest chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageVerifyReport {
    /// Whether `SHA-1(page[0] || descriptor[0]) == image_digest`.
    pub anchor_ok: bool,
    /// Number of interior chain links `k >= 1` whose digest matched `descriptor[k - 1].digest`.
    pub chain_matches: usize,
    /// Total number of interior chain links (`descriptors.len().saturating_sub(1)`).
    pub chain_links: usize,
    /// Whether the final descriptor's digest is all zeros.
    pub last_digest_zero: bool,
}

impl PageVerifyReport {
    /// Returns `true` if every page in the image is authenticated by the chain.
    #[must_use]
    pub const fn is_authentic(&self) -> bool {
        self.anchor_ok && self.chain_matches == self.chain_links && self.last_digest_zero
    }
}

/// Parsed `XEX2` executable metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XexHeader {
    /// Module flags word.
    pub module_flags: u32,
    /// Offset to the PE/compressed data section.
    pub pe_offset: u32,
    /// Offset to the security info block.
    pub security_offset: u32,
    /// Optional header directory entries.
    pub optional_headers: Vec<OptionalHeader>,
    /// Parsed execution info, if `OPT_EXECUTION_INFO` was present.
    pub execution_info: Option<ExecutionInfo>,
    /// Entry point virtual address, if `OPT_ENTRY_POINT` was present.
    pub entry_point: Option<u32>,
    /// Image base virtual address, if `OPT_IMAGE_BASE` was present.
    pub image_base: Option<u32>,
    /// Security info block and page-descriptor chain.
    pub security: SecurityInfo,
}

fn slice<'a>(b: &'a [u8], off: usize, need: usize, what: &'static str) -> Result<&'a [u8], XexError> {
    let end = off.checked_add(need).ok_or(XexError::Truncated {
        what,
        offset: off,
        need,
        len: b.len(),
    })?;
    b.get(off..end).ok_or(XexError::Truncated {
        what,
        offset: off,
        need,
        len: b.len(),
    })
}

fn be32(b: &[u8], off: usize, what: &'static str) -> Result<u32, XexError> {
    let s = slice(b, off, 4, what)?;
    let arr: [u8; 4] = s.try_into().map_err(|_| XexError::Truncated {
        what,
        offset: off,
        need: 4,
        len: b.len(),
    })?;
    Ok(u32::from_be_bytes(arr))
}

/// Parses an `XEX2` executable header, optional headers, and security info.
///
/// # Errors
/// Returns [`XexError::BadMagic`] if the magic is not `b"XEX2"` or [`XexError::Truncated`] if any
/// declared header or descriptor table extends past the end of `data`.
pub fn parse(data: &[u8]) -> Result<XexHeader, XexError> {
    let hdr = slice(data, 0, 24, "XEX2 header")?;
    if hdr.get(0..4) != Some(MAGIC.as_slice()) {
        return Err(XexError::BadMagic);
    }
    let module_flags = be32(data, 4, "module_flags")?;
    let pe_offset = be32(data, 8, "pe_offset")?;
    let security_offset = be32(data, 16, "security_offset")? as usize;
    let opt_count = be32(data, 20, "optional_header_count")? as usize;

    let opt_bytes = opt_count.checked_mul(8).ok_or(XexError::Truncated {
        what: "optional header table",
        offset: 24,
        need: usize::MAX,
        len: data.len(),
    })?;
    let _ = slice(data, 24, opt_bytes, "optional header table")?;

    let mut optional_headers = Vec::with_capacity(opt_count);
    let mut entry_point = None;
    let mut image_base = None;
    let mut execution_info = None;

    for i in 0..opt_count {
        let base = 24 + i * 8;
        let key = be32(data, base, "optional header key")?;
        let value = be32(data, base + 4, "optional header value")?;
        optional_headers.push(OptionalHeader { key, value });
        match key {
            OPT_ENTRY_POINT => entry_point = Some(value),
            OPT_IMAGE_BASE => image_base = Some(value),
            OPT_EXECUTION_INFO => {
                let eo = value as usize;
                let raw = slice(data, eo, 24, "execution info")?;
                execution_info = Some(ExecutionInfo {
                    media_id: be32(raw, 0, "media_id")?,
                    version: be32(raw, 4, "version")?,
                    base_version: be32(raw, 8, "base_version")?,
                    title_id: be32(raw, 12, "title_id")?,
                    platform: raw.get(16).copied().unwrap_or(0),
                    executable_type: raw.get(17).copied().unwrap_or(0),
                    disc_number: raw.get(18).copied().unwrap_or(0),
                    disc_count: raw.get(19).copied().unwrap_or(0),
                });
            }
            _ => {}
        }
    }

    let header_size = be32(data, security_offset, "security header_size")?;
    let image_size = be32(data, security_offset + 4, "security image_size")?;
    let info_off = security_offset + 8 + 256; // 256-byte RSA-2048 signature
    let info_size = be32(data, info_off, "signed image info_size")?;
    let image_flags = be32(data, info_off + 4, "signed image_flags")?;
    let load_address = be32(data, info_off + 8, "signed load_address")?;
    let mut image_digest = [0u8; 20];
    image_digest.copy_from_slice(slice(data, info_off + 12, 20, "image_digest")?);

    let desc_count = be32(data, security_offset + 0x180, "page_descriptor_count")? as usize;
    let desc_table_off = security_offset + 0x184;
    let desc_bytes = desc_count.checked_mul(24).ok_or(XexError::Truncated {
        what: "page descriptors",
        offset: desc_table_off,
        need: usize::MAX,
        len: data.len(),
    })?;
    let _ = slice(data, desc_table_off, desc_bytes, "page descriptors")?;

    let mut descriptors = Vec::with_capacity(desc_count);
    for i in 0..desc_count {
        let off = desc_table_off + i * 24;
        let raw_slice = slice(data, off, 24, "page descriptor")?;
        let mut raw = [0u8; 24];
        raw.copy_from_slice(raw_slice);
        let word = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
        let mut digest = [0u8; 20];
        if let Some(ds) = raw.get(4..24) {
            digest.copy_from_slice(ds);
        }
        descriptors.push(PageDescriptor {
            pages: word >> 4,
            kind: (word & 0xF) as u8,
            digest,
            raw,
        });
    }

    Ok(XexHeader {
        module_flags,
        pe_offset,
        security_offset: security_offset as u32,
        optional_headers,
        execution_info,
        entry_point,
        image_base,
        security: SecurityInfo {
            header_size,
            image_size,
            info_size,
            image_flags,
            load_address,
            image_digest,
            descriptors,
        },
    })
}

fn sha1_page(page: &[u8], desc_raw: &[u8; 24]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(page);
    h.update(desc_raw);
    h.digest().bytes()
}

impl XexHeader {
    /// Verifies a decoded PE memory image against this `XEX2`'s reverse-linked SHA-1 page chain.
    ///
    /// # Errors
    /// Returns [`XexError::ImageSizeMismatch`] if `decoded_image.len()` does not equal
    /// `self.security.image_size`, or [`XexError::Truncated`] if descriptors overrun the image.
    pub fn verify_image_pages(&self, decoded_image: &[u8]) -> Result<PageVerifyReport, XexError> {
        let declared = self.security.image_size as usize;
        if decoded_image.len() != declared {
            return Err(XexError::ImageSizeMismatch {
                actual: decoded_image.len(),
                declared,
            });
        }
        let descs = &self.security.descriptors;
        if descs.is_empty() {
            return Ok(PageVerifyReport {
                anchor_ok: false,
                chain_matches: 0,
                chain_links: 0,
                last_digest_zero: false,
            });
        }
        let page_sz = self.security.page_size();
        let mut off = 0usize;
        let mut anchor_ok = false;
        let mut chain_matches = 0usize;

        for (i, d) in descs.iter().enumerate() {
            let len = (d.pages as usize).saturating_mul(page_sz);
            let page = slice(decoded_image, off, len, "decoded image page")?;
            off += len;
            let computed = sha1_page(page, &d.raw);
            if i == 0 {
                anchor_ok = computed == self.security.image_digest;
            } else if let Some(prev) = descs.get(i - 1)
                && computed == prev.digest
            {
                chain_matches += 1;
            }
        }
        let last_digest_zero = descs.last().is_some_and(|d| d.digest == [0u8; 20]);
        Ok(PageVerifyReport {
            anchor_ok,
            chain_matches,
            chain_links: descs.len().saturating_sub(1),
            last_digest_zero,
        })
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn build_synthetic_xex(pages: &[Vec<u8>]) -> (Vec<u8>, Vec<u8>) {
        // Build reverse-linked SHA-1 descriptors for 4 KiB pages (image_flags = 0x1000_0000)
        let mut descs: Vec<[u8; 24]> = vec![[0u8; 24]; pages.len()];
        let mut next_hash = [0u8; 20];
        for i in (0..pages.len()).rev() {
            let word: u32 = (1u32 << 4) | 2; // 1 page, kind=2
            let mut raw = [0u8; 24];
            raw[0..4].copy_from_slice(&word.to_be_bytes());
            raw[4..24].copy_from_slice(&next_hash);
            next_hash = sha1_page(pages.get(i).map_or(&[], Vec::as_slice), &raw);
            if let Some(slot) = descs.get_mut(i) {
                *slot = raw;
            }
        }
        let image_digest = next_hash;
        let image: Vec<u8> = pages.iter().flat_map(|p| p.iter().copied()).collect();

        // Optional headers: entry_point, image_base, execution_info (at offset 0x40)
        let exec_off: u32 = 0x40;
        let sec_off: usize = 0x60;
        let total_len = sec_off + 0x184 + descs.len() * 24;
        let mut xex = vec![0u8; total_len];
        xex[0..4].copy_from_slice(MAGIC);
        xex[4..8].copy_from_slice(&1u32.to_be_bytes());
        xex[8..12].copy_from_slice(&(total_len as u32).to_be_bytes());
        xex[16..20].copy_from_slice(&(sec_off as u32).to_be_bytes());
        xex[20..24].copy_from_slice(&3u32.to_be_bytes());

        // 3 optional headers
        let opts = [
            (OPT_ENTRY_POINT, 0x8200_1000u32),
            (OPT_IMAGE_BASE, 0x8200_0000u32),
            (OPT_EXECUTION_INFO, exec_off),
        ];
        for (i, (k, v)) in opts.iter().enumerate() {
            let b = 24 + i * 8;
            xex[b..b + 4].copy_from_slice(&k.to_be_bytes());
            xex[b + 4..b + 8].copy_from_slice(&v.to_be_bytes());
        }
        // Execution info at 0x40
        let eo = exec_off as usize;
        xex[eo..eo + 4].copy_from_slice(&0x1122_3344u32.to_be_bytes()); // media_id
        xex[eo + 12..eo + 16].copy_from_slice(&0x5841_097Eu32.to_be_bytes()); // BF1943 Title ID

        // Security info at sec_off
        xex[sec_off..sec_off + 4].copy_from_slice(&(0x184u32 + (descs.len() as u32) * 24).to_be_bytes());
        xex[sec_off + 4..sec_off + 8].copy_from_slice(&(image.len() as u32).to_be_bytes());
        let info = sec_off + 8 + 256;
        xex[info + 4..info + 8].copy_from_slice(&0x1000_0000u32.to_be_bytes()); // 4 KiB pages
        xex[info + 8..info + 12].copy_from_slice(&0x8200_0000u32.to_be_bytes());
        xex[info + 12..info + 32].copy_from_slice(&image_digest);
        xex[sec_off + 0x180..sec_off + 0x184].copy_from_slice(&(descs.len() as u32).to_be_bytes());
        for (i, d) in descs.iter().enumerate() {
            let off = sec_off + 0x184 + i * 24;
            xex[off..off + 24].copy_from_slice(d);
        }
        (xex, image)
    }

    #[test]
    fn parses_xex2_and_verifies_page_chain() {
        let pages = vec![vec![0xAAu8; PAGE_4K], vec![0xBBu8; PAGE_4K], vec![0x00u8; PAGE_4K]];
        let (xex, mut image) = build_synthetic_xex(&pages);
        let parsed = parse(&xex).ok();
        assert!(parsed.is_some());
        if let Some(hdr) = parsed {
            assert_eq!(hdr.entry_point, Some(0x8200_1000));
            assert_eq!(hdr.image_base, Some(0x8200_0000));
            assert_eq!(hdr.execution_info.map(|e| e.title_id), Some(0x5841_097E));
            assert_eq!(hdr.security.page_size(), PAGE_4K);

            let rep = hdr.verify_image_pages(&image).ok();
            assert!(rep.is_some_and(|r| r.is_authentic()));

            // Tampering with page 1 must break the chain
            if let Some(b) = image.get_mut(PAGE_4K + 10) {
                *b ^= 0xFF;
            }
            let bad = hdr.verify_image_pages(&image).ok();
            assert!(bad.is_some_and(|r| !r.is_authentic() && r.anchor_ok && r.chain_matches == 1));
        }
    }

    #[test]
    fn refuses_bad_magic_and_truncated_input() {
        assert_eq!(
            parse(b"NOTXEX"),
            Err(XexError::Truncated {
                what: "XEX2 header",
                offset: 0,
                need: 24,
                len: 6
            })
        );
        assert_eq!(parse(&[0u8; 32]), Err(XexError::BadMagic));
    }
}
