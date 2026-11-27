//! Havok 4.6.1 -> 5.5.0 structural porting and packfile header inspection/upgrading.
#![deny(unsafe_code)]

use std::collections::BTreeMap;

use super::Bfbc1Error;
// ---------------------------------------------------------------------------
// 4. Havok Packfile Version Upgrader (Havok 4.6.1 -> 5.5.0)
// ---------------------------------------------------------------------------

/// Context required to structurally port Xbox 360 `Havok-4.6.1-r1` resources (BFBC1 / BF1943)
/// into PC `Havok-5.5.0-r1` (BFBC2).
pub struct HavokPortContext<'a> {
    /// Multi-shape PC Havok 5.5.0 template packfile (providing target class names & layouts).
    pub template_550: &'a [u8],
    /// Class layouts extracted from the target PC executable (`x360_havok::port::Layouts`).
    pub layouts: &'a x360_havok::port::Layouts,
    /// `hkClass` reflection table for [`x360_havok::convert::convert_packfile`].
    pub classes: &'a x360_havok::convert::Classes,
    /// Optional `hkUFloat8` lookup table from `BFBC2Game.exe` (`x360_havok::port::ufloat8_table`).
    pub ufloat8_table: Option<&'a [f64]>,
}

/// Owned Havok 5.5.0 reflection tables extracted from a PC Frostbite 1.5 executable (`BFBC2Game.exe`).
pub struct HavokExeData {
    /// Flattened `hkClass` layouts for [`x360_havok::convert::convert_packfile`].
    pub classes: x360_havok::convert::Classes,
    /// Class names and `aabbCenter` offsets for [`x360_havok::port::restructure`].
    pub layouts: x360_havok::port::Layouts,
    /// Optional `hkUFloat8` decoding table extracted from the executable.
    pub ufloat8_table: Option<Vec<f64>>,
}

impl HavokExeData {
    /// Borrows a [`HavokPortContext`] pairing these executable tables with a PC 5.5.0 template.
    #[must_use]
    pub fn as_context<'a>(&'a self, template_550: &'a [u8]) -> HavokPortContext<'a> {
        HavokPortContext {
            template_550,
            layouts: &self.layouts,
            classes: &self.classes,
            ufloat8_table: self.ufloat8_table.as_deref(),
        }
    }
}

/// Extracts Havok 5.5.0 `hkClass` reflection tables, `aabbCenter` offsets, and the `hkUFloat8`
/// table from a PC Frostbite 1.5 executable (`BFBC2Game.exe`).
///
/// # Errors
/// Returns [`Bfbc1Error::Havok`] if no `hkClass` initializers are found in `exe_bytes`.
pub fn load_havok_port_data(exe_bytes: &[u8]) -> Result<HavokExeData, Bfbc1Error> {
    let raw_klasses = crate::hkclasses::classes(exe_bytes)
        .ok_or_else(|| Bfbc1Error::Havok("failed to extract hkClass layouts from executable".into()))?;

    let mut by_name: BTreeMap<String, usize> = BTreeMap::new();
    let mut klasses = Vec::with_capacity(raw_klasses.len());
    let mut names = std::collections::HashSet::with_capacity(raw_klasses.len());

    for (idx, (name, parent, size, raw_members)) in raw_klasses.into_iter().enumerate() {
        names.insert(name.clone());
        by_name.insert(name.clone(), idx);
        let members = raw_members
            .into_iter()
            .map(
                |(m_name, cls, ty, subtype, carray, _flags, offset)| x360_havok::convert::Member {
                    name: m_name,
                    cls,
                    ty,
                    subtype,
                    carray,
                    offset: u32::from(offset),
                },
            )
            .collect();
        klasses.push(x360_havok::convert::Klass {
            name,
            parent,
            size,
            members,
        });
    }

    let mut aabb_center = std::collections::HashMap::new();
    for k in &klasses {
        let mut found = None;
        let mut cur = Some(k);
        let mut hops = 0usize;
        while let Some(c) = cur
            && hops < 32
        {
            if let Some(m) = c.members.iter().find(|m| m.name.as_deref() == Some("aabbCenter")) {
                found = Some(m.offset);
                break;
            }
            cur = c
                .parent
                .as_ref()
                .and_then(|p| by_name.get(p).and_then(|&i| klasses.get(i)));
            hops += 1;
        }
        aabb_center.insert(k.name.clone(), found);
    }

    let classes = x360_havok::convert::Classes::new(&klasses);
    let layouts = x360_havok::port::Layouts { names, aabb_center };
    let ufloat8_table = x360_havok::port::ufloat8_table(exe_bytes);

    Ok(HavokExeData {
        classes,
        layouts,
        ufloat8_table,
    })
}

/// Structurally ports an Xbox 360 `Havok-4.6.1-r1` physics resource (BFBC1 / BF1943) into a
/// PC `Havok-5.5.0-r1` resource via:
/// 1. [`x360_havok::port::restructure`] (`4.6.1 BE -> 5.5.0 BE`),
/// 2. [`x360_havok::convert::convert_packfile`] (`5.5.0 BE -> 5.5.0 LE`),
/// 3. [`x360_havok::port::finish`] (wrapper material run-table endian swap + shape subpart w-lanes).
///
/// # Errors
/// Returns [`Bfbc1Error::Havok`] if any stage refuses or fails on the input packfile.
pub fn port_havok_461_to_550_pc(data: &[u8], ctx: &HavokPortContext<'_>) -> Result<Vec<u8>, Bfbc1Error> {
    let be_550 = x360_havok::port::restructure(data, ctx.template_550, ctx.layouts, ctx.ufloat8_table)
        .map_err(|e| Bfbc1Error::Havok(format!("restructure 4.6.1->5.5.0: {e:?}")))?;
    let magic_off = x360_havok::port::find_magic(&be_550)
        .ok_or_else(|| Bfbc1Error::Havok("restructured packfile missing Havok magic".into()))?;
    let le_pack = x360_havok::convert::convert_packfile(&be_550, magic_off, ctx.classes)
        .map_err(|e| Bfbc1Error::Havok(format!("convert_packfile BE->LE: {e}")))?;
    let le_550 = if magic_off > 0 {
        let mut le_wrap = x360_havok::convert::convert_wrapper(&be_550, magic_off)
            .map_err(|e| Bfbc1Error::Havok(format!("convert_wrapper BE->LE: {e:?}")))?;
        let pack_slice = le_pack.get(magic_off..).unwrap_or(&le_pack);
        le_wrap.extend_from_slice(pack_slice);
        le_wrap
    } else {
        le_pack
    };
    x360_havok::port::finish(&le_550).map_err(|e| Bfbc1Error::Havok(format!("finish 5.5.0: {e:?}")))
}

/// Parsed summary of a 64-byte Havok `hkPackfileHeader`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HavokHeaderInfo {
    /// File version word at offset `0x0C` (e.g. `4`, `5`, `7`).
    pub file_version: u32,
    /// Pointer size in bytes (`4` on Xbox 360 / 32-bit PC).
    pub pointer_size: u8,
    /// `true` for little-endian (PC), `false` for big-endian (Xbox 360).
    pub little_endian: bool,
    /// Number of packfile sections (`0x14`).
    pub section_count: u32,
    /// Null-trimmed SDK version banner (`0x28..0x38`, e.g. `"Havok-4.6.1-r1"`).
    pub sdk_version: String,
}

/// Summary report produced by [`upgrade_havok_packfile`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HavokUpgradeReport {
    /// Original SDK version banner.
    pub source_sdk: String,
    /// Output SDK version banner.
    pub target_sdk: String,
    /// Whether Xbox 360 -> PC endianness conversion was performed.
    pub endian_converted: bool,
    /// Whether the SDK banner was patched.
    pub sdk_patched: bool,
}

/// Inspects a Havok `hkPackfile` header (Xbox 360 big-endian or PC little-endian).
///
/// # Errors
/// Returns [`Bfbc1Error::Havok`] if the buffer is shorter than 64 bytes or lacks Havok magic.
pub fn inspect_havok_header(data: &[u8]) -> Result<HavokHeaderInfo, Bfbc1Error> {
    if data.len() < 0x40 || data.get(0..8) != Some(x360_havok::convert::MAGIC.as_slice()) {
        return Err(Bfbc1Error::Havok("not a valid Havok hkPackfile".into()));
    }
    let pointer_size = data.get(0x10).copied().unwrap_or(0);
    let little_endian = data.get(0x11).copied().unwrap_or(0) != 0;
    let read_u32 = |off: usize| -> Result<u32, Bfbc1Error> {
        let arr: [u8; 4] = data
            .get(off..off + 4)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(|| Bfbc1Error::Havok("truncated hkPackfile header".into()))?;
        Ok(if little_endian {
            u32::from_le_bytes(arr)
        } else {
            u32::from_be_bytes(arr)
        })
    };
    let file_version = read_u32(0x0C)?;
    let section_count = read_u32(0x14)?;
    let sdk_raw = data
        .get(0x28..0x38)
        .ok_or_else(|| Bfbc1Error::Havok("truncated SDK banner".into()))?;
    let end = sdk_raw.iter().position(|&b| b == 0).unwrap_or(sdk_raw.len());
    let sdk_version = String::from_utf8_lossy(sdk_raw.get(..end).unwrap_or_default()).into_owned();

    Ok(HavokHeaderInfo {
        file_version,
        pointer_size,
        little_endian,
        section_count,
        sdk_version,
    })
}

/// Upgrades a Havok packfile toward Frostbite 1.5's `Havok-5.5.0-r1` runtime:
/// - Converts Xbox 360 big-endian -> PC little-endian when `classes` is supplied and the input is BE.
/// - Patches the 16-byte SDK banner at `0x28..0x38` to `target_sdk` (typically [`super::FB15_HAVOK_SDK`]).
///
/// Note: for structural `Havok-4.6.1-r1 -> Havok-5.5.0-r1` conversion (BFBC1/BF1943 physics),
/// prefer [`port_havok_461_to_550_pc`] with a [`HavokPortContext`].
///
/// # Errors
/// Returns [`Bfbc1Error::Havok`] if the header is invalid, `target_sdk` exceeds 15 bytes, or
/// endianness conversion fails.
pub fn upgrade_havok_packfile(
    data: &[u8],
    target_sdk: &str,
    classes: Option<&x360_havok::convert::Classes>,
) -> Result<(Vec<u8>, HavokUpgradeReport), Bfbc1Error> {
    if target_sdk.len() > 15 {
        return Err(Bfbc1Error::Havok(format!(
            "target SDK banner `{target_sdk}` exceeds 15 bytes"
        )));
    }
    let info = inspect_havok_header(data)?;

    let (mut out, endian_converted) = if info.little_endian {
        (data.to_vec(), false)
    } else if let Some(cls) = classes {
        let pc = x360_havok::convert::convert_packfile(data, 0, cls).map_err(|e| Bfbc1Error::Havok(e.to_string()))?;
        (pc, true)
    } else {
        (data.to_vec(), false)
    };

    let mut sdk_patched = false;
    if info.sdk_version != target_sdk
        && let Some(slot) = out.get_mut(0x28..0x38)
    {
        slot.fill(0);
        let bytes = target_sdk.as_bytes();
        if let Some(dst) = slot.get_mut(..bytes.len()) {
            dst.copy_from_slice(bytes);
            sdk_patched = true;
        }
    }

    Ok((
        out,
        HavokUpgradeReport {
            source_sdk: info.sdk_version,
            target_sdk: target_sdk.to_owned(),
            endian_converted,
            sdk_patched,
        },
    ))
}
