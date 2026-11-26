//! Pure-Rust reader, writer, and XML exporter for Frostbite 1 / 1.5 `{binary}` `.dbx` partitions.
//!
//! Supports all Frostbite binary DBX element kinds (`Container = 0xA`, `Text = 2`, `Numbers = 7`,
//! and 2-byte scalar/boolean values) with deterministic byte-exact round-trip serialization.
#![deny(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Binary DBX file magic (`"{binary}"`).
pub const MAGIC: &[u8; 8] = b"{binary}";
/// Element kind nibble for container nodes (`partition`, `instance`, `array`, `complex`).
pub const KIND_CONTAINER: u8 = 0x0A;
/// Element kind nibble for string-table text nodes.
pub const KIND_TEXT: u8 = 0x02;
/// Element kind nibble for packed big-endian numeric arrays (`count`, `size`, raw BE bytes).
pub const KIND_NUMBERS: u8 = 0x07;

/// Errors returned while parsing a Frostbite binary `.dbx` partition.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DbxError {
    /// Input is zero bytes (a Frostbite `*deleted*` bundle marker).
    #[error("empty DBX (deleted bundle entry)")]
    DeletedEntry,
    /// Input does not begin with `b"{binary}"`.
    #[error("not a binary DBX file")]
    BadMagic,
    /// Input ends before a required header, varint, string table, or payload finishes.
    #[error("truncated DBX: {0}")]
    Truncated(&'static str),
    /// Malformed structure (invalid string index, invalid number size, unclosed container).
    #[error("damaged DBX: {0}")]
    Damaged(String),
}

/// One element in a Frostbite binary `.dbx` tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    /// Element XML tag name (`partition`, `instance`, `array`, `field`, `item`, ...).
    pub tag: String,
    /// High nibble of the binary element header (`KIND_CONTAINER`, `KIND_TEXT`, `KIND_NUMBERS`, or scalar).
    pub kind: u8,
    /// Ordered `(name, value)` attributes.
    pub attributes: Vec<(String, String)>,
    /// Text payload when `kind == KIND_TEXT`.
    pub text: String,
    /// Raw payload bytes:
    /// - when `kind == KIND_NUMBERS`: `count` varint + `size` varint + `count * size` big-endian bytes
    /// - when scalar/boolean (`kind` other than container/text/numbers): 2 raw bytes
    pub raw: Vec<u8>,
    /// Direct children when `kind == KIND_CONTAINER`.
    pub children: Vec<Element>,
}

impl Element {
    /// Returns the first attribute matching `name`, if any.
    #[must_use]
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attributes.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// Returns the first direct child whose `name` attribute equals `name`.
    #[must_use]
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.attr("name") == Some(name))
    }

    /// Decodes a `KIND_NUMBERS` element into `(elem_size, slices)` of big-endian bytes.
    #[must_use]
    pub fn number_slices(&self) -> Option<(usize, Vec<&[u8]>)> {
        if self.kind != KIND_NUMBERS {
            return None;
        }
        let mut pos = 0usize;
        let count = read_varint(&self.raw, &mut pos).ok()?;
        let size = read_varint(&self.raw, &mut pos).ok()?;
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let start = pos.checked_add(i.checked_mul(size)?)?;
            out.push(self.raw.get(start..start.checked_add(size)?)?);
        }
        Some((size, out))
    }

    /// Decodes a 4-byte `KIND_NUMBERS` element as signed 32-bit big-endian integers.
    #[must_use]
    pub fn as_i32s(&self) -> Option<Vec<i32>> {
        let (size, vals) = self.number_slices()?;
        if size != 4 {
            return None;
        }
        let mut out = Vec::with_capacity(vals.len());
        for v in vals {
            let arr: [u8; 4] = v.try_into().ok()?;
            out.push(i32::from_be_bytes(arr));
        }
        Some(out)
    }

    /// Decodes a 4-byte `KIND_NUMBERS` element as 32-bit big-endian IEEE-754 floats.
    #[must_use]
    pub fn as_f32s(&self) -> Option<Vec<f32>> {
        let (size, vals) = self.number_slices()?;
        if size != 4 {
            return None;
        }
        let mut out = Vec::with_capacity(vals.len());
        for v in vals {
            let arr: [u8; 4] = v.try_into().ok()?;
            out.push(f32::from_be_bytes(arr));
        }
        Some(out)
    }

    /// Creates a `KIND_NUMBERS` field element holding 4-byte signed integers.
    #[must_use]
    pub fn new_i32_field(name: impl Into<String>, values: &[i32]) -> Self {
        let mut raw = Vec::with_capacity(2 + values.len() * 4);
        write_varint(&mut raw, values.len());
        write_varint(&mut raw, 4);
        for &v in values {
            raw.extend_from_slice(&v.to_be_bytes());
        }
        Self {
            tag: "field".to_owned(),
            kind: KIND_NUMBERS,
            attributes: vec![("name".to_owned(), name.into())],
            text: String::new(),
            raw,
            children: Vec::new(),
        }
    }

    /// Creates a `KIND_TEXT` field element.
    #[must_use]
    pub fn new_text_field(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            tag: "field".to_owned(),
            kind: KIND_TEXT,
            attributes: vec![("name".to_owned(), name.into())],
            text: value.into(),
            raw: Vec::new(),
            children: Vec::new(),
        }
    }
}

/// Parsed Frostbite binary `.dbx` partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbxFile {
    /// Root elements (normally a single `<partition>` container).
    pub roots: Vec<Element>,
}

impl DbxFile {
    /// Returns the root `<partition>` element, if present.
    #[must_use]
    pub fn partition(&self) -> Option<&Element> {
        self.roots.first()
    }

    /// Iterates over all `<instance>` children of the root partition.
    pub fn instances(&self) -> impl Iterator<Item = &Element> {
        self.roots
            .first()
            .into_iter()
            .flat_map(|r| r.children.iter())
            .filter(|e| e.tag == "instance")
    }

    /// Finds an `<instance>` by case-insensitive `guid` attribute.
    #[must_use]
    pub fn instance(&self, guid: &str) -> Option<&Element> {
        self.instances()
            .find(|e| e.attr("guid").is_some_and(|g| g.eq_ignore_ascii_case(guid)))
    }

    /// Serializes this DBX tree back into Frostbite `{binary}` format.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        // Build deterministic string table (index 0 is always "")
        let mut table: Vec<String> = vec![String::new()];
        let mut index: BTreeMap<String, usize> = BTreeMap::new();
        index.insert(String::new(), 0);

        let mut intern = |s: &str| -> usize {
            if let Some(&id) = index.get(s) {
                return id;
            }
            let id = table.len();
            table.push(s.to_owned());
            index.insert(s.to_owned(), id);
            id
        };

        for r in &self.roots {
            collect_strings(r, &mut intern);
        }

        // Encode body stream
        let mut body = Vec::new();
        for r in &self.roots {
            write_elem(r, &index, &mut body);
        }

        // Encode string table
        let mut str_bytes = Vec::new();
        let mut offsets = Vec::with_capacity(table.len());
        for s in &table {
            offsets.push(str_bytes.len() as u32);
            str_bytes.extend_from_slice(s.as_bytes());
            str_bytes.push(0);
        }
        let area = (offsets.len() * 4 + str_bytes.len()) as u32;
        let stream_off = 24u32 + area;

        let mut out = Vec::with_capacity(stream_off as usize + body.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&stream_off.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&area.to_be_bytes());
        out.extend_from_slice(&(table.len() as u32).to_be_bytes());
        for off in offsets {
            out.extend_from_slice(&off.to_be_bytes());
        }
        out.extend_from_slice(&str_bytes);
        out.extend_from_slice(&body);
        out
    }

    /// Exports this DBX tree as human-readable XML.
    #[must_use]
    pub fn to_xml(&self) -> String {
        fn escape(s: &str) -> String {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
        }
        fn emit(e: &Element, depth: usize, out: &mut String) {
            for _ in 0..depth {
                out.push('\t');
            }
            let _ = write!(out, "<{}", e.tag);
            for (k, v) in &e.attributes {
                let _ = write!(out, " {k}=\"{}\"", escape(v));
            }
            if e.kind == KIND_CONTAINER {
                if e.children.is_empty() {
                    out.push_str(" />\r\n");
                } else {
                    out.push_str(">\r\n");
                    for c in &e.children {
                        emit(c, depth + 1, out);
                    }
                    for _ in 0..depth {
                        out.push('\t');
                    }
                    let _ = writeln!(out, "</{}>\r", e.tag);
                }
            } else {
                let val = if e.kind == KIND_TEXT {
                    escape(&e.text)
                } else if let Some(ints) = e.as_i32s() {
                    ints.iter().map(i32::to_string).collect::<Vec<_>>().join("/")
                } else {
                    match e.raw.get(1).copied().unwrap_or(0) {
                        0 => "false".to_owned(),
                        1 => "true".to_owned(),
                        v => v.to_string(),
                    }
                };
                let _ = writeln!(out, ">{val}</{}>\r", e.tag);
            }
        }
        let mut out = String::from("<?xml version=\"1.0\"?>\r\n");
        for r in &self.roots {
            emit(r, 0, &mut out);
        }
        out
    }
}

fn be32(b: &[u8], off: usize) -> Result<u32, DbxError> {
    let s = b.get(off..off + 4).ok_or(DbxError::Truncated("u32"))?;
    let arr: [u8; 4] = s.try_into().map_err(|_| DbxError::Truncated("u32"))?;
    Ok(u32::from_be_bytes(arr))
}

fn read_varint(b: &[u8], pos: &mut usize) -> Result<usize, DbxError> {
    let mut val = 0usize;
    let mut shift = 0u32;
    loop {
        let byte = *b.get(*pos).ok_or(DbxError::Truncated("varint"))?;
        *pos += 1;
        if shift > 63 {
            return Err(DbxError::Damaged("varint wider than 64 bits".into()));
        }
        val |= usize::from(byte & 0x7F) << shift;
        if byte < 0x80 {
            return Ok(val);
        }
        shift += 7;
    }
}

fn write_varint(out: &mut Vec<u8>, mut val: usize) {
    loop {
        let mut b = (val & 0x7F) as u8;
        val >>= 7;
        if val != 0 {
            b |= 0x80;
        }
        out.push(b);
        if val == 0 {
            break;
        }
    }
}

fn get_str(strings: &[String], idx: usize) -> Result<String, DbxError> {
    strings
        .get(idx)
        .cloned()
        .ok_or_else(|| DbxError::Damaged(format!("string index {idx} out of range ({})", strings.len())))
}

/// Returns `true` if `data` begins with the Frostbite `{binary}` DBX magic.
#[must_use]
pub fn is_binary(data: &[u8]) -> bool {
    data.len() > 24 && data.get(0..8) == Some(MAGIC.as_slice())
}

fn collect_strings(e: &Element, intern: &mut impl FnMut(&str) -> usize) {
    intern(&e.tag);
    for (k, v) in &e.attributes {
        intern(k);
        intern(v);
    }
    if e.kind == KIND_TEXT {
        intern(&e.text);
    }
    for c in &e.children {
        collect_strings(c, intern);
    }
}

fn write_elem(e: &Element, index: &BTreeMap<String, usize>, out: &mut Vec<u8>) {
    let tag_id = index.get(&e.tag).copied().unwrap_or(0);
    write_varint(out, tag_id);
    let n_attrs = e.attributes.len().min(15) as u8;
    out.push((e.kind << 4) | n_attrs);
    for (k, v) in e.attributes.iter().take(15) {
        write_varint(out, index.get(k).copied().unwrap_or(0));
        write_varint(out, index.get(v).copied().unwrap_or(0));
    }
    if e.kind == KIND_CONTAINER {
        out.push(0);
        for c in &e.children {
            write_elem(c, index, out);
        }
        out.push(0);
    } else if e.kind == KIND_TEXT {
        write_varint(out, index.get(&e.text).copied().unwrap_or(0));
    } else if e.kind == KIND_NUMBERS {
        out.extend_from_slice(&e.raw);
    } else if e.raw.len() >= 2 {
        out.extend_from_slice(e.raw.get(0..2).unwrap_or(&[0, 0]));
    } else {
        out.extend_from_slice(&[0, 0]);
    }
}

/// Parses a Frostbite 1 / 1.5 `{binary}` `.dbx` partition into a [`DbxFile`] tree.
///
/// # Errors
/// Returns [`DbxError`] if the buffer is empty, lacks the `{binary}` magic, is truncated, or
/// contains malformed string indices or unclosed containers.
#[allow(clippy::too_many_lines)]
pub fn parse(data: &[u8]) -> Result<DbxFile, DbxError> {
    if data.is_empty() {
        return Err(DbxError::DeletedEntry);
    }
    if !is_binary(data) {
        return Err(DbxError::BadMagic);
    }
    let area = be32(data, 16)? as usize;
    let count = be32(data, 20)? as usize;
    let stop = 24usize.checked_add(area).ok_or(DbxError::Truncated("string area"))?;
    if stop > data.len() {
        return Err(DbxError::Truncated("string area extends past EOF"));
    }
    let base = 24usize
        .checked_add(count.checked_mul(4).ok_or(DbxError::Truncated("string offset table"))?)
        .ok_or(DbxError::Truncated("string offset table"))?;
    if base > stop {
        return Err(DbxError::Damaged("string offset table exceeds string area".into()));
    }

    let mut strings = Vec::with_capacity(count);
    for i in 0..count {
        let rel = be32(data, 24 + i * 4)? as usize;
        let start = base.checked_add(rel).ok_or(DbxError::Truncated("string offset"))?;
        let slice = data.get(start..stop).ok_or(DbxError::Truncated("string"))?;
        let end = slice
            .iter()
            .position(|&c| c == 0)
            .ok_or_else(|| DbxError::Damaged(format!("unterminated string {i}")))?;
        strings.push(String::from_utf8_lossy(slice.get(..end).unwrap_or_default()).into_owned());
    }

    let mut roots: Vec<Element> = Vec::new();
    let mut stack: Vec<Element> = Vec::new();
    let mut pos = stop;

    while pos < data.len() {
        let tag = read_varint(data, &mut pos)?;
        if tag == 0 {
            if let Some(done) = stack.pop() {
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(done);
                } else {
                    roots.push(done);
                }
            } else {
                break;
            }
            continue;
        }
        let head = *data.get(pos).ok_or(DbxError::Truncated("element header"))?;
        pos += 1;
        let kind = head >> 4;
        let n_attrs = usize::from(head & 0x0F);
        let mut attributes = Vec::with_capacity(n_attrs);
        for _ in 0..n_attrs {
            let k = read_varint(data, &mut pos)?;
            let v = read_varint(data, &mut pos)?;
            attributes.push((get_str(&strings, k)?, get_str(&strings, v)?));
        }

        let mut elem = Element {
            tag: get_str(&strings, tag)?,
            kind,
            attributes,
            text: String::new(),
            raw: Vec::new(),
            children: Vec::new(),
        };

        if kind == KIND_CONTAINER {
            let _ = data.get(pos).ok_or(DbxError::Truncated("container byte"))?;
            pos += 1;
            stack.push(elem);
            continue;
        }
        if kind == KIND_TEXT {
            let idx = read_varint(data, &mut pos)?;
            elem.text = get_str(&strings, idx)?;
        } else if kind == KIND_NUMBERS {
            let start = pos;
            let n_vals = read_varint(data, &mut pos)?;
            let sz = read_varint(data, &mut pos)?;
            if n_vals > 0 && sz < 2 {
                return Err(DbxError::Damaged(format!("invalid number width {sz}")));
            }
            let payload = n_vals.checked_mul(sz).ok_or(DbxError::Truncated("numbers payload"))?;
            let end = pos.checked_add(payload).ok_or(DbxError::Truncated("numbers payload"))?;
            let raw = data.get(start..end).ok_or(DbxError::Truncated("numbers payload"))?;
            elem.raw.extend_from_slice(raw);
            pos = end;
        } else {
            let raw = data.get(pos..pos + 2).ok_or(DbxError::Truncated("scalar value"))?;
            elem.raw.extend_from_slice(raw);
            pos += 2;
        }

        if let Some(parent) = stack.last_mut() {
            parent.children.push(elem);
        } else {
            roots.push(elem);
        }
    }

    if roots.is_empty() {
        return Err(DbxError::Damaged("no root element after string table".into()));
    }
    if roots.first().is_some_and(|r| r.kind != KIND_CONTAINER) {
        return Err(DbxError::Damaged("root element is not a container".into()));
    }
    if !stack.is_empty() {
        return Err(DbxError::Truncated("unclosed container at EOF"));
    }
    Ok(DbxFile { roots })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_dbx_and_xml() {
        let inst = Element {
            tag: "instance".to_owned(),
            kind: KIND_CONTAINER,
            attributes: vec![
                ("guid".to_owned(), "22ECAAE2-5588-11DE-AD94-C76F993A7E1F".to_owned()),
                ("type".to_owned(), "UI.UIFontGlyph".to_owned()),
            ],
            text: String::new(),
            raw: Vec::new(),
            children: vec![
                Element::new_i32_field("Glyph", &[65]),
                Element::new_i32_field("Advance", &[18]),
                Element::new_text_field("Name", "Eurostile"),
            ],
        };
        let part = Element {
            tag: "partition".to_owned(),
            kind: KIND_CONTAINER,
            attributes: vec![("guid".to_owned(), "22ECAAE1-5588-11DE-AD94-C76F993A7E1F".to_owned())],
            text: String::new(),
            raw: Vec::new(),
            children: vec![inst],
        };
        let file = DbxFile { roots: vec![part] };
        let bytes = file.to_bytes();
        assert!(is_binary(&bytes));

        let parsed = parse(&bytes).ok();
        assert_eq!(parsed, Some(file));
        if let Some(p) = parsed {
            let i = p.instance("22ecaae2-5588-11de-ad94-c76f993a7e1f");
            assert!(i.is_some());
            assert_eq!(
                i.and_then(|e| e.child("Glyph")).and_then(Element::as_i32s),
                Some(vec![65])
            );
            assert_eq!(
                i.and_then(|e| e.child("Name")).map(|e| e.text.as_str()),
                Some("Eurostile")
            );
            let xml = p.to_xml();
            assert!(xml.contains("UI.UIFontGlyph"));
            assert!(xml.contains(">Eurostile<"));
        }
    }

    #[test]
    fn refuses_empty_and_bad_magic() {
        assert_eq!(parse(&[]), Err(DbxError::DeletedEntry));
        assert_eq!(parse(b"not a dbx file at all!!"), Err(DbxError::BadMagic));
    }
}
