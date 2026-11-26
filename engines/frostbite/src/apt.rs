//! EA APT UI movie (`BIGF` containing `.apt`, `.const`, `.geo`) Xbox 360 (big-endian) -> PC (little-endian)
//! structural converter and validator.
//!
//! Frostbite 1 / 1.5 stores every Flash/APT UI movie (`SwfMovie` `.res`) as a `BIGF` archive holding:
//! - `<name>.apt`: root `Movie` character (type 9, signature `0x09876543`), character table, frames, frame items,
//!   imports, exports, and AVM1 + EA `ActionScript` bytecode.
//! - `<name>.const`: constant table (`"Apt constant file\x1A\0\0"`, `movie_offset`, `count`, `entries_offset`,
//!   `count` × `{type, value}`, followed by NUL-terminated ASCII strings).
//! - `<name>.geo`: `"Apt PCGL geometry file\x1A\0"` followed by 32-bit `u32`/`f32` mesh/vertex records.

use std::collections::BTreeSet;

const APT_SIGNATURE: u32 = 0x0987_6543;

/// Errors returned while parsing or converting an EA APT `SwfMovie` resource.
#[derive(Debug, thiserror::Error)]
pub enum AptError {
    /// Input is not a valid `BIGF` archive or is truncated.
    #[error("invalid BIGF archive: {0}")]
    InvalidBigf(&'static str),
    /// `.const` header or table is malformed.
    #[error("invalid .const file: {0}")]
    InvalidConst(&'static str),
    /// `.apt` structure or movie root is malformed.
    #[error("invalid .apt file: {0}")]
    InvalidApt(&'static str),
    /// `.geo` header is malformed.
    #[error("invalid .geo file: {0}")]
    InvalidGeo(&'static str),
}

fn read_be32(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    let mut a = [0u8; 4];
    a.copy_from_slice(s);
    Some(u32::from_be_bytes(a))
}

fn read_le32(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    let mut a = [0u8; 4];
    a.copy_from_slice(s);
    Some(u32::from_le_bytes(a))
}

/// Unpacks a Frostbite `BIGF` archive into `(name, bytes)` pairs in original order.
///
/// # Errors
/// Returns [`AptError::InvalidBigf`] if the header or file ranges are invalid.
pub fn parse_bigf(raw: &[u8]) -> Result<Vec<(String, Vec<u8>)>, AptError> {
    if raw.len() < 16 || raw.get(0..4) != Some(b"BIGF") {
        return Err(AptError::InvalidBigf("missing BIGF magic"));
    }
    let count = read_be32(raw, 8).ok_or(AptError::InvalidBigf("truncated count"))? as usize;
    let mut pos = 16usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let off = read_be32(raw, pos).ok_or(AptError::InvalidBigf("truncated offset"))? as usize;
        let len = read_be32(raw, pos + 4).ok_or(AptError::InvalidBigf("truncated size"))? as usize;
        let rest = raw.get(pos + 8..).ok_or(AptError::InvalidBigf("truncated name"))?;
        let end = memchr::memchr(0, rest).ok_or(AptError::InvalidBigf("unterminated name"))?;
        let name = String::from_utf8_lossy(rest.get(..end).unwrap_or_default()).into_owned();
        pos += 8 + end + 1;
        let data = raw
            .get(off..off + len)
            .ok_or(AptError::InvalidBigf("member out of bounds"))?
            .to_vec();
        out.push((name, data));
    }
    Ok(out)
}

/// Repacks `(name, bytes)` pairs into a 4-byte-aligned `BIGF` archive.
#[must_use]
pub fn write_bigf(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let index_size: usize = files.iter().map(|(n, _)| 8 + n.len() + 1).sum();
    let mut pos = 16 + index_size;
    let mut offsets = Vec::with_capacity(files.len());
    for (_, data) in files {
        pos = (pos + 3) & !3;
        offsets.push(pos);
        pos += data.len();
    }
    let mut out = vec![0u8; pos];
    if let Some(dst) = out.get_mut(0..4) {
        dst.copy_from_slice(b"BIGF");
    }
    if let Some(dst) = out.get_mut(4..8) {
        dst.copy_from_slice(&(pos as u32).to_le_bytes());
    }
    if let Some(dst) = out.get_mut(8..12) {
        dst.copy_from_slice(&(files.len() as u32).to_be_bytes());
    }
    if let Some(dst) = out.get_mut(12..16) {
        dst.copy_from_slice(&((16 + index_size) as u32).to_be_bytes());
    }
    let mut ip = 16usize;
    for ((name, data), &off) in files.iter().zip(&offsets) {
        if let Some(dst) = out.get_mut(ip..ip + 4) {
            dst.copy_from_slice(&(off as u32).to_be_bytes());
        }
        if let Some(dst) = out.get_mut(ip + 4..ip + 8) {
            dst.copy_from_slice(&(data.len() as u32).to_be_bytes());
        }
        let nb = name.as_bytes();
        if let Some(dst) = out.get_mut(ip + 8..ip + 8 + nb.len()) {
            dst.copy_from_slice(nb);
        }
        ip += 8 + nb.len() + 1;
        if let Some(dst) = out.get_mut(off..off + data.len()) {
            dst.copy_from_slice(data);
        }
    }
    out
}

/// Converts a big-endian `.const` file to little-endian and returns `(converted_bytes, movie_offset)`.
///
/// # Errors
/// Returns [`AptError::InvalidConst`] if the header or constant entries are invalid.
pub fn convert_const_be_to_le(src: &[u8]) -> Result<(Vec<u8>, usize), AptError> {
    if src.len() < 32 || src.get(0..17) != Some(b"Apt constant file") {
        return Err(AptError::InvalidConst("missing magic"));
    }
    let movie_off = read_be32(src, 20).ok_or(AptError::InvalidConst("movie_off"))?;
    let count = read_be32(src, 24).ok_or(AptError::InvalidConst("count"))?;
    let entries_off = read_be32(src, 28).ok_or(AptError::InvalidConst("entries_off"))?;

    let mut out = src.to_vec();
    for (at, val) in [(20usize, movie_off), (24, count), (28, entries_off)] {
        if let Some(dst) = out.get_mut(at..at + 4) {
            dst.copy_from_slice(&val.to_le_bytes());
        }
    }

    let base = entries_off as usize;
    for i in 0..count as usize {
        let p = base + 8 * i;
        let ty = read_be32(src, p).ok_or(AptError::InvalidConst("entry type"))?;
        let val = read_be32(src, p + 4).ok_or(AptError::InvalidConst("entry value"))?;
        if let Some(dst) = out.get_mut(p..p + 4) {
            dst.copy_from_slice(&ty.to_le_bytes());
        }
        if let Some(dst) = out.get_mut(p + 4..p + 8) {
            dst.copy_from_slice(&val.to_le_bytes());
        }
    }
    Ok((out, movie_off as usize))
}

/// Converts a big-endian `.geo` (`Apt PCGL geometry file`) to little-endian.
///
/// Every field after the 24-byte ASCII magic is a 32-bit `u32`/`f32` word.
///
/// # Errors
/// Returns [`AptError::InvalidGeo`] if the header magic is missing.
pub fn convert_geo_be_to_le(src: &[u8]) -> Result<Vec<u8>, AptError> {
    if src.len() < 32 || src.get(0..22) != Some(b"Apt PCGL geometry file") {
        return Err(AptError::InvalidGeo("missing PCGL magic"));
    }
    let mut out = src.to_vec();
    let mut pos = 24usize;
    while pos + 4 <= out.len() {
        if let Some(val) = read_be32(src, pos)
            && let Some(dst) = out.get_mut(pos..pos + 4)
        {
            dst.copy_from_slice(&val.to_le_bytes());
        }
        pos += 4;
    }
    Ok(out)
}

/// Tracks swapped offsets so shared structures or action blocks are never byte-swapped twice.
struct AptSwapper<'a> {
    src: &'a [u8],
    out: Vec<u8>,
    swapped_words: BTreeSet<usize>,
    visited_actions: BTreeSet<usize>,
    visited_chars: BTreeSet<usize>,
}

impl<'a> AptSwapper<'a> {
    fn new(src: &'a [u8]) -> Self {
        Self {
            src,
            out: src.to_vec(),
            swapped_words: BTreeSet::new(),
            visited_actions: BTreeSet::new(),
            visited_chars: BTreeSet::new(),
        }
    }

    fn u32_be(&self, off: usize) -> Option<u32> {
        read_be32(self.src, off)
    }

    fn swap_u32(&mut self, off: usize) -> Option<u32> {
        let val = read_be32(self.src, off)?;
        if self.swapped_words.insert(off)
            && let Some(dst) = self.out.get_mut(off..off + 4)
        {
            dst.copy_from_slice(&val.to_le_bytes());
        }
        Some(val)
    }

    fn swap_u32_range(&mut self, start: usize, count: usize) {
        for i in 0..count {
            let _ = self.swap_u32(start + 4 * i);
        }
    }

    fn swap_u16_unaligned(&mut self, off: usize) {
        if let Some(s) = self.src.get(off..off + 2)
            && let (Some(&b0), Some(&b1)) = (s.first(), s.get(1))
        {
            let v = u16::from_be_bytes([b0, b1]);
            if let Some(dst) = self.out.get_mut(off..off + 2) {
                dst.copy_from_slice(&v.to_le_bytes());
            }
        }
    }

    fn swap_u32_unaligned(&mut self, off: usize) {
        if let Some(v) = read_be32(self.src, off)
            && let Some(dst) = self.out.get_mut(off..off + 4)
        {
            dst.copy_from_slice(&v.to_le_bytes());
        }
    }

    fn convert_movie(&mut self, movie_off: usize) -> Result<(), AptError> {
        let ty = self
            .u32_be(movie_off)
            .ok_or(AptError::InvalidApt("truncated movie header"))?;
        let sig = self
            .u32_be(movie_off + 4)
            .ok_or(AptError::InvalidApt("truncated movie sig"))?;
        if ty != 9 || sig != APT_SIGNATURE {
            return Err(AptError::InvalidApt("root is not Movie(9) with 0x09876543"));
        }

        // Movie header: 16 u32 words (type, sig, frameCount, framesPtr, ptr, charCount, charsPtr,
        // width, height, msPerFrame, importCount, importsPtr, exportCount, exportsPtr, count2, ptr2)
        self.swap_u32_range(movie_off, 16);

        let frame_count = self.u32_be(movie_off + 8).unwrap_or(0) as usize;
        let frames_ptr = self.u32_be(movie_off + 12).unwrap_or(0) as usize;
        let char_count = self.u32_be(movie_off + 20).unwrap_or(0) as usize;
        let chars_ptr = self.u32_be(movie_off + 24).unwrap_or(0) as usize;
        let import_count = self.u32_be(movie_off + 40).unwrap_or(0) as usize;
        let imports_ptr = self.u32_be(movie_off + 44).unwrap_or(0) as usize;
        let export_count = self.u32_be(movie_off + 48).unwrap_or(0) as usize;
        let exports_ptr = self.u32_be(movie_off + 52).unwrap_or(0) as usize;

        // Swap imports table (16-byte rows: moviePtr, symbolPtr, charId, reserved)
        if imports_ptr != 0 {
            self.swap_u32_range(imports_ptr, import_count * 4);
        }
        // Swap exports table (8-byte rows: symbolPtr, charId)
        if exports_ptr != 0 {
            self.swap_u32_range(exports_ptr, export_count * 2);
        }

        // Swap root movie frames
        self.convert_frames(frames_ptr, frame_count);

        // Swap characters pointer table & each character
        if chars_ptr != 0 {
            self.swap_u32_range(chars_ptr, char_count);
            for i in 0..char_count {
                let cp = self.u32_be(chars_ptr + 4 * i).unwrap_or(0) as usize;
                if cp != 0 && cp != movie_off {
                    self.convert_character(cp);
                }
            }
        }
        Ok(())
    }

    fn convert_character(&mut self, cp: usize) {
        if !self.visited_chars.insert(cp) {
            return;
        }
        let Some(ct) = self.u32_be(cp) else { return };
        match ct {
            1 => {
                // Shape: type, sig, 4 floats bounds, geo_id (7 words)
                self.swap_u32_range(cp, 7);
            }
            2 => {
                // EditText: type, sig, 4 bounds, font_id, align, color, font_height, flags,
                // read_only, multiline, word_wrap, text_ptr, var_ptr (16 words)
                self.swap_u32_range(cp, 16);
            }
            3 => {
                // Font: type, sig, name_ptr, glyph_count, glyphs_ptr (5 words)
                self.swap_u32_range(cp, 5);
                let gcount = self.u32_be(cp + 12).unwrap_or(0) as usize;
                let gptr = self.u32_be(cp + 16).unwrap_or(0) as usize;
                if gptr != 0 {
                    self.swap_u32_range(gptr, gcount);
                }
            }
            5 => {
                // Sprite: type, sig, frame_count, frames_ptr, ptr (5 words)
                self.swap_u32_range(cp, 5);
                let fcount = self.u32_be(cp + 8).unwrap_or(0) as usize;
                let fptr = self.u32_be(cp + 12).unwrap_or(0) as usize;
                self.convert_frames(fptr, fcount);
            }
            6 | 7 => {
                // Sound (6) / Image (7): type, sig, id (3 words)
                self.swap_u32_range(cp, 3);
            }
            10 => {
                // StaticText: type, sig, 4 bounds, 6 matrix, record_count, records_ptr (14 words)
                self.swap_u32_range(cp, 14);
                let rcount = self.u32_be(cp + 48).unwrap_or(0) as usize;
                let rptr = self.u32_be(cp + 52).unwrap_or(0) as usize;
                if rptr != 0 {
                    for r in 0..rcount {
                        let rec = rptr + r * 28;
                        // font_id, color, offset_x, offset_y, height, glyph_count, glyphs_ptr (7 words)
                        self.swap_u32_range(rec, 7);
                        let gcount = self.u32_be(rec + 20).unwrap_or(0) as usize;
                        let gptr = self.u32_be(rec + 24).unwrap_or(0) as usize;
                        if gptr != 0 {
                            self.swap_u32_range(gptr, gcount * 2);
                        }
                    }
                }
            }
            _ => {
                self.swap_u32_range(cp, 4);
            }
        }
    }

    fn convert_frames(&mut self, frames_ptr: usize, frame_count: usize) {
        if frames_ptr == 0 || frame_count == 0 {
            return;
        }
        self.swap_u32_range(frames_ptr, frame_count * 2);
        for f in 0..frame_count {
            let n = self.u32_be(frames_ptr + 8 * f).unwrap_or(0) as usize;
            let items_ptr = self.u32_be(frames_ptr + 8 * f + 4).unwrap_or(0) as usize;
            if items_ptr == 0 || n == 0 {
                continue;
            }
            self.swap_u32_range(items_ptr, n);
            for k in 0..n {
                let it = self.u32_be(items_ptr + 4 * k).unwrap_or(0) as usize;
                if it != 0 {
                    self.convert_frame_item(it);
                }
            }
        }
    }

    fn convert_frame_item(&mut self, it: usize) {
        let Some(ty) = self.u32_be(it) else { return };
        match ty {
            1 => {
                // Action: type, bytecode_ptr (2 words)
                self.swap_u32_range(it, 2);
                if let Some(bc) = self.u32_be(it + 4) {
                    self.convert_action_block(bc as usize, self.src.len());
                }
            }
            2 => {
                // FrameLabel: type, label_ptr, flags, frame (4 words)
                self.swap_u32_range(it, 4);
            }
            3 => {
                // PlaceObject: 16 words (0x40 bytes):
                // type, flags, depth, character, matrix[6], color, unknown, ratio, namePtr(+0x34),
                // clipDepth(+0x38), clipEventsPtr(+0x3C)
                self.swap_u32_range(it, 16);
                let clip = self.u32_be(it + 0x3C).unwrap_or(0) as usize;
                if clip != 0 && clip + 8 <= self.src.len() {
                    self.swap_u32_range(clip, 2);
                    let cn = self.u32_be(clip).unwrap_or(0) as usize;
                    let cp = self.u32_be(clip + 4).unwrap_or(0) as usize;
                    if cp != 0 {
                        for c in 0..cn {
                            let ev = cp + 12 * c;
                            self.swap_u32_range(ev, 3);
                            if let Some(bc) = self.u32_be(ev + 8) {
                                self.convert_action_block(bc as usize, self.src.len());
                            }
                        }
                    }
                }
            }
            4 | 5 => {
                // RemoveObject (depth) / BackgroundColor (color): 2 words
                self.swap_u32_range(it, 2);
            }
            8 => {
                // InitAction: type, sprite_id, bytecode_ptr (3 words)
                self.swap_u32_range(it, 3);
                if let Some(bc) = self.u32_be(it + 8) {
                    self.convert_action_block(bc as usize, self.src.len());
                }
            }
            _ => {
                let _ = self.swap_u32(it);
            }
        }
    }

    fn convert_action_block(&mut self, start: usize, limit: usize) {
        if start == 0 || start >= self.src.len() || !self.visited_actions.insert(start) {
            return;
        }
        let mut p = start;
        let mut guard = 0usize;
        while p < self.src.len() && p < limit && guard < 200_000 {
            guard += 1;
            let op = self.src.get(p).copied().unwrap_or(0);
            p += 1;
            match op {
                0x00 => {
                    if limit == self.src.len() {
                        break;
                    }
                }
                // 0-byte operand opcodes (< 0x80 except 0x77) + 0x8A/0x8D/0x9A/0x9E
                0x01..=0x76 | 0x8A | 0x8D | 0x9A | 0x9E => {}
                // 1-byte operand opcodes
                0xA2 | 0xAE..=0xB3 | 0xB5 | 0xB9 => {
                    p += 1;
                }
                // 2-byte unaligned u16/i16 operand opcodes
                0xA3 | 0xB6 => {
                    self.swap_u16_unaligned(p);
                    p += 2;
                }
                // 4-byte unaligned float/int operand opcodes
                0x77 | 0xB4 | 0xB7 => {
                    self.swap_u32_unaligned(p);
                    p += 4;
                }
                // Aligned single 4-byte operand (int / branch offset / string ptr)
                0x81 | 0x87 | 0x8B | 0x8C | 0x94 | 0x99 | 0x9D | 0x9F | 0xA1 | 0xA4..=0xA7 | 0xB8 => {
                    p = (p + 3) & !3;
                    let _ = self.swap_u32(p);
                    p += 4;
                }
                // GetURL: aligned 2 string pointers
                0x83 => {
                    p = (p + 3) & !3;
                    self.swap_u32_range(p, 2);
                    p += 8;
                }
                // ConstantPool (0x88) & PushData (0x96): aligned {u32 count, u32 list_ptr} + list of u32
                0x88 | 0x96 => {
                    p = (p + 3) & !3;
                    let n = self.swap_u32(p).unwrap_or(0) as usize;
                    let ptr = self.swap_u32(p + 4).unwrap_or(0) as usize;
                    if ptr != 0 {
                        self.swap_u32_range(ptr, n);
                    }
                    p += 8;
                }
                // Try (0x8F): aligned 5 u32 words (20 bytes)
                0x8F => {
                    p = (p + 3) & !3;
                    self.swap_u32_range(p, 5);
                    p += 20;
                }
                // DefineFunction (0x9B): aligned 6 u32 words (24 bytes) + params_ptr[nParams]
                0x9B => {
                    p = (p + 3) & !3;
                    self.swap_u32_range(p, 6);
                    let n_params = self.u32_be(p + 4).unwrap_or(0) as usize;
                    let params_ptr = self.u32_be(p + 8).unwrap_or(0) as usize;
                    let body_size = self.u32_be(p + 12).unwrap_or(0) as usize;
                    if params_ptr != 0 {
                        self.swap_u32_range(params_ptr, n_params);
                    }
                    let body_start = p + 24;
                    let body_end = body_start.saturating_add(body_size).min(self.src.len());
                    self.convert_action_block(body_start, body_end);
                    p = body_end;
                }
                // DefineFunction2 (0x8E): aligned 7 u32 words (28 bytes) + params_ptr[nParams * 2]
                0x8E => {
                    p = (p + 3) & !3;
                    self.swap_u32_range(p, 7);
                    let n_params = self.u32_be(p + 4).unwrap_or(0) as usize;
                    let params_ptr = self.u32_be(p + 12).unwrap_or(0) as usize;
                    let body_size = self.u32_be(p + 16).unwrap_or(0) as usize;
                    if params_ptr != 0 {
                        self.swap_u32_range(params_ptr, n_params * 2);
                    }
                    let body_start = p + 28;
                    let body_end = body_start.saturating_add(body_size).min(self.src.len());
                    self.convert_action_block(body_start, body_end);
                    p = body_end;
                }
                _ => {
                    break;
                }
            }
        }
    }
}

fn has_ext(name: &str, ext: &str) -> bool {
    std::path::Path::new(name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

/// Converts a complete Xbox 360 big-endian `SwfMovie` (`BIGF` archive of `.apt` + `.const` + `.geo`)
/// into a PC little-endian `SwfMovie` resource.
///
/// # Errors
/// Returns [`AptError`] if any member is missing or structurally invalid.
pub fn convert_bigf_be_to_le(raw: &[u8]) -> Result<Vec<u8>, AptError> {
    let files = parse_bigf(raw)?;
    let const_src = files
        .iter()
        .find(|(n, _)| has_ext(n, "const"))
        .map(|(_, d)| d.as_slice())
        .ok_or(AptError::InvalidConst("missing .const member"))?;
    let (const_le, movie_off) = convert_const_be_to_le(const_src)?;

    let mut out_files = Vec::with_capacity(files.len());
    for (name, data) in files {
        if has_ext(&name, "const") {
            out_files.push((name, const_le.clone()));
        } else if has_ext(&name, "geo") {
            let geo_le = convert_geo_be_to_le(&data)?;
            out_files.push((name, geo_le));
        } else if has_ext(&name, "apt") {
            let mut swapper = AptSwapper::new(&data);
            swapper.convert_movie(movie_off)?;
            // Verify LE root movie header after swap
            let ty = read_le32(&swapper.out, movie_off).unwrap_or(0);
            let sig = read_le32(&swapper.out, movie_off + 4).unwrap_or(0);
            if ty != 9 || sig != APT_SIGNATURE {
                return Err(AptError::InvalidApt("post-swap LE verification failed"));
            }
            out_files.push((name, swapper.out));
        } else {
            out_files.push((name, data));
        }
    }
    Ok(write_bigf(&out_files))
}

/// Pure-Rust append-only patcher for a little-endian PC `SwfMovie` (`BIGF` archive).
///
/// Appending strings, bytecode blocks, and expanded frame-item tables to the end of `.apt`
/// preserves every existing offset and pointer in the movie without relocating anything.
pub struct AptPatcher {
    files: Vec<(String, Vec<u8>)>,
    apt_idx: usize,
    movie_off: usize,
}

impl AptPatcher {
    /// Opens a little-endian PC `SwfMovie` (`BIGF`) for patching.
    ///
    /// # Errors
    /// Returns [`AptError`] if the archive is missing `.apt` or `.const` or has an invalid root.
    pub fn from_bigf_le(raw: &[u8]) -> Result<Self, AptError> {
        let files = parse_bigf(raw)?;
        let apt_idx = files
            .iter()
            .position(|(n, _)| has_ext(n, "apt"))
            .ok_or(AptError::InvalidApt("missing .apt member"))?;
        let const_bytes = files
            .iter()
            .find(|(n, _)| has_ext(n, "const"))
            .map(|(_, d)| d.as_slice())
            .ok_or(AptError::InvalidConst("missing .const member"))?;
        let movie_off = read_le32(const_bytes, 20).ok_or(AptError::InvalidConst("truncated movie_off"))? as usize;
        Ok(Self {
            files,
            apt_idx,
            movie_off,
        })
    }

    fn apt_mut(&mut self) -> Result<&mut Vec<u8>, AptError> {
        self.files
            .get_mut(self.apt_idx)
            .map(|(_, d)| d)
            .ok_or(AptError::InvalidApt("invalid apt index"))
    }

    fn apt_ref(&self) -> Result<&[u8], AptError> {
        self.files
            .get(self.apt_idx)
            .map(|(_, d)| d.as_slice())
            .ok_or(AptError::InvalidApt("invalid apt index"))
    }

    fn align4_apt(&mut self) -> Result<usize, AptError> {
        let a = self.apt_mut()?;
        while !a.len().is_multiple_of(4) {
            a.push(0);
        }
        Ok(a.len())
    }

    /// Appends a NUL-terminated ASCII string (4-byte aligned) to `.apt` and returns its offset.
    ///
    /// # Errors
    /// Returns [`AptError::InvalidApt`] if the internal `.apt` buffer is unavailable.
    pub fn append_cstr(&mut self, s: &str) -> Result<u32, AptError> {
        let off = self.align4_apt()?;
        let a = self.apt_mut()?;
        a.extend_from_slice(s.as_bytes());
        a.push(0);
        Ok(off as u32)
    }

    /// Appends an `Action` frame item (`type = 1`) carrying `bytecode` to root movie frame `frame_idx`.
    ///
    /// # Errors
    /// Returns [`AptError::InvalidApt`] if `frame_idx` is out of range.
    pub fn append_root_frame_action(&mut self, frame_idx: usize, bytecode: &[u8]) -> Result<(), AptError> {
        let (frame_count, frames_ptr) = {
            let a = self.apt_ref()?;
            let fc = read_le32(a, self.movie_off + 8).ok_or(AptError::InvalidApt("frame_count"))? as usize;
            let fp = read_le32(a, self.movie_off + 12).ok_or(AptError::InvalidApt("frames_ptr"))? as usize;
            (fc, fp)
        };
        if frame_idx >= frame_count {
            return Err(AptError::InvalidApt("frame_idx out of bounds"));
        }

        // 1. Append bytecode (4-byte aligned)
        let bc_off = self.align4_apt()? as u32;
        self.apt_mut()?.extend_from_slice(bytecode);

        // 2. Append Action FrameItem { type = 1, bytecode_ptr }
        let item_off = self.align4_apt()? as u32;
        {
            let a = self.apt_mut()?;
            a.extend_from_slice(&1u32.to_le_bytes());
            a.extend_from_slice(&bc_off.to_le_bytes());
        }

        // 3. Clone existing frame items pointer list + append new Action item pointer
        let fr_off = frames_ptr + 8 * frame_idx;
        let (old_cnt, old_ptr) = {
            let a = self.apt_ref()?;
            let c = read_le32(a, fr_off).ok_or(AptError::InvalidApt("old item_count"))? as usize;
            let p = read_le32(a, fr_off + 4).ok_or(AptError::InvalidApt("old items_ptr"))? as usize;
            (c, p)
        };
        let mut new_list = Vec::with_capacity((old_cnt + 1) * 4);
        if old_cnt > 0 {
            let slice = self
                .apt_ref()?
                .get(old_ptr..old_ptr + old_cnt * 4)
                .ok_or(AptError::InvalidApt("old items slice"))?;
            new_list.extend_from_slice(slice);
        }
        new_list.extend_from_slice(&item_off.to_le_bytes());

        let new_list_off = self.align4_apt()? as u32;
        let a = self.apt_mut()?;
        a.extend_from_slice(&new_list);

        // 4. Patch frame descriptor { item_count + 1, new_list_off }
        let new_cnt = (old_cnt + 1) as u32;
        if let Some(dst) = a.get_mut(fr_off..fr_off + 4) {
            dst.copy_from_slice(&new_cnt.to_le_bytes());
        }
        if let Some(dst) = a.get_mut(fr_off + 4..fr_off + 8) {
            dst.copy_from_slice(&new_list_off.to_le_bytes());
        }
        Ok(())
    }

    /// Patches BF1943's `main_nav.apt` Frame 0 so it defines `init()` (called by BFBC2 PC's `main_root`
    /// Frame 36 `"hide_splash"`) and jumps directly to Frame 1 (`"start"`), emitting `FSCommand:bf1943NavInit`
    /// and `FSCommand:bf1943NavStart` telemetry hooks.
    ///
    /// # Errors
    /// Returns [`AptError`] if patching fails.
    pub fn patch_bf1943_main_nav_entry(&mut self) -> Result<(), AptError> {
        fn pad4(v: &mut Vec<u8>) {
            while !v.len().is_multiple_of(4) {
                v.push(0);
            }
        }

        let s_fs_init = self.append_cstr("FSCommand:bf1943NavInit")?;
        let s_fs_start = self.append_cstr("FSCommand:bf1943NavStart")?;
        let s_empty = self.append_cstr("")?;
        let s_init = self.append_cstr("init")?;
        let s_start = self.append_cstr("start")?;

        // Build bytecode starting at a 4-byte aligned offset so relative alignments are exact!
        let mut bc = Vec::<u8>::new();

        // 1. GetURL "FSCommand:bf1943NavInit", ""
        bc.push(0x83);
        pad4(&mut bc);
        bc.extend_from_slice(&s_fs_init.to_le_bytes());
        bc.extend_from_slice(&s_empty.to_le_bytes());

        // 2. Build `init()` function body (starts at a 4-byte aligned offset because DefineFunction header is 24 bytes):
        //    GetURL "FSCommand:bf1943NavStart", ""
        //    GotoLabel "start"
        //    Stop
        let mut body = Vec::<u8>::new();
        body.push(0x83);
        pad4(&mut body);
        body.extend_from_slice(&s_fs_start.to_le_bytes());
        body.extend_from_slice(&s_empty.to_le_bytes());
        body.push(0x8C); // GotoLabel
        pad4(&mut body);
        body.extend_from_slice(&s_start.to_le_bytes());
        body.push(0x07); // Stop (NEVER pad trailing 0x00 bytes — 0x00 is the End opcode!)

        // DefineFunction (0x9B): aligned 24-byte header { namePtr, nParams=0, paramsPtr=0, size, marker1, marker2 }
        bc.push(0x9B);
        pad4(&mut bc);
        bc.extend_from_slice(&s_init.to_le_bytes());
        bc.extend_from_slice(&0u32.to_le_bytes()); // nParams = 0
        bc.extend_from_slice(&0u32.to_le_bytes()); // paramsPtr = 0
        bc.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bc.extend_from_slice(&0x9876_5432u32.to_le_bytes());
        bc.extend_from_slice(&0x1234_5678u32.to_le_bytes());
        bc.extend_from_slice(&body);

        // 3. Also advance to "start" right away on Frame 0
        bc.push(0x8C);
        pad4(&mut bc);
        bc.extend_from_slice(&s_start.to_le_bytes());
        bc.push(0x07); // Stop
        bc.push(0x00); // End

        self.append_root_frame_action(0, &bc)
    }

    /// Rebuilds the patched `BIGF` archive bytes.
    #[must_use]
    pub fn into_bigf(self) -> Vec<u8> {
        write_bigf(&self.files)
    }
}

/// Symbol inventory extracted from an EA APT `BIGF` `.const` table for UI/menu hookup auditing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AptSymbolReport {
    /// Total string constants found in `.const`.
    pub string_count: usize,
    /// `FSCommand:` command strings (or `"FSCommand"` / `"fscommand"` targets).
    pub fscommands: Vec<String>,
    /// `extern.*` variable/callback names queried from the game engine.
    pub externs: Vec<String>,
    /// Xbox 360 controller prompt / button token strings needing KB+M prompt swaps on PC.
    pub prompts: Vec<String>,
}

/// Inspects an EA APT `BIGF` movie (Xbox 360 big-endian or PC little-endian) and extracts its
/// `FSCommand`, `extern.*`, and console button-prompt symbol inventory from `.const`.
///
/// # Errors
/// Returns [`AptError`] if the `BIGF` archive or its `.const` member is missing or malformed.
pub fn inspect_apt_symbols(bigf: &[u8]) -> Result<AptSymbolReport, AptError> {
    let files = parse_bigf(bigf)?;
    let (_, const_bytes) = files
        .iter()
        .find(|(n, _)| n.to_ascii_lowercase().ends_with(".const"))
        .ok_or(AptError::InvalidConst("no .const member in BIGF"))?;
    if const_bytes.len() < 32 || const_bytes.get(0..18) != Some(b"Apt constant file\x1A") {
        return Err(AptError::InvalidConst("missing Apt constant file header"));
    }

    // Detect endianness: entries_offset at 28..32 should be 32 (0x20) in either BE or LE.
    let big_endian = read_be32(const_bytes, 28) == Some(32);
    let read_u32 = |off: usize| -> Option<u32> {
        if big_endian {
            read_be32(const_bytes, off)
        } else {
            read_le32(const_bytes, off)
        }
    };
    let count = read_u32(24).ok_or(AptError::InvalidConst("truncated count"))? as usize;
    let entries_off = read_u32(28).ok_or(AptError::InvalidConst("truncated entries_offset"))? as usize;

    let mut fscommands = BTreeSet::new();
    let mut externs = BTreeSet::new();
    let mut prompts = BTreeSet::new();
    let mut string_count = 0usize;

    for i in 0..count {
        let off = entries_off.saturating_add(i.saturating_mul(8));
        let Some(kind) = read_u32(off) else {
            break;
        };
        let Some(val) = read_u32(off + 4) else {
            break;
        };
        if kind != 1 {
            continue;
        }
        let s_off = val as usize;
        let Some(rest) = const_bytes.get(s_off..) else {
            continue;
        };
        let Some(end) = memchr::memchr(0, rest) else {
            continue;
        };
        let s = String::from_utf8_lossy(rest.get(..end).unwrap_or_default()).into_owned();
        if s.is_empty() {
            continue;
        }
        string_count += 1;
        let lower = s.to_ascii_lowercase();
        if let Some(cmd) = s.strip_prefix("FSCommand:").or_else(|| s.strip_prefix("fscommand:")) {
            fscommands.insert(cmd.to_owned());
        } else if lower.starts_with("extern.") || lower == "extern" {
            externs.insert(s);
        } else if is_console_prompt_symbol(&s) {
            prompts.insert(s);
        }
    }

    Ok(AptSymbolReport {
        string_count,
        fscommands: fscommands.into_iter().collect(),
        externs: externs.into_iter().collect(),
        prompts: prompts.into_iter().collect(),
    })
}

fn is_console_prompt_symbol(s: &str) -> bool {
    const TOKENS: [&str; 12] = [
        "BTN_A",
        "BTN_B",
        "BTN_X",
        "BTN_Y",
        "BTN_LB",
        "BTN_RB",
        "BTN_LT",
        "BTN_RT",
        "DPAD",
        "LSTICK",
        "RSTICK",
        "XENON_BTN",
    ];
    let upper = s.to_ascii_uppercase();
    TOKENS.iter().any(|t| upper.contains(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bigf_roundtrip_and_bad_magic() {
        let entries = vec![
            ("test.apt".to_owned(), b"Apt Data:99\0".to_vec()),
            ("test.const".to_owned(), b"Apt constant file\x1A\0\0".to_vec()),
        ];
        let packed = write_bigf(&entries);
        assert!(packed.starts_with(b"BIGF"));
        let parsed = parse_bigf(&packed).ok();
        assert_eq!(parsed, Some(entries));
        assert!(parse_bigf(b"NOT_BIGF").is_err());
    }

    #[test]
    fn inspects_apt_fscommands_externs_and_prompts() {
        let mut c = Vec::new();
        c.extend_from_slice(b"Apt constant file\x1A\0\0");
        c.extend_from_slice(&0u32.to_be_bytes()); // movie_offset
        c.extend_from_slice(&3u32.to_be_bytes()); // count = 3
        c.extend_from_slice(&32u32.to_be_bytes()); // entries_offset = 32
        let str_base = 32 + 3 * 8;
        let s0 = b"FSCommand:StartCampaign\0";
        let s1 = b"extern.kitCount\0";
        let s2 = b"ID_HUD_BTN_A_ACCEPT\0";
        let offs = [str_base, str_base + s0.len(), str_base + s0.len() + s1.len()];
        for o in offs {
            c.extend_from_slice(&1u32.to_be_bytes());
            c.extend_from_slice(&(o as u32).to_be_bytes());
        }
        c.extend_from_slice(s0);
        c.extend_from_slice(s1);
        c.extend_from_slice(s2);

        let bigf = write_bigf(&[("menu.const".into(), c)]);
        let rep = inspect_apt_symbols(&bigf).ok();
        assert_eq!(
            rep,
            Some(AptSymbolReport {
                string_count: 3,
                fscommands: vec!["StartCampaign".into()],
                externs: vec!["extern.kitCount".into()],
                prompts: vec!["ID_HUD_BTN_A_ACCEPT".into()],
            })
        );
    }
}
