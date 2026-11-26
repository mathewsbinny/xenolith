//! Frostbite `ShaderDatabase` and Direct3D 10/11 `DXBC` container parser, validator, and repacker.
//!
//! Provides two complementary pure-Rust pipelines for Frostbite 1 / 1.5 shader assets:
//! 1. **PC `DXBC` container & `ShaderDatabase` repacking** ([`DxbcContainer`], [`dxbc_checksum`],
//!    [`repack_dxbc_database`]): parses Direct3D 10/11 `DXBC` shader blobs, validates and
//!    recomputes the Microsoft DXBC MD5 checksum, identifies shader stages (`VS`/`PS`/`GS`/`HS`/`DS`/`CS`),
//!    strips debug chunks (`SPDB`/`SDBG`/`PRIV`), and repacks embedded length-prefixed or raw
//!    `DXBC` entries inside Frostbite `ShaderDatabase` blobs with optional digest-keyed replacement.
//! 2. **Xbox 360 `ShaderDatabase` scanning & HLSL translation** ([`extract_xenos_shaders`],
//!    [`translate_xenos_database`]): locates XDK Xenos shader containers (`0x102A_1100`..`0x102A_1102`)
//!    inside console `ShaderDatabase` blobs and translates them in parallel to Dx10/Dx11 HLSL via
//!    [`x360_xenos_shader`].
#![deny(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use memchr::memmem;
use rayon::prelude::*;
use regex::Regex;
use thiserror::Error;
use x360_xenos_shader::{hlsl, xenos};

/// Direct3D 10/11 `DXBC` container magic bytes.
pub const DXBC_MAGIC: &[u8; 4] = b"DXBC";
/// Minimum size of a valid `DXBC` header (magic + 16-byte digest + version + size + count).
pub const DXBC_HEADER_LEN: usize = 32;

/// Errors returned by `DXBC` or `ShaderDatabase` parsing and repacking.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ShaderDbError {
    /// Input is shorter than the 32-byte `DXBC` header or lacks `"DXBC"` magic.
    #[error("not a valid DXBC container (len={0})")]
    NotDxbc(usize),
    /// Declared total size or chunk bounds exceed the input slice.
    #[error("truncated DXBC container: declared {declared} bytes, available {available}")]
    Truncated {
        /// Byte size declared by the container or chunk.
        declared: usize,
        /// Available bytes in the buffer.
        available: usize,
    },
    /// Chunk offset or payload is out of bounds.
    #[error("DXBC chunk {index} at offset {offset:#x} is out of bounds")]
    InvalidChunk {
        /// Zero-based chunk index.
        index: usize,
        /// Byte offset declared in the chunk offset table.
        offset: usize,
    },
    /// Container exceeds 32-bit `DXBC` size limit.
    #[error("DXBC container exceeds u32 byte size")]
    TooLarge,
    /// External HLSL shader compilation (`fxc.exe`) failed.
    #[error("shader compilation failed: {0}")]
    Compile(String),
}

/// Target Direct3D shader model profile for HLSL lowering and `fxc.exe` compilation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetProfile {
    /// DirectX 9 (`vs_3_0` / `ps_3_0`).
    Dx9,
    /// DirectX 10 (`vs_4_0` / `ps_4_0` -> `DXBC`).
    Dx10,
    /// DirectX 11 (`vs_5_0` / `ps_5_0` -> `DXBC`).
    Dx11,
}

impl TargetProfile {
    /// Returns the `fxc.exe` `/T` profile string for vertex or pixel shaders.
    #[must_use]
    pub const fn fxc_target(self, is_pixel: bool) -> &'static str {
        match (self, is_pixel) {
            (Self::Dx9, false) => "vs_3_0",
            (Self::Dx9, true) => "ps_3_0",
            (Self::Dx10, false) => "vs_4_0",
            (Self::Dx10, true) => "ps_4_0",
            (Self::Dx11, false) => "vs_5_0",
            (Self::Dx11, true) => "ps_5_0",
        }
    }
}

/// Direct3D 10/11 shader pipeline stage decoded from `SHDR`/`SHEX` bytecode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShaderStage {
    /// Pixel / fragment shader (`0`).
    Pixel,
    /// Vertex shader (`1`).
    Vertex,
    /// Geometry shader (`2`).
    Geometry,
    /// Hull shader (`3`).
    Hull,
    /// Domain shader (`4`).
    Domain,
    /// Compute shader (`5`).
    Compute,
}

impl ShaderStage {
    /// Short canonical stage label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pixel => "PS",
            Self::Vertex => "VS",
            Self::Geometry => "GS",
            Self::Hull => "HS",
            Self::Domain => "DS",
            Self::Compute => "CS",
        }
    }
}

/// A single fourCC-tagged chunk inside a [`DxbcContainer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DxbcChunk {
    /// Four-character chunk identifier (e.g. `*b"SHDR"`, `*b"SHEX"`, `*b"ISGN"`, `*b"OSGN"`, `*b"RDEF"`, `*b"SPDB"`).
    pub fourcc: [u8; 4],
    /// Raw chunk payload bytes.
    pub data: Vec<u8>,
}

/// Parsed Direct3D 10/11 `DXBC` shader container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DxbcContainer {
    /// Container version (always `1` in Direct3D 10/11).
    pub version: u32,
    /// 16-byte Microsoft DXBC MD5 checksum stored in bytes `4..20`.
    pub digest: [u8; 16],
    /// Ordered list of chunks in the container.
    pub chunks: Vec<DxbcChunk>,
}

fn read_u32_le(data: &[u8], at: usize) -> Option<u32> {
    let bytes: [u8; 4] = data.get(at..at + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn read_u32_be(data: &[u8], at: usize) -> Option<u32> {
    let bytes: [u8; 4] = data.get(at..at + 4)?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

impl DxbcContainer {
    /// Parses a `DXBC` container from the start of `bytes`.
    ///
    /// Returns the parsed container and its exact declared byte length (which may be less
    /// than `bytes.len()` when scanning inside a larger `ShaderDatabase`).
    ///
    /// # Errors
    /// Returns [`ShaderDbError`] if the header, total size, or any chunk is malformed.
    pub fn parse_prefix(bytes: &[u8]) -> Result<(Self, usize), ShaderDbError> {
        if bytes.len() < DXBC_HEADER_LEN || bytes.get(0..4) != Some(DXBC_MAGIC.as_slice()) {
            return Err(ShaderDbError::NotDxbc(bytes.len()));
        }
        let digest: [u8; 16] = bytes
            .get(4..20)
            .and_then(|s| s.try_into().ok())
            .ok_or(ShaderDbError::NotDxbc(bytes.len()))?;
        let version = read_u32_le(bytes, 20).ok_or(ShaderDbError::NotDxbc(bytes.len()))?;
        let total_size = read_u32_le(bytes, 24).ok_or(ShaderDbError::NotDxbc(bytes.len()))? as usize;
        let chunk_count = read_u32_le(bytes, 28).ok_or(ShaderDbError::NotDxbc(bytes.len()))? as usize;

        if total_size < DXBC_HEADER_LEN || total_size > bytes.len() {
            return Err(ShaderDbError::Truncated {
                declared: total_size,
                available: bytes.len(),
            });
        }
        let table_end = DXBC_HEADER_LEN
            .checked_add(chunk_count.saturating_mul(4))
            .ok_or(ShaderDbError::Truncated {
                declared: total_size,
                available: bytes.len(),
            })?;
        if table_end > total_size {
            return Err(ShaderDbError::Truncated {
                declared: table_end,
                available: total_size,
            });
        }

        let mut chunks = Vec::with_capacity(chunk_count);
        for i in 0..chunk_count {
            let off = read_u32_le(bytes, DXBC_HEADER_LEN + i * 4).ok_or(ShaderDbError::InvalidChunk {
                index: i,
                offset: DXBC_HEADER_LEN + i * 4,
            })? as usize;
            let header = bytes
                .get(off..off.saturating_add(8))
                .ok_or(ShaderDbError::InvalidChunk { index: i, offset: off })?;
            let fourcc: [u8; 4] = header
                .get(0..4)
                .and_then(|s| s.try_into().ok())
                .ok_or(ShaderDbError::InvalidChunk { index: i, offset: off })?;
            let chunk_size =
                read_u32_le(header, 4).ok_or(ShaderDbError::InvalidChunk { index: i, offset: off })? as usize;
            let payload_start = off.saturating_add(8);
            let payload_end = payload_start.saturating_add(chunk_size);
            if payload_end > total_size {
                return Err(ShaderDbError::InvalidChunk { index: i, offset: off });
            }
            let payload = bytes
                .get(payload_start..payload_end)
                .ok_or(ShaderDbError::InvalidChunk { index: i, offset: off })?
                .to_vec();
            chunks.push(DxbcChunk { fourcc, data: payload });
        }

        Ok((
            Self {
                version,
                digest,
                chunks,
            },
            total_size,
        ))
    }

    /// Parses an exact `DXBC` container slice.
    ///
    /// # Errors
    /// Returns [`ShaderDbError`] if the slice is malformed.
    pub fn parse(bytes: &[u8]) -> Result<Self, ShaderDbError> {
        let (container, _) = Self::parse_prefix(bytes)?;
        Ok(container)
    }

    /// Serializes this `DXBC` container and writes a fresh Microsoft DXBC MD5 checksum.
    ///
    /// # Errors
    /// Returns [`ShaderDbError::TooLarge`] if the serialized container exceeds `u32::MAX` bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>, ShaderDbError> {
        let table_len = self.chunks.len().checked_mul(4).ok_or(ShaderDbError::TooLarge)?;
        let header_and_table = DXBC_HEADER_LEN.checked_add(table_len).ok_or(ShaderDbError::TooLarge)?;
        let chunks_bytes: usize = self.chunks.iter().map(|c| 8 + c.data.len()).sum();
        let total_size = header_and_table
            .checked_add(chunks_bytes)
            .ok_or(ShaderDbError::TooLarge)?;
        let total_u32 = u32::try_from(total_size).map_err(|_| ShaderDbError::TooLarge)?;
        let count_u32 = u32::try_from(self.chunks.len()).map_err(|_| ShaderDbError::TooLarge)?;

        let mut out = Vec::with_capacity(total_size);
        out.extend_from_slice(DXBC_MAGIC);
        out.extend_from_slice(&[0u8; 16]); // placeholder for DXBC MD5 digest
        out.extend_from_slice(&self.version.to_le_bytes());
        out.extend_from_slice(&total_u32.to_le_bytes());
        out.extend_from_slice(&count_u32.to_le_bytes());

        let mut cursor = header_and_table;
        for chunk in &self.chunks {
            let off_u32 = u32::try_from(cursor).map_err(|_| ShaderDbError::TooLarge)?;
            out.extend_from_slice(&off_u32.to_le_bytes());
            cursor = cursor.saturating_add(8 + chunk.data.len());
        }
        for chunk in &self.chunks {
            let len_u32 = u32::try_from(chunk.data.len()).map_err(|_| ShaderDbError::TooLarge)?;
            out.extend_from_slice(&chunk.fourcc);
            out.extend_from_slice(&len_u32.to_le_bytes());
            out.extend_from_slice(&chunk.data);
        }

        let digest = dxbc_checksum(out.get(20..).unwrap_or_default());
        if let Some(slot) = out.get_mut(4..20) {
            slot.copy_from_slice(&digest);
        }
        Ok(out)
    }

    /// Removes debug/private chunks (`SPDB`, `SDBG`, `PRIV`) and returns how many were stripped.
    pub fn strip_debug_chunks(&mut self) -> usize {
        let before = self.chunks.len();
        self.chunks
            .retain(|c| !matches!(&c.fourcc, b"SPDB" | b"SDBG" | b"PRIV"));
        before - self.chunks.len()
    }

    /// Returns the shader stage decoded from the `SHDR` or `SHEX` bytecode chunk, if present.
    #[must_use]
    pub fn stage(&self) -> Option<ShaderStage> {
        let chunk = self.chunks.iter().find(|c| matches!(&c.fourcc, b"SHDR" | b"SHEX"))?;
        let ver_token = read_u32_le(&chunk.data, 0)?;
        match ver_token >> 16 {
            0 => Some(ShaderStage::Pixel),
            1 => Some(ShaderStage::Vertex),
            2 => Some(ShaderStage::Geometry),
            3 => Some(ShaderStage::Hull),
            4 => Some(ShaderStage::Domain),
            5 => Some(ShaderStage::Compute),
            _ => None,
        }
    }

    /// Returns `true` if `bytes` is a valid `DXBC` container whose stored digest matches
    /// [`dxbc_checksum`].
    #[must_use]
    pub fn verify_checksum(bytes: &[u8]) -> bool {
        let Some(stored) = bytes.get(4..20) else {
            return false;
        };
        let Some(body) = bytes.get(20..) else {
            return false;
        };
        if bytes.get(0..4) != Some(DXBC_MAGIC.as_slice()) {
            return false;
        }
        dxbc_checksum(body) == stored
    }
}

/// Computes the 16-byte Microsoft Direct3D `DXBC` MD5 checksum over `body` (`&dxbc[20..]`).
///
/// Uses standard MD5 compression with Direct3D's modified final padding block
/// (bit-length at offset 0 when `rem >= 56`, and `(bits >> 1) | 1` in the final word).
#[must_use]
pub fn dxbc_checksum(body: &[u8]) -> [u8; 16] {
    let mut state: [u32; 4] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476];

    let full_blocks = body.len() / 64;
    for i in 0..full_blocks {
        if let Some(chunk) = body.get(i * 64..(i + 1) * 64) {
            md5_compress(&mut state, chunk);
        }
    }

    let rem = body.len() % 64;
    let bits = (body.len() as u32).wrapping_mul(8);
    let last_word = ((body.len() as u32) << 1) | 1;

    if rem >= 56 {
        let mut block = [0u8; 64];
        if let (Some(dst), Some(src)) = (block.get_mut(..rem), body.get(full_blocks * 64..)) {
            dst.copy_from_slice(src);
        }
        if let Some(pad) = block.get_mut(rem) {
            *pad = 0x80;
        }
        md5_compress(&mut state, &block);

        let mut final_block = [0u8; 64];
        final_block[0..4].copy_from_slice(&bits.to_le_bytes());
        final_block[60..64].copy_from_slice(&last_word.to_le_bytes());
        md5_compress(&mut state, &final_block);
    } else {
        let mut block = [0u8; 64];
        block[0..4].copy_from_slice(&bits.to_le_bytes());
        if let (Some(dst), Some(src)) = (block.get_mut(4..4 + rem), body.get(full_blocks * 64..)) {
            dst.copy_from_slice(src);
        }
        if let Some(pad) = block.get_mut(4 + rem) {
            *pad = 0x80;
        }
        block[60..64].copy_from_slice(&last_word.to_le_bytes());
        md5_compress(&mut state, &block);
    }

    let mut digest = [0u8; 16];
    for (i, word) in state.iter().enumerate() {
        if let Some(slot) = digest.get_mut(i * 4..(i + 1) * 4) {
            slot.copy_from_slice(&word.to_le_bytes());
        }
    }
    digest
}

#[allow(clippy::many_single_char_names)]
fn md5_compress(state: &mut [u32; 4], block: &[u8]) {
    const K: [u32; 64] = [
        0xD76A_A478,
        0xE8C7_B756,
        0x2420_70DB,
        0xC1BD_CEEE,
        0xF57C_0FAF,
        0x4787_C62A,
        0xA830_4613,
        0xFD46_9501,
        0x6980_98D8,
        0x8B44_F7AF,
        0xFFFF_5BB1,
        0x895C_D7BE,
        0x6B90_1122,
        0xFD98_7193,
        0xA679_438E,
        0x49B4_0821,
        0xF61E_2562,
        0xC040_B340,
        0x265E_5A51,
        0xE9B6_C7AA,
        0xD62F_105D,
        0x0244_1453,
        0xD8A1_E681,
        0xE7D3_FBC8,
        0x21E1_CDE6,
        0xC337_07D6,
        0xF4D5_0D87,
        0x455A_14ED,
        0xA9E3_E905,
        0xFCEF_A3F8,
        0x676F_02D9,
        0x8D2A_4C8A,
        0xFFFA_3942,
        0x8771_F681,
        0x6D9D_6122,
        0xFDE5_380C,
        0xA4BE_EA44,
        0x4BDE_CFA9,
        0xF6BB_4B60,
        0xBEBF_BC70,
        0x289B_7EC6,
        0xEAA1_27FA,
        0xD4EF_3085,
        0x0488_1D05,
        0xD9D4_D039,
        0xE6DB_99E5,
        0x1FA2_7CF8,
        0xC4AC_5665,
        0xF429_2244,
        0x432A_FF97,
        0xAB94_23A7,
        0xFC93_A039,
        0x655B_59C3,
        0x8F0C_CC92,
        0xFFEF_F47D,
        0x8584_5DD1,
        0x6FA8_7E4F,
        0xFE2C_E6E0,
        0xA301_4314,
        0x4E08_11A1,
        0xF753_7E82,
        0xBD3A_F235,
        0x2AD7_D2BB,
        0xEB86_D391,
    ];
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14,
        20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6,
        10, 15, 21,
    ];

    let mut m = [0u32; 16];
    for (i, slot) in m.iter_mut().enumerate() {
        *slot = read_u32_le(block, i * 4).unwrap_or(0);
    }

    let [mut a, mut b, mut c, mut d] = *state;
    for i in 0..64 {
        let (f, g) = match i {
            0..=15 => ((b & c) | ((!b) & d), i),
            16..=31 => ((d & b) | ((!d) & c), (5 * i + 1) % 16),
            32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
            _ => (c ^ (b | (!d)), (7 * i) % 16),
        };
        let mg = m.get(g).copied().unwrap_or(0);
        let ki = K.get(i).copied().unwrap_or(0);
        let si = S.get(i).copied().unwrap_or(0);
        let temp = d;
        d = c;
        c = b;
        b = b.wrapping_add(a.wrapping_add(f).wrapping_add(ki).wrapping_add(mg).rotate_left(si));
        a = temp;
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
}

/// Options for [`repack_dxbc_database`].
#[derive(Debug, Clone, Default)]
pub struct RepackOptions {
    /// Strip debug chunks (`SPDB`, `SDBG`, `PRIV`) from each repacked `DXBC` container.
    pub strip_debug: bool,
    /// Optional replacement map from original 16-byte `DXBC` digest to replacement `DXBC` bytes.
    pub replacements: BTreeMap<[u8; 16], Vec<u8>>,
}

/// Summary report returned by [`repack_dxbc_database`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepackReport {
    /// Total valid `DXBC` containers found in the database.
    pub total_dxbc: usize,
    /// Number of `DXBC` containers replaced via [`RepackOptions::replacements`].
    pub replaced: usize,
    /// Total debug chunks stripped across all containers.
    pub stripped_chunks: usize,
    /// Net byte delta (`old_len - new_len`, positive means bytes saved).
    pub bytes_saved: isize,
    /// Counts of `DXBC` containers grouped by shader stage (`VS`, `PS`, `GS`, ...).
    pub stages: BTreeMap<String, usize>,
}

/// Scans a Frostbite `ShaderDatabase` blob for embedded `DXBC` containers, validates them,
/// optionally applies digest-keyed replacements and/or strips debug chunks, recomputes valid
/// `DXBC` MD5 checksums, and updates preceding `u32` little-endian length prefixes when present.
///
/// # Errors
/// Returns [`ShaderDbError`] if a replacement or repacked `DXBC` container cannot be serialized.
pub fn repack_dxbc_database(data: &[u8], opts: &RepackOptions) -> Result<(Vec<u8>, RepackReport), ShaderDbError> {
    let mut out = Vec::with_capacity(data.len());
    let mut report = RepackReport::default();
    let mut cursor = 0usize;

    for pos in memmem::find_iter(data, DXBC_MAGIC) {
        if pos < cursor {
            continue;
        }
        let Some(slice) = data.get(pos..) else {
            continue;
        };
        let Ok((mut dxbc, old_len)) = DxbcContainer::parse_prefix(slice) else {
            continue;
        };

        report.total_dxbc += 1;
        if let Some(stage) = dxbc.stage() {
            *report.stages.entry(stage.as_str().to_owned()).or_insert(0) += 1;
        }

        let new_bytes = if let Some(replacement) = opts.replacements.get(&dxbc.digest) {
            let mut rep = DxbcContainer::parse(replacement)?;
            if opts.strip_debug {
                report.stripped_chunks += rep.strip_debug_chunks();
            }
            report.replaced += 1;
            rep.to_bytes()?
        } else if opts.strip_debug {
            let stripped = dxbc.strip_debug_chunks();
            report.stripped_chunks += stripped;
            if stripped > 0 {
                dxbc.to_bytes()?
            } else {
                slice.get(..old_len).unwrap_or_default().to_vec()
            }
        } else {
            slice.get(..old_len).unwrap_or_default().to_vec()
        };

        // Check if the 4 bytes immediately before `pos` hold a u32 LE length equal to `old_len`.
        let has_len_prefix = pos >= cursor + 4 && read_u32_le(data, pos - 4).is_some_and(|v| v as usize == old_len);

        if has_len_prefix {
            if let Some(between) = data.get(cursor..pos - 4) {
                out.extend_from_slice(between);
            }
            let new_len_u32 = u32::try_from(new_bytes.len()).map_err(|_| ShaderDbError::TooLarge)?;
            out.extend_from_slice(&new_len_u32.to_le_bytes());
        } else if let Some(between) = data.get(cursor..pos) {
            out.extend_from_slice(between);
        }

        out.extend_from_slice(&new_bytes);
        cursor = pos + old_len;
    }

    if let Some(tail) = data.get(cursor..) {
        out.extend_from_slice(tail);
    }

    report.bytes_saved = data.len() as isize - out.len() as isize;
    Ok((out, report))
}

/// An Xbox 360 Xenos shader container discovered inside a console `ShaderDatabase`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XenosBlobEntry {
    /// Byte offset inside the source `ShaderDatabase`.
    pub offset: usize,
    /// Byte length of the XDK shader container.
    pub size: usize,
    /// Whether the shader is a pixel shader (`true`) or vertex shader (`false`).
    pub is_pixel: bool,
}

/// Scans an Xbox 360 `ShaderDatabase` blob for XDK Xenos shader containers
/// (`0x102A_1100`..`0x102A_1102`) and returns their validated `(offset, size, is_pixel)` spans.
#[must_use]
pub fn extract_xenos_shaders(data: &[u8]) -> Vec<XenosBlobEntry> {
    let mut found = Vec::new();
    let mut cursor = 0usize;
    for pos in memmem::find_iter(data, &[0x10, 0x2A, 0x11]) {
        if pos < cursor {
            continue;
        }
        let Some(magic) = read_u32_be(data, pos) else {
            continue;
        };
        if !matches!(magic, 0x102A_1100..=0x102A_1102) {
            continue;
        }
        let Some(hdr_size) = read_u32_be(data, pos + 4) else {
            continue;
        };
        let Some(ucode_size) = read_u32_be(data, pos + 8) else {
            continue;
        };
        let total = (hdr_size as usize).saturating_add(ucode_size as usize);
        if total < 0x24 || pos + total > data.len() {
            continue;
        }
        let Some(slice) = data.get(pos..pos + total) else {
            continue;
        };
        if let Ok(sh) = xenos::parse(slice, None) {
            found.push(XenosBlobEntry {
                offset: pos,
                size: total,
                is_pixel: sh.is_pixel,
            });
            cursor = pos + total;
        }
    }
    found
}

/// Discovers all Xenos shader containers in an Xbox 360 `ShaderDatabase` and translates
/// them to Dx10/Dx11 HLSL in parallel via Rayon.
#[must_use]
pub fn translate_xenos_database(
    data: &[u8],
    iface: &hlsl::Interface,
) -> Vec<(XenosBlobEntry, Result<String, x360_xenos_shader::Error>)> {
    let entries = extract_xenos_shaders(data);
    entries
        .into_par_iter()
        .map(|entry| {
            let slice = data.get(entry.offset..entry.offset + entry.size).unwrap_or_default();
            let res = xenos::parse(slice, Some(entry.is_pixel))
                .map_err(x360_xenos_shader::Error::from)
                .and_then(|sh| hlsl::translate(&sh, iface).map_err(x360_xenos_shader::Error::from));
            (entry, res)
        })
        .collect()
}

const DX9_HELPERS: &str = r"float3 tsizeLod(sampler2D t, float lod) { return float3(1024.0, 1024.0, floor(lod + 0.5)); }
float4 xcube(float3 d) {
    float3 a = abs(d);
    if (a.z >= a.x && a.z >= a.y) return float4(-d.y, d.z < 0.0 ? -d.x : d.x, 2.0 * d.z, d.z < 0.0 ? 5.0 : 4.0);
    if (a.y >= a.x) return float4(d.y < 0.0 ? -d.z : d.z, d.x, 2.0 * d.y, d.y < 0.0 ? 3.0 : 2.0);
    return float4(-d.y, d.x < 0.0 ? d.z : -d.z, 2.0 * d.x, d.x < 0.0 ? 1.0 : 0.0);
}
float4 xmul(float4 a, float4 b) { return (a == 0.0 || b == 0.0) ? 0.0 : a * b; }
float xmul1(float a, float b) { return (a == 0.0 || b == 0.0) ? 0.0 : a * b; }
float xdot4(float4 a, float4 b) { float4 m = xmul(a, b); return m.x + m.y + m.z + m.w; }
float xmax4(float4 v) { return max(max(v.x, v.y), max(v.z, v.w)); }
float xlog(float a) { float t = a == 1.0 ? 0.0 : log2(a); return (t < -3.4e38) ? -3.402823466e+38 : t; }
float xrcp(float a) { return a == 1.0 ? 1.0 : 1.0 / a; }
float xrcpc(float a) { float t = xrcp(a); return clamp(t, -3.402823466e+38, 3.402823466e+38); }
float xrcpf(float a) { float t = xrcp(a); return (abs(t) > 3.4e38) ? 0.0 : t; }
float xrsq(float a) { return a == 1.0 ? 1.0 : rsqrt(a); }
float xrsqc(float a) { float t = xrsq(a); return clamp(t, -3.402823466e+38, 3.402823466e+38); }
float xrsqf(float a) { float t = xrsq(a); return (abs(t) > 3.4e38) ? 0.0 : t; }
float2 xshadowuv(float3 consoleCoord) {
    float cascade = clamp(floor(consoleCoord.z * 3.0), 0.0, 2.0);
    float2 tile = cascade < 0.5 ? float2(0.0, 0.0) :
                  (cascade < 1.5 ? float2(0.5, 0.0) : float2(0.0, 0.5));
    return (consoleCoord.xy - 0.5) * (0.5 * (255.0 / 256.0)) + 0.25 + tile;
}
float2 tdim(sampler2D t) { return float2(1024.0, 1024.0); }
float3 xcubefetch(float3 c) {
    float sc = (c.x - 1.5) * 2.0, tc = (c.y - 1.5) * 2.0;
    int f = (int)floor(c.z + 0.5);
    if (f == 0) return float3(1.0, -tc, -sc);
    if (f == 1) return float3(-1.0, -tc, sc);
    if (f == 2) return float3(sc, 1.0, tc);
    if (f == 3) return float3(sc, -1.0, -tc);
    if (f == 4) return float3(sc, -tc, 1.0);
    return float3(-sc, -tc, -1.0);
}
float4 xsrgb(float4 c) { float3 l = saturate(c.rgb); return float4(l <= 0.0031308 ? l * 12.92 : 1.055 * pow(l, 1.0 / 2.4) - 0.055, c.a); }";

/// Lowers `x360-xenos-shader` Dx10/11 HLSL source to DirectX 9 Shader Model 3.0 (`vs_3_0` / `ps_3_0`) HLSL.
///
/// Replaces Dx10-only `HELPERS` (`Texture2D`, `uint`, `GetDimensions`, `isinf`, `round`) with SM 3.0
/// equivalents, rewrites system-value semantics (`SV_Position` -> `POSITION`, `SV_Target*` -> `COLOR*`,
/// `SV_Depth` -> `DEPTH`, `bool frontFace : SV_IsFrontFace` -> `float frontFace : VFACE`),
/// converts `Texture2D`/`TextureCube`/`Texture3D` + `SamplerState` pairs into `sampler2D`/`samplerCUBE`/`sampler3D`,
/// lowers `.Sample(...)` / `.SampleLevel(...)` / `.SampleGrad(...)` / `.SampleCmpLevelZero(...)` calls,
/// and flattens `cbuffer` wrappers for `fxc.exe /T vs_3_0` or `/T ps_3_0`.
#[must_use]
pub fn hlsl_dx10_to_dx9(hlsl_src: &str, is_pixel: bool) -> String {
    let preprocessed = hlsl_src.replace(hlsl::HELPERS, DX9_HELPERS);
    let mut out = String::with_capacity(preprocessed.len());
    let mut in_cbuffer = false;

    for line in preprocessed.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("cbuffer ") && trimmed.ends_with('{') {
            in_cbuffer = true;
            continue;
        }
        if in_cbuffer && trimmed == "};" {
            in_cbuffer = false;
            continue;
        }
        if trimmed.starts_with("SamplerState ") || trimmed.starts_with("SamplerComparisonState ") {
            continue;
        }
        if trimmed.starts_with("Texture2D ") {
            let rewritten = line
                .replacen("Texture2D ", "sampler2D ", 1)
                .replace(" : register(t", " : register(s");
            out.push_str(&rewritten);
            out.push('\n');
            continue;
        }
        if trimmed.starts_with("TextureCube ") {
            let rewritten = line
                .replacen("TextureCube ", "samplerCUBE ", 1)
                .replace(" : register(t", " : register(s");
            out.push_str(&rewritten);
            out.push('\n');
            continue;
        }
        if trimmed.starts_with("Texture3D ") {
            let rewritten = line
                .replacen("Texture3D ", "sampler3D ", 1)
                .replace(" : register(t", " : register(s");
            out.push_str(&rewritten);
            out.push('\n');
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }

    // Rewrite Dx10/11 texture sampling intrinsics -> Dx9 tex2D* calls
    if let Ok(re_cmp) = Regex::new(
        r"([A-Za-z_][A-Za-z0-9_]*)\.SampleCmpLevelZero\(\s*[A-Za-z_][A-Za-z0-9_]*\s*,\s*([^,]+),\s*([^)]+)\)",
    ) {
        out = re_cmp
            .replace_all(&out, "tex2Dlod($1, float4(($2).xy, $3, 0.0)).r")
            .into_owned();
    }
    if let Ok(re_sample) = Regex::new(r"([A-Za-z_][A-Za-z0-9_]*)\.Sample\(\s*[A-Za-z_][A-Za-z0-9_]*\s*,\s*([^)]+)\)") {
        out = re_sample.replace_all(&out, "tex2D($1, $2)").into_owned();
    }
    if let Ok(re_lod) =
        Regex::new(r"([A-Za-z_][A-Za-z0-9_]*)\.SampleLevel\(\s*[A-Za-z_][A-Za-z0-9_]*\s*,\s*([^,]+),\s*([^)]+)\)")
    {
        out = re_lod
            .replace_all(&out, "tex2Dlod($1, float4(($2).xy, 0.0, $3))")
            .into_owned();
    }
    if let Ok(re_grad) = Regex::new(
        r"([A-Za-z_][A-Za-z0-9_]*)\.SampleGrad\(\s*[A-Za-z_][A-Za-z0-9_]*\s*,\s*([^,]+),\s*([^,]+),\s*([^)]+)\)",
    ) {
        out = re_grad.replace_all(&out, "tex2Dgrad($1, $2, $3, $4)").into_owned();
    }

    // Rewrite Dx10/11 system semantics -> Dx9 SM 3.0 semantics & ensure SM 3.0 output structs are initialized
    out = out
        .replace("struct PSOut { };", "struct PSOut { float4 c0 : COLOR0; };")
        .replace("struct PSOut {};", "struct PSOut { float4 c0 : COLOR0; };")
        .replace("PSOut po;", "PSOut po = (PSOut)0;")
        .replace("PSOut o;", "PSOut o = (PSOut)0;")
        .replace("V2P o;", "V2P o = (V2P)0;")
        .replace("SV_Position", "POSITION")
        .replace("SV_POSITION", "POSITION")
        .replace("SV_Target0", "COLOR0")
        .replace("SV_Target1", "COLOR1")
        .replace("SV_Target2", "COLOR2")
        .replace("SV_Target3", "COLOR3")
        .replace("SV_Target", "COLOR0")
        .replace("SV_Depth", "DEPTH")
        .replace("bool frontFace : SV_IsFrontFace", "float frontFace : VFACE");

    if is_pixel {
        out = out.replace("frontFace ?", "(frontFace > 0.0) ?");
    }
    out
}

/// Auto-discovers Microsoft's `fxc.exe` HLSL compiler from `FXC_PATH`, `PATH`, or standard
/// Windows 10/11 SDK installations (`Windows Kits\10\bin\*\x64\fxc.exe`).
#[must_use]
pub fn find_fxc() -> Option<PathBuf> {
    if let Ok(env_path) = std::env::var("FXC_PATH") {
        let p = PathBuf::from(env_path);
        if p.is_file() {
            return Some(p);
        }
    }
    let kits_root = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    if let Ok(entries) = std::fs::read_dir(kits_root) {
        let mut versions: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
        versions.sort();
        for dir in versions.into_iter().rev() {
            for arch in ["x64", "x86"] {
                let cand = dir.join(arch).join("fxc.exe");
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }
    None
}

/// Compiles HLSL source to Direct3D bytecode (`DXBC` for [`TargetProfile::Dx10`]/[`TargetProfile::Dx11`],
/// or SM 3.0 bytecode for [`TargetProfile::Dx9`]) using `fxc.exe`.
///
/// # Errors
/// Returns [`ShaderDbError::Compile`] if `fxc.exe` fails to launch or rejects the HLSL source.
pub fn compile_hlsl_with_fxc(
    fxc: &Path,
    hlsl_src: &str,
    is_pixel: bool,
    profile: TargetProfile,
) -> Result<Vec<u8>, ShaderDbError> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tid = std::thread::current().id();
    let dir = std::env::temp_dir();
    let src_path = dir.join(format!("xenolith_sh_{stamp}_{tid:?}.hlsl"));
    let out_path = dir.join(format!("xenolith_sh_{stamp}_{tid:?}.bin"));

    std::fs::write(&src_path, hlsl_src).map_err(|e| ShaderDbError::Compile(format!("write temp hlsl: {e}")))?;
    let output = Command::new(fxc)
        .arg("/nologo")
        .arg("/T")
        .arg(profile.fxc_target(is_pixel))
        .arg("/E")
        .arg("main")
        .arg("/Fo")
        .arg(&out_path)
        .arg(&src_path)
        .output()
        .map_err(|e| ShaderDbError::Compile(format!("spawn {}: {e}", fxc.display())))?;

    let _ = std::fs::remove_file(&src_path);
    if !output.status.success() {
        let _ = std::fs::remove_file(&out_path);
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ShaderDbError::Compile(stderr.trim().to_owned()));
    }
    let bytecode = std::fs::read(&out_path).map_err(|e| ShaderDbError::Compile(format!("read fxc output: {e}")))?;
    let _ = std::fs::remove_file(&out_path);
    Ok(bytecode)
}

/// Discovers all Xenos shaders in an Xbox 360 `ShaderDatabase`, translates them to HLSL
/// (lowering to DirectX 9 SM 3.0 when `profile == TargetProfile::Dx9`), and compiles each
/// shader to PC bytecode in parallel via `fxc.exe`.
#[must_use]
pub fn compile_xenos_database(
    data: &[u8],
    fxc: &Path,
    profile: TargetProfile,
    iface: &hlsl::Interface,
) -> Vec<(XenosBlobEntry, Result<Vec<u8>, String>)> {
    let translated = translate_xenos_database(data, iface);
    translated
        .into_par_iter()
        .map(|(entry, hlsl_res)| {
            let compiled = hlsl_res.map_err(|e| e.to_string()).and_then(|src| {
                let final_src = if profile == TargetProfile::Dx9 {
                    hlsl_dx10_to_dx9(&src, entry.is_pixel)
                } else {
                    src
                };
                compile_hlsl_with_fxc(fxc, &final_src, entry.is_pixel, profile).map_err(|e| e.to_string())
            });
            (entry, compiled)
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn make_dxbc(stage_type: u32, with_debug: bool) -> Vec<u8> {
        let mut shdr = Vec::new();
        shdr.extend_from_slice(&((stage_type << 16) | 0x0040).to_le_bytes()); // sm 4.0
        shdr.extend_from_slice(&2u32.to_le_bytes());

        let mut chunks = vec![DxbcChunk {
            fourcc: *b"SHDR",
            data: shdr,
        }];
        if with_debug {
            chunks.push(DxbcChunk {
                fourcc: *b"SPDB",
                data: vec![0xAB; 48],
            });
        }
        let container = DxbcContainer {
            version: 1,
            digest: [0; 16],
            chunks,
        };
        container.to_bytes().unwrap()
    }

    #[test]
    fn dxbc_round_trip_and_checksum_verification() {
        let raw = make_dxbc(1, true);
        assert!(DxbcContainer::verify_checksum(&raw));
        let parsed = DxbcContainer::parse(&raw).unwrap();
        assert_eq!(parsed.stage(), Some(ShaderStage::Vertex));
        assert_eq!(parsed.chunks.len(), 2);

        let mut tampered = raw.clone();
        tampered[raw.len() - 1] ^= 0xFF;
        assert!(!DxbcContainer::verify_checksum(&tampered));
    }

    #[test]
    fn repacks_length_prefixed_shader_database_and_strips_debug() {
        let vs = make_dxbc(1, true);
        let ps = make_dxbc(0, true);
        let ps_parsed = DxbcContainer::parse(&ps).unwrap();
        let ps_replacement = make_dxbc(0, false);

        // Build a synthetic length-prefixed ShaderDatabase
        let mut db = b"FSD0".to_vec();
        db.extend_from_slice(&(vs.len() as u32).to_le_bytes());
        db.extend_from_slice(&vs);
        db.extend_from_slice(&(ps.len() as u32).to_le_bytes());
        db.extend_from_slice(&ps);
        db.extend_from_slice(b"TAIL");

        let mut opts = RepackOptions {
            strip_debug: true,
            replacements: BTreeMap::new(),
        };
        opts.replacements.insert(ps_parsed.digest, ps_replacement);

        let (repacked, report) = repack_dxbc_database(&db, &opts).unwrap();
        assert_eq!(report.total_dxbc, 2);
        assert_eq!(report.replaced, 1);
        assert_eq!(report.stripped_chunks, 1);
        assert!(report.bytes_saved > 0);
        assert_eq!(report.stages.get("VS"), Some(&1));
        assert_eq!(report.stages.get("PS"), Some(&1));
        assert!(repacked.starts_with(b"FSD0"));
        assert!(repacked.ends_with(b"TAIL"));

        // Verify the repacked length prefixes and DXBC checksums are valid
        let vs_new_len = read_u32_le(&repacked, 4).unwrap() as usize;
        let vs_slice = &repacked[8..8 + vs_new_len];
        assert!(DxbcContainer::verify_checksum(vs_slice));
        assert_eq!(DxbcContainer::parse(vs_slice).unwrap().chunks.len(), 1);
    }

    #[test]
    fn discovers_and_translates_xenos_shader_in_database() {
        let mut sh = vec![0u8; 0x24 + 32 + 24];
        let put = |b: &mut Vec<u8>, o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_be_bytes());
        put(&mut sh, 0, 0x102A_1100);
        put(&mut sh, 4, 0x24 + 32);
        put(&mut sh, 8, 24);
        put(&mut sh, 0x18, 0x24);
        put(&mut sh, 0x24 + 4, 24);
        put(&mut sh, 0x24 + 32, 0x0000_1001);
        put(&mut sh, 0x24 + 36, 0x2000);

        let mut db = vec![0xCC; 16];
        db.extend_from_slice(&sh);
        db.extend_from_slice(&[0xDD; 8]);

        let entries = extract_xenos_shaders(&db);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].offset, 16);
        assert_eq!(entries[0].size, sh.len());

        let translated = translate_xenos_database(&db, &hlsl::Interface::new(vec![]));
        assert_eq!(translated.len(), 1);
        let hlsl_src = translated[0].1.as_ref().unwrap();
        assert!(hlsl_src.starts_with("// translated from Xbox 360 (Xenos) microcode\n"));

        let dx9_src = hlsl_dx10_to_dx9(
            "cbuffer C : register(b0) {\n  float4 c0;\n};\nTexture2D tex0 : register(t0);\nSamplerState smp0 : register(s0);\nfloat4 main(float2 uv : TEXCOORD0, bool frontFace : SV_IsFrontFace) : SV_Target0 {\n  return tex0.Sample(smp0, uv) * (frontFace ? c0 : 1.0);\n}\n",
            true,
        );
        assert!(dx9_src.contains("sampler2D tex0 : register(s0);"));
        assert!(dx9_src.contains("tex2D(tex0, uv)"));
        assert!(dx9_src.contains("float frontFace : VFACE"));
        assert!(dx9_src.contains("COLOR0"));
        assert!(!dx9_src.contains("cbuffer"));

        if let Some(fxc) = find_fxc() {
            let dxbc = compile_hlsl_with_fxc(
                &fxc,
                "float4 main() : SV_Target0 { return float4(1,0,0,1); }",
                true,
                TargetProfile::Dx10,
            )
            .unwrap();
            assert!(DxbcContainer::verify_checksum(&dxbc));
            let dx9_bin = compile_hlsl_with_fxc(&fxc, &dx9_src, true, TargetProfile::Dx9).unwrap();
            assert_ne!(dx9_bin, [] as [u8; 0]);
            // End-to-end: compile actual x360_xenos_shader::hlsl::translate output lowered to DirectX 9 ps_3_0!
            let xenos_dx9 = hlsl_dx10_to_dx9(hlsl_src, true);
            let xenos_dx9_bin = compile_hlsl_with_fxc(&fxc, &xenos_dx9, true, TargetProfile::Dx9).unwrap();
            assert_ne!(xenos_dx9_bin, [] as [u8; 0]);
        }
    }
}
