//! Battlefield: Bad Company (BFBC1 / Frostbite 1.0) -> Frostbite 1.5 (BF1943 / BFBC2 PC)
//! asset and schema upgraders.
//!
//! Provides pure-Rust upgraders for the four Frostbite 1.0 -> 1.5 format boundaries:
//! 1. **DBX schema upgrader** ([`upgrade_dbx`], [`upgrade_dbx_bytes`], [`DbxSchemaRules`]):
//!    drops deprecated instance classes (`Audio.ValueRef`, `LevelReferencedAssetsData`),
//!    renames legacy instance/complex types (`RegroupOrderEntityData` -> `FollowOrderEntityData`),
//!    wraps `Mesh1p`/`Mesh3p` into `Meshes1p`/`Meshes3p` arrays, normalizes `EIA*` -> `Eia*`
//!    input-action enums, remaps `ske01` `BoneId` indices, drops/renames fields, and
//!    normalizes `.xenonTexture` resource references.
//! 2. **Terrain heightfield upgrader** ([`upgrade_heightfield`], [`downsample_sub_heightfield_256_to_128`]):
//!    supports Frostbite 1.0's `0x29` and `0x2D` headers as well as `256x256` and `512x512` tiles,
//!    expanding them into Frostbite 1.5's 49-byte (`0x31`) little-endian PC heightfield layout.
//! 3. **Mesh vertex layout & header upgrader** ([`compute_pc_vertex_layout`], [`build_pc_layout_body`],
//!    [`upgrade_mesh_header`]): maps console packed vertex formats (`DEC3N` `0x2B`, `UBYTE4N` `0x0D`)
//!    to PC `float16x4` (`0x08`) layouts and upgrades `MeshData` headers to little-endian PC format.
//! 4. **Havok packfile version upgrader** ([`inspect_havok_header`], [`port_havok_461_to_550_pc`],
//!    [`upgrade_havok_packfile`]): inspects `hkPackfile` endianness and SDK banner, routes
//!    `Havok-4.6.1-r1` packfiles through [`x360_havok::port`] -> [`x360_havok::convert`] ->
//!    [`x360_havok::port::finish`], and patches SDK banners.
#![deny(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use crate::dbx::{self, DbxError, DbxFile, Element};
use crate::fbrb::FbrbArchive;
use crate::mesh::{self, Pair};
use crate::{meshconv, meshdata, terrain};

/// Frostbite 1.0 compact heightfield header length (41 bytes).
pub const FB10_HEIGHTFIELD_HEADER: usize = 0x29;
/// Frostbite 1.0 / BF1943 heightfield header length (45 bytes).
pub const FB10_EXT_HEIGHTFIELD_HEADER: usize = 0x2D;
/// Frostbite 1.5 PC heightfield header length (49 bytes).
pub const FB15_HEIGHTFIELD_HEADER: usize = 0x31;
/// Target Havok SDK banner used by Frostbite 1.5 (BFBC2 PC / BF1943).
pub const FB15_HAVOK_SDK: &str = "Havok-5.5.0-r1";

/// Errors returned by BFBC1 (Frostbite 1.0 -> 1.5) upgraders.
#[derive(Debug, Error)]
pub enum Bfbc1Error {
    /// Underlying DBX parse/serialize error.
    #[error(transparent)]
    Dbx(#[from] DbxError),
    /// Terrain heightfield conversion failed.
    #[error("BFBC1 heightfield upgrade failed: {0}")]
    Terrain(String),
    /// Mesh header or vertex layout upgrade failed.
    #[error("BFBC1 mesh upgrade failed: {0}")]
    Mesh(String),
    /// Havok packfile inspection or conversion failed.
    #[error("BFBC1 Havok upgrade failed: {0}")]
    Havok(String),
    /// Bink movie unwrapping failed (no `BIKi`/`KB2i` stream found).
    #[error("BFBC1 Bink movie unwrap failed: {0}")]
    Bink(String),
    /// Kynogon HPD navmesh upgrade failed.
    #[error("BFBC1 Kynogon HPD upgrade failed: {0}")]
    Hpd(String),
}

// ---------------------------------------------------------------------------
// 1. DBX Schema Upgrader
// ---------------------------------------------------------------------------

/// Default field to inject into matching DBX instances during a Frostbite 1.0 -> 1.5 upgrade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultField {
    /// Instance `type` attribute to match (e.g. `"StaticModelEntityData"`).
    pub instance_type: String,
    /// Element to insert if no direct child with the same `name` attribute exists.
    pub element: Element,
}

/// Configurable Frostbite 1.0 -> 1.5 DBX schema migration rules.
#[derive(Debug, Clone, Default)]
pub struct DbxSchemaRules {
    /// Set of `<instance type="...">` class names to remove entirely (e.g. `Audio.ValueRef`,
    /// `GameSharedResources.LevelReferencedAssetsData`).
    pub dropped_instance_types: BTreeSet<String>,
    /// Map of `old_type -> new_type` applied to `instance` and `complex` `type` attributes.
    pub type_renames: BTreeMap<String, String>,
    /// Map of `(instance_type, old_field_name) -> new_field_name`. Use `"*"` for global renames.
    pub field_renames: BTreeMap<(String, String), String>,
    /// Map of single-value field name -> Frostbite 1.5 `<array name="...">` wrapper name
    /// (e.g. `"Mesh1p" -> "Meshes1p"`, `"Mesh3p" -> "Meshes3p"`).
    pub wrap_as_array: BTreeMap<String, String>,
    /// Set of `(instance_type, field_name)` pairs to drop. Use `"*"` for global drops.
    pub dropped_fields: BTreeSet<(String, String)>,
    /// Default fields to inject when absent on matching instances.
    pub default_fields: Vec<DefaultField>,
    /// Optional BFBC1 -> BFBC2 `ske01` bone-index map applied to `BoneId` integer fields.
    pub bone_id_map: BTreeMap<i32, i32>,
    /// Rewrite BFBC1 `EIA*` input-action enum text values to BFBC2 `Eia*` casing.
    pub rename_eia_enums: bool,
    /// Rewrite console texture/resource suffixes (e.g. `.xenonTexture` -> `.itexture`).
    pub normalize_console_refs: bool,
}

impl DbxSchemaRules {
    /// Returns the empirical BFBC1 (Frostbite 1.0) -> BFBC2 (Frostbite 1.5) schema migration ruleset.
    #[must_use]
    pub fn bfbc1_to_fb15() -> Self {
        let mut dropped_instance_types = BTreeSet::new();
        // Absent from BFBC2Game.exe TypeInfo (verified across 12,382 BFBC1 DBX partitions):
        dropped_instance_types.insert("Audio.ValueRef".into());
        dropped_instance_types.insert("GameSharedResources.LevelReferencedAssetsData".into());
        // Value-type struct in Frostbite 1.5 (serialized as <instance> DataContainer in BFBC1 -> crashes 0x0049EA77):
        dropped_instance_types.insert("GameSharedResources.RichPresenceContextPair".into());

        let mut type_renames = BTreeMap::new();
        type_renames.insert(
            "GameSharedResources.RegroupOrderEntityData".into(),
            "GameSharedResources.FollowOrderEntityData".into(),
        );
        type_renames.insert("XenonTextureAsset".into(), "TextureAsset".into());
        type_renames.insert("XenonTexture".into(), "ITexture".into());
        type_renames.insert("StaticEnlightenEntityData".into(), "EnlightenEntityData".into());

        let mut field_renames = BTreeMap::new();
        field_renames.insert(
            ("TerrainData".into(), "HeightfieldResource".into()),
            "TerrainStreamingTreeResource".into(),
        );
        field_renames.insert(("*".into(), "XenonResource".into()), "Resource".into());

        let mut wrap_as_array = BTreeMap::new();
        wrap_as_array.insert("Mesh1p".into(), "Meshes1p".into());
        wrap_as_array.insert("Mesh3p".into(), "Meshes3p".into());

        let mut dropped_fields = BTreeSet::new();
        dropped_fields.insert(("*".into(), "XenonOnly".into()));
        dropped_fields.insert(("*".into(), "Ps3Only".into()));

        Self {
            dropped_instance_types,
            type_renames,
            field_renames,
            wrap_as_array,
            dropped_fields,
            default_fields: Vec::new(),
            bone_id_map: BTreeMap::new(),
            rename_eia_enums: true,
            normalize_console_refs: true,
        }
    }
}

/// Summary report produced by [`upgrade_dbx`] and [`upgrade_dbx_bytes`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DbxUpgradeReport {
    /// Total `<instance>` nodes inspected.
    pub instances_visited: usize,
    /// Number of deprecated `<instance>` nodes removed.
    pub instances_dropped: usize,
    /// Number of `type` attributes renamed.
    pub types_renamed: usize,
    /// Number of field `name` attributes renamed.
    pub fields_renamed: usize,
    /// Number of single fields wrapped into Frostbite 1.5 `<array>` nodes.
    pub arrays_wrapped: usize,
    /// Number of deprecated fields dropped.
    pub fields_dropped: usize,
    /// Number of missing Frostbite 1.5 default fields injected.
    pub fields_injected: usize,
    /// Number of `EIA*` -> `Eia*` enum values normalized.
    pub enums_renamed: usize,
    /// Number of `BoneId` integer values remapped.
    pub bones_remapped: usize,
    /// Number of dangling `ref` attributes cleared after dropping deprecated instances.
    pub dangling_refs_cleared: usize,
    /// Number of `Audio.ValueRef` scalar defaults preserved when lowering references.
    pub audio_value_refs_lowered: usize,
    /// Number of console resource references normalized.
    pub refs_updated: usize,
}

impl DbxUpgradeReport {
    /// Returns `true` if any schema mutation occurred.
    #[must_use]
    pub const fn changed(&self) -> bool {
        self.instances_dropped > 0
            || self.types_renamed > 0
            || self.fields_renamed > 0
            || self.arrays_wrapped > 0
            || self.fields_dropped > 0
            || self.fields_injected > 0
            || self.enums_renamed > 0
            || self.bones_remapped > 0
            || self.dangling_refs_cleared > 0
            || self.audio_value_refs_lowered > 0
            || self.refs_updated > 0
    }
}

/// Upgrades a parsed Frostbite 1.0 [`DbxFile`] in place according to `rules`.
pub fn upgrade_dbx(file: &mut DbxFile, rules: &DbxSchemaRules) -> DbxUpgradeReport {
    let mut report = DbxUpgradeReport::default();
    let mut dropped_guids = BTreeSet::new();
    let mut lowered_audio_values = BTreeMap::new();
    for root in &mut file.roots {
        upgrade_element(
            root,
            None,
            rules,
            &mut dropped_guids,
            &mut lowered_audio_values,
            &mut report,
        );
    }
    if !dropped_guids.is_empty() {
        for root in &mut file.roots {
            clear_dangling_refs(root, &dropped_guids, &lowered_audio_values, &mut report);
        }
    }
    report
}

/// Parses a Frostbite 1.0 `{binary}` `.dbx` buffer, applies `rules`, and serializes the
/// upgraded `{binary}` `.dbx` partition.
///
/// # Errors
/// Returns [`Bfbc1Error::Dbx`] if parsing fails.
pub fn upgrade_dbx_bytes(data: &[u8], rules: &DbxSchemaRules) -> Result<(Vec<u8>, DbxUpgradeReport), Bfbc1Error> {
    let mut file = dbx::parse(data)?;
    let report = upgrade_dbx(&mut file, rules);
    let out = if report.changed() {
        file.to_bytes()
    } else {
        data.to_vec()
    };
    Ok((out, report))
}

#[allow(clippy::too_many_lines)]
fn upgrade_element(
    elem: &mut Element,
    parent_type: Option<&str>,
    rules: &DbxSchemaRules,
    dropped_guids: &mut BTreeSet<String>,
    lowered_audio_values: &mut BTreeMap<String, String>,
    report: &mut DbxUpgradeReport,
) {
    let is_instance = elem.tag == "instance";
    if is_instance {
        report.instances_visited += 1;
    }

    // 1. Rename instance/complex `type` attributes.
    if is_instance || elem.tag == "complex" {
        for (k, v) in &mut elem.attributes {
            if k == "type"
                && let Some(new_ty) = rules.type_renames.get(v.as_str())
            {
                new_ty.clone_into(v);
                report.types_renamed += 1;
            }
        }
    }

    let current_type = elem
        .attr("type")
        .map(ToOwned::to_owned)
        .or_else(|| parent_type.map(ToOwned::to_owned));

    // 2. Rename field `name` attributes.
    if let Some(name_attr) = elem.attributes.iter_mut().find(|(k, _)| k == "name") {
        let renamed = current_type
            .as_deref()
            .and_then(|ty| rules.field_renames.get(&(ty.to_owned(), name_attr.1.clone())))
            .or_else(|| rules.field_renames.get(&("*".to_owned(), name_attr.1.clone())));
        if let Some(new_name) = renamed {
            new_name.clone_into(&mut name_attr.1);
            report.fields_renamed += 1;
        }
    }

    // 3. Normalize console resource references and EIA* -> Eia* enum names.
    if rules.normalize_console_refs {
        if elem.kind == dbx::KIND_TEXT && normalize_ref_string(&mut elem.text) {
            report.refs_updated += 1;
        }
        for (k, v) in &mut elem.attributes {
            if (k == "ref" || k == "partition") && normalize_ref_string(v) {
                report.refs_updated += 1;
            }
        }
    }
    if rules.rename_eia_enums
        && elem.kind == dbx::KIND_TEXT
        && let Some(rest) = elem.text.strip_prefix("EIA")
        && !rest.is_empty()
    {
        elem.text = format!("Eia{rest}");
        report.enums_renamed += 1;
    }

    // 4. Remap `BoneId` integer fields if a ske01 bone-index table is supplied.
    if !rules.bone_id_map.is_empty()
        && elem.attr("name") == Some("BoneId")
        && let Some(vals) = elem.as_i32s()
    {
        let mut changed = false;
        let remapped: Vec<i32> = vals
            .into_iter()
            .map(|id| {
                if let Some(&new_id) = rules.bone_id_map.get(&id)
                    && new_id != id
                {
                    changed = true;
                    new_id
                } else {
                    id
                }
            })
            .collect();
        if changed {
            let replacement = Element::new_i32_field("BoneId", &remapped);
            elem.raw = replacement.raw;
            report.bones_remapped += 1;
        }
    }

    // 5. Drop deprecated direct child `<instance>` nodes and deprecated child fields.
    if !elem.children.is_empty() {
        let ty_for_drop = current_type.as_deref();
        elem.children.retain(|child| {
            if child.tag == "instance"
                && let Some(ty) = child.attr("type")
                && rules.dropped_instance_types.contains(ty)
            {
                if let Some(guid) = child.attr("guid") {
                    dropped_guids.insert(guid.to_owned());
                    if ty == "Audio.ValueRef"
                        && let Some(val_str) = extract_audio_value_ref(child)
                    {
                        lowered_audio_values.insert(guid.to_owned(), val_str);
                    }
                }
                report.instances_dropped += 1;
                return false;
            }
            let Some(name) = child.attr("name") else {
                return true;
            };
            let drop_typed =
                ty_for_drop.is_some_and(|ty| rules.dropped_fields.contains(&(ty.to_owned(), name.to_owned())));
            let drop_global = rules.dropped_fields.contains(&("*".to_owned(), name.to_owned()));
            if drop_typed || drop_global {
                report.fields_dropped += 1;
                return false;
            }
            true
        });
    }

    // 6. Wrap single fields (e.g. `Mesh1p` -> `<array name="Meshes1p"><item .../></array>`).
    if !rules.wrap_as_array.is_empty() {
        for child in &mut elem.children {
            if child.tag == "field"
                && let Some(old_name) = child.attr("name").map(ToOwned::to_owned)
                && let Some(array_name) = rules.wrap_as_array.get(&old_name)
            {
                let mut item = child.clone();
                item.tag = "item".into();
                item.attributes.retain(|(k, _)| k != "name");
                *child = Element {
                    tag: "array".into(),
                    kind: dbx::KIND_CONTAINER,
                    attributes: vec![("name".into(), array_name.clone())],
                    text: String::new(),
                    raw: Vec::new(),
                    children: vec![item],
                };
                report.arrays_wrapped += 1;
            }
        }
    }

    // 7. Recurse into children.
    for child in &mut elem.children {
        upgrade_element(
            child,
            current_type.as_deref(),
            rules,
            dropped_guids,
            lowered_audio_values,
            report,
        );
    }

    // 8. Inject missing default fields on matching instances.
    if is_instance && let Some(inst_ty) = elem.attr("type").map(ToOwned::to_owned) {
        for def in &rules.default_fields {
            if def.instance_type == inst_ty
                && let Some(field_name) = def.element.attr("name")
                && elem.child(field_name).is_none()
            {
                elem.children.push(def.element.clone());
                report.fields_injected += 1;
            }
        }
    }
}

fn extract_audio_value_ref(inst: &Element) -> Option<String> {
    let node = inst.child("Value").or_else(|| inst.child("DefaultValue"))?;
    if node.kind == dbx::KIND_TEXT && !node.text.is_empty() {
        return Some(node.text.clone());
    }
    if let Some(vals) = node.as_f32s()
        && let Some(&first) = vals.first()
    {
        return Some(format!("{first}"));
    }
    if let Some(vals) = node.as_i32s()
        && let Some(&first) = vals.first()
    {
        return Some(format!("{first}"));
    }
    None
}

fn clear_dangling_refs(
    elem: &mut Element,
    dropped_guids: &BTreeSet<String>,
    lowered_audio_values: &BTreeMap<String, String>,
    report: &mut DbxUpgradeReport,
) {
    let mut lowered_val: Option<String> = None;
    for (k, v) in &mut elem.attributes {
        if k == "ref" && dropped_guids.contains(v.as_str()) {
            if let Some(val) = lowered_audio_values.get(v.as_str()) {
                lowered_val = Some(val.clone());
            }
            "null".clone_into(v);
            report.dangling_refs_cleared += 1;
        }
    }
    if let Some(val) = lowered_val {
        if elem.kind == dbx::KIND_TEXT && elem.text.is_empty() {
            elem.text = val;
        } else if elem.attr("loweredValue").is_none() {
            elem.attributes.push(("loweredValue".into(), val));
        }
        report.audio_value_refs_lowered += 1;
    }
    for child in &mut elem.children {
        clear_dangling_refs(child, dropped_guids, lowered_audio_values, report);
    }
    // Prune pure `<item ref="null"/>` entries inside `<array>` containers so Frostbite 1.5
    // never receives raw NULL pointers inside `Objects` / `Components` / `Blueprints` vectors.
    if elem.tag == "array" {
        elem.children.retain(|child| {
            !(child.tag == "item"
                && child.children.is_empty()
                && child.text.is_empty()
                && child.attr("ref") == Some("null"))
        });
    }
}

// ---------------------------------------------------------------------------
// 5. BFBC1 Pre-Rendered Bink Movies (RES\x02 unwrap) & Common Bundle Dedup
// ---------------------------------------------------------------------------

/// Strips Frostbite's 64-byte `RES\x02` wrapper prefix (if present) from a BFBC1 Xbox 360
/// `Output/xenon/movies/*.res` file and returns the underlying PC-playable Bink (`BIKi`/`KB2i`)
/// video stream slice.
///
/// # Errors
/// Returns [`Bfbc1Error::Bink`] if no `BIKi` or `KB2i` magic is found.
pub fn unwrap_bink_movie(data: &[u8]) -> Result<&[u8], Bfbc1Error> {
    let trimmed = if data.starts_with(b"RES\x02") && data.len() > 64 {
        data.get(64..).unwrap_or(data)
    } else {
        data
    };
    if trimmed.starts_with(b"BIKi") || trimmed.starts_with(b"KB2i") {
        return Ok(trimmed);
    }
    if let Some(pos) = memchr::memmem::find(data, b"BIKi").or_else(|| memchr::memmem::find(data, b"KB2i"))
        && let Some(slice) = data.get(pos..)
    {
        return Ok(slice);
    }
    Err(Bfbc1Error::Bink("no BIKi/KB2i Bink video stream found".into()))
}

/// Summary report produced by [`upgrade_kynogon_hpd`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HpdUpgradeReport {
    /// Number of navmesh sectors upgraded.
    pub sectors: usize,
    /// Total graph nodes preserved across all sectors.
    pub nodes: usize,
    /// Total graph edges compacted from 20 bytes to 12 bytes.
    pub edges: usize,
}

/// Upgrades a Frostbite 1.0 (BFBC1) Kynogon HPD v1 (`1.3f` magic, 20-byte edges) navmesh file
/// into Frostbite 1.5 (BFBC2 PC) Kynogon HPD v2 (`"Kynogon HPD\0"`, version `2.2`, 12-byte edges).
///
/// Each sector's 36-byte nodes and trailing `"Kynogon InPlace Mesh"` payload (natively supported
/// by BFBC2's Kynapse runtime) are preserved verbatim while edge records are compacted from
/// 20 bytes to 12 bytes and sector offsets/sizes are recalculated.
///
/// # Errors
/// Returns [`Bfbc1Error::Hpd`] if the input is truncated or does not match Kynogon HPD v1/v2.
pub fn upgrade_kynogon_hpd(data: &[u8]) -> Result<(Vec<u8>, HpdUpgradeReport), Bfbc1Error> {
    const HPD_V1_MAGIC: u32 = 0x3FA6_6666; // 1.3f
    const HPD_V1_HDR_LEN: usize = 0x84;
    const HPD_V2_HDR_LEN: usize = 0x9C;
    const SECTOR_ENTRY_LEN: usize = 48;
    const NODE_LEN: usize = 36;
    const V1_EDGE_LEN: usize = 20;
    const V2_EDGE_LEN: usize = 12;

    if data.starts_with(b"Kynogon HPD\0") {
        // Already HPD v2
        let sectors = if data.len() >= 0x18 {
            u32::from_le_bytes(data[0x14..0x18].try_into().unwrap_or([0; 4])) as usize
        } else {
            0
        };
        return Ok((
            data.to_vec(),
            HpdUpgradeReport {
                sectors,
                nodes: 0,
                edges: 0,
            },
        ));
    }

    if data.len() < HPD_V1_HDR_LEN {
        return Err(Bfbc1Error::Hpd(format!(
            "HPD too small ({} bytes < {HPD_V1_HDR_LEN})",
            data.len()
        )));
    }
    let magic = u32::from_le_bytes(data[0..4].try_into().unwrap_or([0; 4]));
    if magic != HPD_V1_MAGIC {
        return Err(Bfbc1Error::Hpd(format!(
            "unexpected HPD v1 magic {magic:#010x} (expected {HPD_V1_MAGIC:#010x})"
        )));
    }
    let sector_count = u32::from_le_bytes(data[4..8].try_into().unwrap_or([0; 4])) as usize;
    let node_size = u32::from_le_bytes(data[0x7C..0x80].try_into().unwrap_or([0; 4])) as usize;
    let edge_size = u32::from_le_bytes(data[0x80..0x84].try_into().unwrap_or([0; 4])) as usize;
    if node_size != NODE_LEN || edge_size != V1_EDGE_LEN {
        return Err(Bfbc1Error::Hpd(format!(
            "unsupported HPD v1 node/edge sizes ({node_size}/{edge_size}, expected {NODE_LEN}/{V1_EDGE_LEN})"
        )));
    }

    let tbl_end = HPD_V1_HDR_LEN
        .checked_add(sector_count.saturating_mul(SECTOR_ENTRY_LEN))
        .ok_or_else(|| Bfbc1Error::Hpd("sector table overflow".into()))?;
    if tbl_end > data.len() {
        return Err(Bfbc1Error::Hpd(format!(
            "sector table end {tbl_end:#x} exceeds file size {:#x}",
            data.len()
        )));
    }

    // Locate payload base from the last sector entry's (offset + size)
    let pay_base = if sector_count == 0 {
        tbl_end
    } else {
        let last_off_pos = HPD_V1_HDR_LEN + (sector_count - 1) * SECTOR_ENTRY_LEN;
        let last_sz =
            u32::from_le_bytes(data[last_off_pos + 0x20..last_off_pos + 0x24].try_into().unwrap_or([0; 4]))
                as usize;
        let last_off =
            u32::from_le_bytes(data[last_off_pos + 0x24..last_off_pos + 0x28].try_into().unwrap_or([0; 4]))
                as usize;
        let total_pay = last_off
            .checked_add(last_sz)
            .ok_or_else(|| Bfbc1Error::Hpd("sector payload length overflow".into()))?;
        if total_pay > data.len() {
            return Err(Bfbc1Error::Hpd(format!(
                "sector payload ({total_pay}) exceeds file size ({})",
                data.len()
            )));
        }
        data.len() - total_pay
    };
    if pay_base < tbl_end {
        return Err(Bfbc1Error::Hpd(format!(
            "computed payload base {pay_base:#x} overlaps sector table end {tbl_end:#x}"
        )));
    }

    // Build HPD v2 header (0x9C bytes)
    let mut out = Vec::with_capacity(data.len());
    out.extend_from_slice(b"Kynogon HPD\0"); // 0x00..0x0C
    out.extend_from_slice(&2u32.to_le_bytes()); // 0x0C: version = 2
    out.extend_from_slice(&2u32.to_le_bytes()); // 0x10: minor = 2
    out.extend_from_slice(&(sector_count as u32).to_le_bytes()); // 0x14: sector_count
    out.extend_from_slice(&1u32.to_le_bytes()); // 0x18
    out.extend_from_slice(&[0u8; 16]); // 0x1C..0x2C
    out.extend_from_slice(&2u32.to_le_bytes()); // 0x2C
    out.extend_from_slice(&0u32.to_le_bytes()); // 0x30
    out.extend_from_slice(&8u32.to_le_bytes()); // 0x34 (FB1.5 uses 8 vs FB1.0's 6)
    out.extend_from_slice(&[0u8; 0x8C - 0x38]); // 0x38..0x8C: 21 zero u32s
    out.extend_from_slice(&(NODE_LEN as u32).to_le_bytes()); // 0x8C: node_size = 36
    out.extend_from_slice(&(V2_EDGE_LEN as u32).to_le_bytes()); // 0x90: edge_size = 12
    out.extend_from_slice(&1u32.to_le_bytes()); // 0x94
    out.extend_from_slice(&0u32.to_le_bytes()); // 0x98
    debug_assert_eq!(out.len(), HPD_V2_HDR_LEN);

    let mut new_table = Vec::with_capacity(sector_count * SECTOR_ENTRY_LEN);
    let mut new_payload = Vec::with_capacity(data.len().saturating_sub(pay_base));
    let mut report = HpdUpgradeReport {
        sectors: sector_count,
        nodes: 0,
        edges: 0,
    };

    for i in 0..sector_count {
        let epos = HPD_V1_HDR_LEN + i * SECTOR_ENTRY_LEN;
        let mut entry = [0u8; SECTOR_ENTRY_LEN];
        entry.copy_from_slice(&data[epos..epos + SECTOR_ENTRY_LEN]);
        let old_sz = u32::from_le_bytes(entry[0x20..0x24].try_into().unwrap_or([0; 4])) as usize;
        let old_off = u32::from_le_bytes(entry[0x24..0x28].try_into().unwrap_or([0; 4])) as usize;
        let s_start = pay_base
            .checked_add(old_off)
            .ok_or_else(|| Bfbc1Error::Hpd(format!("sector {i} offset overflow")))?;
        let s_end = s_start
            .checked_add(old_sz)
            .ok_or_else(|| Bfbc1Error::Hpd(format!("sector {i} size overflow")))?;
        if s_end > data.len() || old_sz < 8 {
            return Err(Bfbc1Error::Hpd(format!(
                "sector {i} range {s_start:#x}..{s_end:#x} out of bounds"
            )));
        }
        let sec = &data[s_start..s_end];
        let n_nodes = u32::from_le_bytes(sec[0..4].try_into().unwrap_or([0; 4])) as usize;
        let n_edges = u32::from_le_bytes(sec[4..8].try_into().unwrap_or([0; 4])) as usize;
        let nodes_end = 8usize
            .checked_add(n_nodes.saturating_mul(NODE_LEN))
            .ok_or_else(|| Bfbc1Error::Hpd(format!("sector {i} node table overflow")))?;
        let edges_end = nodes_end
            .checked_add(n_edges.saturating_mul(V1_EDGE_LEN))
            .ok_or_else(|| Bfbc1Error::Hpd(format!("sector {i} edge table overflow")))?;
        if edges_end > sec.len() {
            return Err(Bfbc1Error::Hpd(format!(
                "sector {i} graph ({n_nodes} nodes, {n_edges} edges) exceeds sector size {}",
                sec.len()
            )));
        }

        let new_off = new_payload.len();
        // Copy header + nodes verbatim
        new_payload.extend_from_slice(&sec[..nodes_end]);
        // Compact 20-byte edges -> 12-byte edges (tag, from, to)
        for e in 0..n_edges {
            let base = nodes_end + e * V1_EDGE_LEN;
            new_payload.extend_from_slice(&sec[base..base + V2_EDGE_LEN]);
        }
        // Copy trailing Kynogon InPlace Mesh payload verbatim
        new_payload.extend_from_slice(&sec[edges_end..]);
        let new_sz = new_payload.len() - new_off;

        entry[0x20..0x24].copy_from_slice(&(new_sz as u32).to_le_bytes());
        entry[0x24..0x28].copy_from_slice(&(new_off as u32).to_le_bytes());
        new_table.extend_from_slice(&entry);

        report.nodes += n_nodes;
        report.edges += n_edges;
    }

    out.extend_from_slice(&new_table);
    out.extend_from_slice(&new_payload);
    Ok((out, report))
}

/// Builds a binary Frostbite 1.5 `GameSharedResources.PathdataSectorOffsets` DBX partition
/// (`Levels/<level>/Precomputed/<layer>/SectorOffsets.dbx`) so `BFBC2Game.exe` (`0x00F20A33`)
/// discovers and opens the upgraded Kynogon HPD v2 `PathData.hpd` file for BFBC1 SP levels.
#[must_use]
pub fn build_sector_offsets_dbx(level_name: &str, layer: &str, x_offset: i32, z_offset: i32) -> Vec<u8> {
    // Deterministic FNV-1a-derived UUIDs from (level_name, layer)
    let make_guid = |salt: u64| -> String {
        let mut h1: u64 = 0xCBF2_9CE4_8422_2325 ^ salt;
        let mut h2: u64 = 0x8422_2325_CBF2_9CE4 ^ salt.rotate_left(17);
        for b in level_name.bytes().chain(b"/".iter().copied()).chain(layer.bytes()) {
            h1 ^= u64::from(b.to_ascii_lowercase());
            h1 = h1.wrapping_mul(0x0100_0000_01B3);
            h2 ^= u64::from(b.to_ascii_uppercase());
            h2 = h2.wrapping_mul(0x0100_0000_01B3);
        }
        format!(
            "{:08X}-{:04X}-{:04X}-{:04X}-{:012X}",
            (h1 >> 32) as u32,
            ((h1 >> 16) & 0xFFFF) as u16,
            (h1 & 0xFFFF) as u16,
            ((h2 >> 48) & 0xFFFF) as u16,
            h2 & 0xFFFF_FFFF_FFFF
        )
    };
    let part_guid = make_guid(0x5041_5254_0001);
    let inst_guid = make_guid(0x494E_5354_0002);
    let asset_name = format!("Levels/{level_name}/precomputed/{layer}/SectorOffsets");

    let field = |name: &str, text: String| Element {
        tag: "field".into(),
        kind: dbx::KIND_TEXT,
        attributes: vec![("name".into(), name.into())],
        text,
        raw: Vec::new(),
        children: Vec::new(),
    };
    let root = Element {
        tag: "partition".into(),
        kind: dbx::KIND_CONTAINER,
        attributes: vec![
            ("guid".into(), part_guid),
            ("primaryInstance".into(), inst_guid.clone()),
            ("exportMode".into(), "All".into()),
        ],
        text: String::new(),
        raw: Vec::new(),
        children: vec![Element {
            tag: "instance".into(),
            kind: dbx::KIND_CONTAINER,
            attributes: vec![
                ("guid".into(), inst_guid),
                ("type".into(), "GameSharedResources.PathdataSectorOffsets".into()),
                ("id".into(), "SectorOffsets".into()),
            ],
            text: String::new(),
            raw: Vec::new(),
            children: vec![
                field("Name", asset_name),
                field("MemoryReportGroup", "MemoryReportGroup_default".into()),
                field("XOffset", x_offset.to_string()),
                field("ZOffset", z_offset.to_string()),
            ],
        }],
    };
    DbxFile { roots: vec![root] }.to_bytes()
}

/// Removes entries from a self-contained BFBC1 `level-00.fbrb` archive whose case-insensitive
/// paths already exist in a shared `bfbc1_common` archive, returning `(deduped_archive, removed_count)`.
#[must_use]
pub fn deduplicate_common_entries(level_arc: &FbrbArchive, common_paths: &BTreeSet<String>) -> (FbrbArchive, usize) {
    let mut kept = Vec::with_capacity(level_arc.entries.len());
    let mut removed = 0usize;
    for entry in &level_arc.entries {
        if common_paths.contains(&entry.path.to_ascii_lowercase()) {
            removed += 1;
        } else {
            kept.push(entry.clone());
        }
    }
    (
        FbrbArchive {
            version: level_arc.version,
            entries: kept,
        },
        removed,
    )
}

fn is_ascii_hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

fn is_guid_bytes(s: &[u8]) -> bool {
    s.len() == 36
        && s[8] == b'-'
        && s[13] == b'-'
        && s[18] == b'-'
        && s[23] == b'-'
        && s.iter()
            .enumerate()
            .all(|(i, &b)| matches!(i, 8 | 13 | 18 | 23) || is_ascii_hex(b))
}

fn contains_ascii_ci(hay: &[u8], needle_lower: &[u8]) -> bool {
    if needle_lower.is_empty() || needle_lower.len() > hay.len() {
        return false;
    }
    hay.windows(needle_lower.len())
        .any(|w| w.iter().zip(needle_lower).all(|(&a, &b)| a.to_ascii_lowercase() == b))
}

/// GUID-aware deduplication of a BFBC1 level archive against PC BFBC2's `mp_common`.
///
/// Removes entries whose path exists in `common_arc`, **except** when any DBX in `level_arc`
/// references `<partition>/<guid>` whose `<guid>` is absent from `common_arc`'s DBX — in which
/// case `<partition>.dbx` and all paired `.res` files sharing that stem (`<partition>.res`,
/// `<partition>_*`) are preserved from `level_arc`.
#[must_use]
pub fn deduplicate_common_entries_guid_aware(
    level_arc: &FbrbArchive,
    common_arc: &FbrbArchive,
) -> (FbrbArchive, usize) {
    let mut common_paths = BTreeSet::new();
    let mut common_dbx: BTreeMap<String, &[u8]> = BTreeMap::new();
    for entry in &common_arc.entries {
        if entry.data.is_empty() || entry.flags == 0 {
            continue;
        }
        let lower = entry.path.replace('\\', "/").to_ascii_lowercase();
        if let Some(stem) = lower.strip_suffix(".dbx") {
            common_dbx.insert(stem.to_owned(), entry.data.as_slice());
        }
        common_paths.insert(lower);
    }

    // Scan level DBXs for `<partition>/<36-char-guid>` references whose GUID is missing in common_dbx
    let mut protected_stems: BTreeSet<String> = BTreeSet::new();
    for entry in &level_arc.entries {
        if !entry.path.to_ascii_lowercase().ends_with(".dbx") || entry.data.len() < 40 {
            continue;
        }
        let d = entry.data.as_slice();
        let mut i = 0usize;
        while i + 37 <= d.len() {
            if d[i] == b'/' && is_guid_bytes(&d[i + 1..i + 37]) {
                let mut start = i;
                while start > 0 && d[start - 1] >= 0x20 && d[start - 1] < 0x7F && d[start - 1] != b'"' {
                    start -= 1;
                }
                if start < i {
                    let part = String::from_utf8_lossy(&d[start..i])
                        .replace('\\', "/")
                        .to_ascii_lowercase();
                    if let Some(&com_bytes) = common_dbx.get(&part) {
                        let guid_lower: Vec<u8> = d[i + 1..i + 37]
                            .iter()
                            .map(u8::to_ascii_lowercase)
                            .collect();
                        if !contains_ascii_ci(com_bytes, &guid_lower) {
                            protected_stems.insert(part);
                        }
                    }
                }
                i += 37;
            } else {
                i += 1;
            }
        }
    }

    let mut kept = Vec::with_capacity(level_arc.entries.len());
    let mut removed = 0usize;
    for entry in &level_arc.entries {
        let lower = entry.path.replace('\\', "/").to_ascii_lowercase();
        let is_protected = protected_stems.iter().any(|stem| {
            lower == format!("{stem}.dbx")
                || lower == format!("{stem}.res")
                || lower.starts_with(&format!("{stem}_"))
        });
        if !is_protected && common_paths.contains(&lower) {
            removed += 1;
        } else {
            kept.push(entry.clone());
        }
    }
    (
        FbrbArchive {
            version: level_arc.version,
            entries: kept,
        },
        removed,
    )
}

fn normalize_ref_string(s: &mut String) -> bool {
    if s.contains(".xenonTexture") {
        *s = s.replace(".xenonTexture", ".itexture");
        return true;
    }
    if s.contains(".XenonTexture") {
        *s = s.replace(".XenonTexture", ".ITexture");
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// 2. Terrain Heightfield Upgrader (FB 1.0 -> FB 1.5)
// ---------------------------------------------------------------------------

/// Summary report returned by [`upgrade_heightfield`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerrainUpgradeReport {
    /// Grid dimension (`64`, `128`, `256`, `512`, `1024`, or `2048`).
    pub dimension: u32,
    /// Source header byte length (`0x29`, `0x2D`, or `0x31`).
    pub source_header_len: usize,
    /// Output header byte length (`0x31`).
    pub target_header_len: usize,
}

fn read_u32_be(x: &[u8], at: usize) -> Option<u32> {
    let bytes: [u8; 4] = x.get(at..at + 4)?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

fn read_u32_le(x: &[u8], at: usize) -> Option<u32> {
    let bytes: [u8; 4] = x.get(at..at + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn read_u16_le(x: &[u8], at: usize) -> Option<u16> {
    let bytes: [u8; 2] = x.get(at..at + 2)?.try_into().ok()?;
    Some(u16::from_le_bytes(bytes))
}

fn swap_chunks(src: &[u8], dst: &mut [u8], width: usize) -> Result<(), Bfbc1Error> {
    if src.len() != dst.len() || !src.len().is_multiple_of(width) {
        return Err(Bfbc1Error::Terrain(format!(
            "byte-swap length mismatch: src={} dst={} width={width}",
            src.len(),
            dst.len()
        )));
    }
    for (d, s) in dst.chunks_exact_mut(width).zip(src.chunks_exact(width)) {
        for (a, b) in d.iter_mut().zip(s.iter().rev()) {
            *a = *b;
        }
    }
    Ok(())
}

/// Upgrades a BFBC1 (Frostbite 1.0) Xbox 360 `TerrainHeightfield` to Frostbite 1.5 PC format.
///
/// Automatically handles:
/// - Standard `64`, `128`, `256`, `1024`, `2048` heightfields with `0x2D` or `0x31` headers
///   (delegated to [`terrain::heightfield`]).
/// - Frostbite 1.0 compact `0x29`-byte headers and `512x512` heightfield tiles,
///   expanding the header to `0x31` bytes and byte-swapping the height samples and quadtree tail.
///
/// # Errors
/// Returns [`Bfbc1Error::Terrain`] if the input header or payload geometry is invalid.
pub fn upgrade_heightfield(data: &[u8]) -> Result<(Vec<u8>, TerrainUpgradeReport), Bfbc1Error> {
    let n = read_u32_be(data, 0x18).ok_or_else(|| Bfbc1Error::Terrain("heightfield shorter than 0x1C bytes".into()))?;
    let n_usize = n as usize;

    // Fast path: standard BFBC1/BF1943 (0x2D) or BFBC2 (0x31) layout supported directly by terrain.rs.
    if matches!(n, 64 | 128 | 256 | 1024 | 2048) {
        if let Ok(pc) = terrain::heightfield(data, false) {
            return Ok((
                pc,
                TerrainUpgradeReport {
                    dimension: n,
                    source_header_len: FB10_EXT_HEIGHTFIELD_HEADER,
                    target_header_len: FB15_HEIGHTFIELD_HEADER,
                },
            ));
        }
        if let Ok(pc) = terrain::heightfield(data, true) {
            return Ok((
                pc,
                TerrainUpgradeReport {
                    dimension: n,
                    source_header_len: FB15_HEIGHTFIELD_HEADER,
                    target_header_len: FB15_HEIGHTFIELD_HEADER,
                },
            ));
        }
    }

    // Frostbite 1.0 compact 0x29 header or 512x512 sub-terrain heightfield.
    if !matches!(n, 64 | 128 | 256 | 512 | 1024 | 2048) {
        return Err(Bfbc1Error::Terrain(format!("unsupported heightfield dimension {n}")));
    }

    let grid_bytes = 2usize
        .checked_mul(n_usize)
        .and_then(|v| v.checked_mul(n_usize))
        .ok_or_else(|| Bfbc1Error::Terrain("heightfield grid size overflow".into()))?;
    let quad_bytes = (4 * terrain::quadtree_nodes(u64::from(n))) as usize;

    let src_hdr = if data.len() == FB10_HEIGHTFIELD_HEADER + grid_bytes + quad_bytes {
        FB10_HEIGHTFIELD_HEADER
    } else if data.len() == FB10_EXT_HEIGHTFIELD_HEADER + grid_bytes + quad_bytes {
        FB10_EXT_HEIGHTFIELD_HEADER
    } else {
        return Err(Bfbc1Error::Terrain(format!(
            "unrecognized Frostbite 1.0 heightfield size {} for dimension {n}",
            data.len()
        )));
    };

    let mut out = vec![0u8; FB15_HEIGHTFIELD_HEADER + grid_bytes + quad_bytes];
    swap_chunks(
        data.get(0..0x24)
            .ok_or_else(|| Bfbc1Error::Terrain("short header".into()))?,
        out.get_mut(0..0x24)
            .ok_or_else(|| Bfbc1Error::Terrain("short out".into()))?,
        4,
    )?;
    swap_chunks(
        data.get(0x24..0x28)
            .ok_or_else(|| Bfbc1Error::Terrain("short header".into()))?,
        out.get_mut(0x24..0x28)
            .ok_or_else(|| Bfbc1Error::Terrain("short out".into()))?,
        2,
    )?;
    if src_hdr >= FB10_EXT_HEIGHTFIELD_HEADER {
        swap_chunks(
            data.get(0x28..0x2C)
                .ok_or_else(|| Bfbc1Error::Terrain("short ext header".into()))?,
            out.get_mut(0x28..0x2C)
                .ok_or_else(|| Bfbc1Error::Terrain("short out".into()))?,
            4,
        )?;
    }

    let src_grid = data
        .get(src_hdr..src_hdr + grid_bytes)
        .ok_or_else(|| Bfbc1Error::Terrain("truncated heightfield grid".into()))?;
    let dst_grid = out
        .get_mut(FB15_HEIGHTFIELD_HEADER..FB15_HEIGHTFIELD_HEADER + grid_bytes)
        .ok_or_else(|| Bfbc1Error::Terrain("short out grid".into()))?;
    swap_chunks(src_grid, dst_grid, 2)?;

    let src_tail = data
        .get(src_hdr + grid_bytes..src_hdr + grid_bytes + quad_bytes)
        .ok_or_else(|| Bfbc1Error::Terrain("truncated quadtree tail".into()))?;
    let dst_tail = out
        .get_mut(FB15_HEIGHTFIELD_HEADER + grid_bytes..)
        .ok_or_else(|| Bfbc1Error::Terrain("short out tail".into()))?;
    swap_chunks(src_tail, dst_tail, 2)?;

    Ok((
        out,
        TerrainUpgradeReport {
            dimension: n,
            source_header_len: src_hdr,
            target_header_len: FB15_HEIGHTFIELD_HEADER,
        },
    ))
}

/// Downsamples a PC little-endian `256x256` sub-terrain heightfield to `128x128` via 2x2 box
/// averaging and rebuilds its min/max quadtree (fallback if a target PC runtime clamps sub-tiles
/// to `N <= 128`).
///
/// # Errors
/// Returns [`Bfbc1Error::Terrain`] if `pc_256` is not a valid `256x256` PC sub-heightfield.
pub fn downsample_sub_heightfield_256_to_128(pc_256: &[u8]) -> Result<Vec<u8>, Bfbc1Error> {
    let n = read_u32_le(pc_256, 0x18).ok_or_else(|| Bfbc1Error::Terrain("short PC heightfield".into()))?;
    if n != 256 {
        return Err(Bfbc1Error::Terrain(format!("expected 256x256 heightfield, got {n}")));
    }
    let grid_256 = 2 * 256 * 256;
    if pc_256.len() < FB15_HEIGHTFIELD_HEADER + grid_256 {
        return Err(Bfbc1Error::Terrain("truncated 256x256 heightfield grid".into()));
    }

    let grid_128 = 2 * 128 * 128;
    let quad_128 = (4 * terrain::quadtree_nodes(128)) as usize;
    let mut out = vec![0u8; FB15_HEIGHTFIELD_HEADER + grid_128 + quad_128];
    if let (Some(dst), Some(src)) = (
        out.get_mut(..FB15_HEIGHTFIELD_HEADER),
        pc_256.get(..FB15_HEIGHTFIELD_HEADER),
    ) {
        dst.copy_from_slice(src);
    }
    // Update dimension (128) and quadtree depth (bit_length(128) - 4 = 4)
    if let Some(slot) = out.get_mut(0x18..0x1C) {
        slot.copy_from_slice(&128u32.to_le_bytes());
    }
    if let Some(slot) = out.get_mut(0x1C..0x20) {
        slot.copy_from_slice(&4u32.to_le_bytes());
    }

    let mut h128 = vec![0u16; 128 * 128];
    for y in 0..128usize {
        for x in 0..128usize {
            let s00 = read_u16_le(pc_256, FB15_HEIGHTFIELD_HEADER + 2 * ((2 * y) * 256 + (2 * x))).unwrap_or(0);
            let s01 = read_u16_le(pc_256, FB15_HEIGHTFIELD_HEADER + 2 * ((2 * y) * 256 + (2 * x + 1))).unwrap_or(0);
            let s10 = read_u16_le(pc_256, FB15_HEIGHTFIELD_HEADER + 2 * ((2 * y + 1) * 256 + (2 * x))).unwrap_or(0);
            let s11 = read_u16_le(pc_256, FB15_HEIGHTFIELD_HEADER + 2 * ((2 * y + 1) * 256 + (2 * x + 1))).unwrap_or(0);
            let avg = ((u32::from(s00) + u32::from(s01) + u32::from(s10) + u32::from(s11) + 2) / 4) as u16;
            if let Some(slot) = h128.get_mut(y * 128 + x) {
                *slot = avg;
            }
            let off = FB15_HEIGHTFIELD_HEADER + 2 * (y * 128 + x);
            if let Some(dst) = out.get_mut(off..off + 2) {
                dst.copy_from_slice(&avg.to_le_bytes());
            }
        }
    }

    // Rebuild 4-level min/max quadtree (1 + 4 + 16 + 64 = 85 nodes)
    let mut tail_off = FB15_HEIGHTFIELD_HEADER + grid_128;
    for level in 0..4u32 {
        let cells = 1usize << level;
        let span = 128 / cells;
        for cy in 0..cells {
            for cx in 0..cells {
                let mut lo = u16::MAX;
                let mut hi = 0u16;
                for dy in 0..span {
                    for dx in 0..span {
                        let v = h128
                            .get((cy * span + dy) * 128 + (cx * span + dx))
                            .copied()
                            .unwrap_or(0);
                        lo = lo.min(v);
                        hi = hi.max(v);
                    }
                }
                if let Some(dst) = out.get_mut(tail_off..tail_off + 2) {
                    dst.copy_from_slice(&lo.to_le_bytes());
                }
                if let Some(dst) = out.get_mut(tail_off + 2..tail_off + 4) {
                    dst.copy_from_slice(&hi.to_le_bytes());
                }
                tail_off += 4;
            }
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// 3. Mesh Vertex Layout & MeshData Header Upgrader
// ---------------------------------------------------------------------------

/// Result of computing a PC Frostbite 1.5 vertex layout from a console Frostbite 1.0 stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcVertexLayout {
    /// Element conversion pairs for [`mesh::convert_vertices`].
    pub pairs: Vec<Pair>,
    /// Computed PC vertex stride in bytes.
    pub pc_stride: usize,
    /// Number of console-only packed formats (`0x0D` UBYTE4N / `0x2B` DEC3N) expanded to `float16x4`.
    pub expanded_elements: usize,
}

/// Computes a Frostbite 1.5 PC vertex layout from a console `(format, offset)` element list.
///
/// Maps console-only packed formats (`0x0D` UBYTE4N, `0x2B` DEC3N) to PC `float16x4` (`0x08`)
/// and packs all elements sequentially into a tightly aligned PC vertex stride.
///
/// # Errors
/// Returns [`Bfbc1Error::Mesh`] if any vertex element format is unknown to [`mesh::size`].
pub fn compute_pc_vertex_layout(console_elements: &[(u8, usize)]) -> Result<PcVertexLayout, Bfbc1Error> {
    let mut pairs = Vec::with_capacity(console_elements.len());
    let mut pc_offset = 0usize;
    let mut expanded_elements = 0usize;

    for &(fc, oc) in console_elements {
        if mesh::size(fc).is_none() {
            return Err(Bfbc1Error::Mesh(format!("unknown console vertex format {fc:#x}")));
        }
        let fp = match fc {
            0x0D | 0x2B => {
                expanded_elements += 1;
                0x08 // float16x4
            }
            other => other,
        };
        let fp_size = mesh::size(fp).ok_or_else(|| Bfbc1Error::Mesh(format!("unknown PC vertex format {fp:#x}")))?;
        pairs.push(Pair {
            fc,
            oc,
            fp,
            op: pc_offset,
        });
        pc_offset = pc_offset.saturating_add(fp_size);
    }

    Ok(PcVertexLayout {
        pairs,
        pc_stride: pc_offset,
        expanded_elements,
    })
}

/// Synthesizes a 70-byte Frostbite PC vertex layout record body from a console vertex layout
/// record body (extracted by [`meshconv::console_layouts`]), expanding `DEC3N` (`0x2B`) and
/// `UBYTE4N` (`0x0D`) elements to `float16x4` (`0x08`) and recomputing stream-0 offsets/stride.
#[must_use]
pub fn build_pc_layout_body(console_layout: &[u8]) -> Option<Vec<u8>> {
    let (els, _) = meshconv::elements(console_layout)?;
    let mut out = console_layout.to_vec();
    if out.len() < 70 {
        out.resize(70, 0);
    }
    let mut offset_s0 = 0usize;
    for (i, &(fc, usage, orig_off, stream)) in els.iter().enumerate() {
        let (fp, new_off) = if stream == 0 {
            let fp = match fc {
                0x0D | 0x2B => 0x08,
                other => other,
            };
            let elem_size = mesh::size(fp)?;
            let o = u8::try_from(offset_s0).ok()?;
            offset_s0 = offset_s0.saturating_add(elem_size);
            (fp, o)
        } else {
            let fp = match fc {
                0x0D | 0x2B => 0x08,
                other => other,
            };
            (fp, orig_off)
        };
        if let Some(slot) = out.get_mut(4 * i..4 * i + 4) {
            slot.copy_from_slice(&[fp, usage, new_off, stream]);
        }
    }
    if let Some(stride_slot) = out.get_mut(69) {
        *stride_slot = u8::try_from(offset_s0).ok()?;
    }
    Some(out)
}

/// Upgrades a Frostbite 1.0 Xbox 360 `MeshData` header into Frostbite 1.5 PC little-endian bytes,
/// attaching the converted PC vertex and index buffers.
///
/// # Errors
/// Returns [`Bfbc1Error::Mesh`] if the `MeshData` header is malformed.
pub fn upgrade_mesh_header(data: &[u8], pc_vertices: &[u8], pc_indices: &[u8]) -> Result<Vec<u8>, Bfbc1Error> {
    let hdr =
        meshdata::parse_mesh(data, true).map_err(|_| Bfbc1Error::Mesh("invalid big-endian MeshData header".into()))?;
    meshdata::write_mesh(&hdr, pc_vertices, pc_indices)
        .map_err(|_| Bfbc1Error::Mesh("failed to serialize PC MeshData header".into()))
}

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
        let pack_slice = if le_pack.len() >= magic_off {
            &le_pack[magic_off..]
        } else {
            &le_pack
        };
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
/// - Patches the 16-byte SDK banner at `0x28..0x38` to `target_sdk` (typically [`FB15_HAVOK_SDK`]).
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn upgrades_bfbc1_dbx_empirical_schema_rules() {
        let root = Element {
            tag: "partition".into(),
            kind: dbx::KIND_CONTAINER,
            attributes: vec![("guid".into(), "11111111-2222-3333-4444-555555555555".into())],
            text: String::new(),
            raw: Vec::new(),
            children: vec![
                // Deprecated BFBC1-only instances that must be dropped:
                Element {
                    tag: "instance".into(),
                    kind: dbx::KIND_CONTAINER,
                    attributes: vec![
                        ("guid".into(), "00000000-0000-0000-0000-000000000001".into()),
                        ("type".into(), "Audio.ValueRef".into()),
                    ],
                    text: String::new(),
                    raw: Vec::new(),
                    children: Vec::new(),
                },
                Element {
                    tag: "instance".into(),
                    kind: dbx::KIND_CONTAINER,
                    attributes: vec![
                        ("guid".into(), "00000000-0000-0000-0000-000000000002".into()),
                        ("type".into(), "GameSharedResources.LevelReferencedAssetsData".into()),
                    ],
                    text: String::new(),
                    raw: Vec::new(),
                    children: Vec::new(),
                },
                // Renamed SP class + wrapped Mesh1p + EIA enum + BoneId + console ref:
                Element {
                    tag: "instance".into(),
                    kind: dbx::KIND_CONTAINER,
                    attributes: vec![
                        ("guid".into(), "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into()),
                        ("type".into(), "GameSharedResources.RegroupOrderEntityData".into()),
                    ],
                    text: String::new(),
                    raw: Vec::new(),
                    children: vec![
                        Element {
                            tag: "field".into(),
                            kind: dbx::KIND_TEXT,
                            attributes: vec![("name".into(), "Mesh1p".into())],
                            text: "weapons/xm8/xm8_1p.xenonTexture".into(),
                            raw: Vec::new(),
                            children: Vec::new(),
                        },
                        Element {
                            tag: "field".into(),
                            kind: dbx::KIND_TEXT,
                            attributes: vec![("name".into(), "Action".into())],
                            text: "EIAFire".into(),
                            raw: Vec::new(),
                            children: Vec::new(),
                        },
                        Element::new_i32_field("BoneId", &[12]),
                        Element {
                            tag: "field".into(),
                            kind: dbx::KIND_TEXT,
                            attributes: vec![
                                ("name".into(), "SoundParam".into()),
                                ("ref".into(), "00000000-0000-0000-0000-000000000001".into()),
                            ],
                            text: String::new(),
                            raw: Vec::new(),
                            children: Vec::new(),
                        },
                    ],
                },
            ],
        };

        let mut rules = DbxSchemaRules::bfbc1_to_fb15();
        rules.bone_id_map.insert(12, 42);

        let raw_dbx = DbxFile { roots: vec![root] }.to_bytes();
        let (upgraded_bytes, report) = upgrade_dbx_bytes(&raw_dbx, &rules).unwrap();
        assert_eq!(report.instances_dropped, 2);
        assert_eq!(report.fields_dropped, 0);
        assert_eq!(report.dangling_refs_cleared, 1);
        assert_eq!(report.types_renamed, 1);
        assert_eq!(report.arrays_wrapped, 1);
        assert_eq!(report.enums_renamed, 1);
        assert_eq!(report.bones_remapped, 1);
        assert_eq!(report.refs_updated, 1);

        let parsed = dbx::parse(&upgraded_bytes).unwrap();
        let insts: Vec<&Element> = parsed.instances().collect();
        assert_eq!(insts.len(), 1);
        assert_eq!(insts[0].attr("type"), Some("GameSharedResources.FollowOrderEntityData"));
        let meshes1p = insts[0].child("Meshes1p").unwrap();
        assert_eq!(meshes1p.tag, "array");
        assert_eq!(meshes1p.children[0].text, "weapons/xm8/xm8_1p.itexture");
        assert_eq!(insts[0].child("Action").unwrap().text, "EiaFire");
        assert_eq!(insts[0].child("BoneId").unwrap().as_i32s(), Some(vec![42]));
        assert_eq!(insts[0].child("SoundParam").unwrap().attr("ref"), Some("null"));
    }

    #[test]
    fn upgrades_bfbc1_256x256_sub_heightfield_and_downsamples_to_128() {
        let n: u32 = 256;
        let grid_bytes = 2 * 256 * 256;
        let quad_bytes = (4 * terrain::quadtree_nodes(256)) as usize;
        assert_eq!(FB10_EXT_HEIGHTFIELD_HEADER + grid_bytes + quad_bytes, 132_481);

        let mut src = vec![0u8; 132_481];
        src[0x18..0x1C].copy_from_slice(&n.to_be_bytes());
        src[0x1C..0x20].copy_from_slice(&5u32.to_be_bytes()); // bit_length(256) - 4 = 5
        src[FB10_EXT_HEIGHTFIELD_HEADER..FB10_EXT_HEIGHTFIELD_HEADER + 2].copy_from_slice(&400u16.to_be_bytes());

        let (pc_256, report) = upgrade_heightfield(&src).unwrap();
        assert_eq!(report.dimension, 256);
        assert_eq!(report.source_header_len, FB10_EXT_HEIGHTFIELD_HEADER);
        assert_eq!(pc_256.len(), FB15_HEIGHTFIELD_HEADER + grid_bytes + quad_bytes);

        let pc_128 = downsample_sub_heightfield_256_to_128(&pc_256).unwrap();
        assert_eq!(read_u32_le(&pc_128, 0x18), Some(128));
        assert_eq!(read_u16_le(&pc_128, FB15_HEIGHTFIELD_HEADER), Some(100)); // (400+0+0+0+2)/4 = 100
    }

    #[test]
    fn computes_pc_vertex_layout_and_builds_layout_body() {
        let layout = compute_pc_vertex_layout(&[(0x03, 0), (0x2B, 12), (0x0D, 16)]).unwrap();
        assert_eq!(layout.expanded_elements, 2);
        assert_eq!(layout.pc_stride, 28);

        let mut console_body = vec![0u8; 70];
        console_body[0..4].copy_from_slice(&[0x03, 1, 0, 0]);
        console_body[4..8].copy_from_slice(&[0x2B, 2, 12, 0]);
        console_body[64] = 2; // count
        console_body[69] = 16; // console stride
        let pc_body = build_pc_layout_body(&console_body).unwrap();
        assert_eq!(pc_body[4], 0x08);
        assert_eq!(pc_body[6], 12);
        assert_eq!(pc_body[69], 20);
    }

    #[test]
    fn inspects_and_patches_havok_sdk_banner() {
        let mut hk = vec![0u8; 0x40];
        hk[0..8].copy_from_slice(&x360_havok::convert::MAGIC);
        hk[0x0C..0x10].copy_from_slice(&4u32.to_le_bytes());
        hk[0x10] = 4;
        hk[0x11] = 1;
        hk[0x14..0x18].copy_from_slice(&3u32.to_le_bytes());
        hk[0x28..0x28 + 14].copy_from_slice(b"Havok-4.6.1-r1");

        let info = inspect_havok_header(&hk).unwrap();
        assert_eq!(info.sdk_version, "Havok-4.6.1-r1");
        assert_eq!(info.file_version, 4);

        let (upgraded, report) = upgrade_havok_packfile(&hk, FB15_HAVOK_SDK, None).unwrap();
        assert!(report.sdk_patched);
        assert_eq!(inspect_havok_header(&upgraded).unwrap().sdk_version, FB15_HAVOK_SDK);
    }

    #[test]
    fn lowers_audio_value_ref_defaults_into_referencing_field() {
        let root = Element {
            tag: "partition".into(),
            kind: dbx::KIND_CONTAINER,
            attributes: vec![("guid".into(), "11111111-2222-3333-4444-555555555555".into())],
            text: String::new(),
            raw: Vec::new(),
            children: vec![
                Element {
                    tag: "instance".into(),
                    kind: dbx::KIND_CONTAINER,
                    attributes: vec![
                        ("guid".into(), "99999999-0000-0000-0000-000000000001".into()),
                        ("type".into(), "Audio.ValueRef".into()),
                    ],
                    text: String::new(),
                    raw: Vec::new(),
                    children: vec![Element {
                        tag: "field".into(),
                        kind: dbx::KIND_TEXT,
                        attributes: vec![("name".into(), "DefaultValue".into())],
                        text: "0.75".into(),
                        raw: Vec::new(),
                        children: Vec::new(),
                    }],
                },
                Element {
                    tag: "instance".into(),
                    kind: dbx::KIND_CONTAINER,
                    attributes: vec![
                        ("guid".into(), "99999999-0000-0000-0000-000000000002".into()),
                        ("type".into(), "SoundGraphData".into()),
                    ],
                    text: String::new(),
                    raw: Vec::new(),
                    children: vec![Element {
                        tag: "field".into(),
                        kind: dbx::KIND_TEXT,
                        attributes: vec![
                            ("name".into(), "Gain".into()),
                            ("ref".into(), "99999999-0000-0000-0000-000000000001".into()),
                        ],
                        text: String::new(),
                        raw: Vec::new(),
                        children: Vec::new(),
                    }],
                },
            ],
        };
        let rules = DbxSchemaRules::bfbc1_to_fb15();
        let raw = DbxFile { roots: vec![root] }.to_bytes();
        let (out, rep) = upgrade_dbx_bytes(&raw, &rules).unwrap();
        assert_eq!(rep.audio_value_refs_lowered, 1);
        let parsed = dbx::parse(&out).unwrap();
        let sg = parsed.instances().next().unwrap();
        let gain = sg.child("Gain").unwrap();
        assert_eq!(gain.attr("ref"), Some("null"));
        assert_eq!(gain.text, "0.75");
    }

    #[test]
    fn unwraps_res_prefixed_bink_movie() {
        let mut res = vec![0u8; 64];
        res[0..4].copy_from_slice(b"RES\x02");
        res.extend_from_slice(b"BIKi\x10\x20\x30\x40");
        let bik = unwrap_bink_movie(&res).unwrap();
        assert_eq!(bik, b"BIKi\x10\x20\x30\x40");
        assert!(unwrap_bink_movie(b"not a movie").is_err());
    }

    #[test]
    fn deduplicates_shared_common_entries() {
        use crate::fbrb::FbrbEntry;
        let mk = |path: &str| FbrbEntry {
            path: path.into(),
            ext: "ITexture".into(),
            flags: 1,
            data: vec![1, 2, 3, 4],
        };
        let arc = FbrbArchive {
            version: 2,
            entries: vec![mk("Characters/ske01.res"), mk("Levels/sp_01/unique.res")],
        };
        let common: BTreeSet<String> = ["characters/ske01.res".into()].into_iter().collect();
        let (deduped, removed) = deduplicate_common_entries(&arc, &common);
        assert_eq!(removed, 1);
        assert_eq!(deduped.entries.len(), 1);
        assert_eq!(deduped.entries[0].path, "Levels/sp_01/unique.res");
    }

    #[test]
    fn prunes_null_items_inside_arrays() {
        let root = Element {
            tag: "partition".into(),
            kind: dbx::KIND_CONTAINER,
            attributes: vec![("guid".into(), "11111111-1111-1111-1111-111111111111".into())],
            text: String::new(),
            raw: Vec::new(),
            children: vec![
                Element {
                    tag: "instance".into(),
                    kind: dbx::KIND_CONTAINER,
                    attributes: vec![
                        ("guid".into(), "aaaaaaaa-0000-0000-0000-000000000001".into()),
                        (
                            "type".into(),
                            "GameSharedResources.LevelReferencedAssetsData".into(),
                        ),
                    ],
                    text: String::new(),
                    raw: Vec::new(),
                    children: Vec::new(),
                },
                Element {
                    tag: "instance".into(),
                    kind: dbx::KIND_CONTAINER,
                    attributes: vec![
                        ("guid".into(), "aaaaaaaa-0000-0000-0000-000000000002".into()),
                        ("type".into(), "SubWorldData".into()),
                    ],
                    text: String::new(),
                    raw: Vec::new(),
                    children: vec![Element {
                        tag: "array".into(),
                        kind: dbx::KIND_CONTAINER,
                        attributes: vec![("name".into(), "Objects".into())],
                        text: String::new(),
                        raw: Vec::new(),
                        children: vec![
                            Element {
                                tag: "item".into(),
                                kind: dbx::KIND_CONTAINER,
                                attributes: vec![(
                                    "ref".into(),
                                    "aaaaaaaa-0000-0000-0000-000000000001".into(),
                                )],
                                text: String::new(),
                                raw: Vec::new(),
                                children: Vec::new(),
                            },
                            Element {
                                tag: "item".into(),
                                kind: dbx::KIND_CONTAINER,
                                attributes: vec![(
                                    "ref".into(),
                                    "aaaaaaaa-0000-0000-0000-000000000002".into(),
                                )],
                                text: String::new(),
                                raw: Vec::new(),
                                children: Vec::new(),
                            },
                        ],
                    }],
                },
            ],
        };
        let rules = DbxSchemaRules::bfbc1_to_fb15();
        let raw = DbxFile { roots: vec![root] }.to_bytes();
        let (out, rep) = upgrade_dbx_bytes(&raw, &rules).unwrap();
        assert_eq!(rep.instances_dropped, 1);
        assert_eq!(rep.dangling_refs_cleared, 1);
        let parsed = dbx::parse(&out).unwrap();
        let sw = parsed.instances().next().unwrap();
        let objs = sw.child("Objects").unwrap();
        assert_eq!(objs.children.len(), 1);
        assert_eq!(
            objs.children[0].attr("ref"),
            Some("aaaaaaaa-0000-0000-0000-000000000002")
        );
    }

    #[test]
    fn upgrades_kynogon_hpd_v1_to_v2() {
        // Build minimal synthetic HPD v1 with 1 sector (1 node, 2 20-byte edges, trailing mesh)
        let mut v1 = vec![0u8; 0x84];
        v1[0..4].copy_from_slice(&0x3FA6_6666u32.to_le_bytes()); // 1.3f
        v1[4..8].copy_from_slice(&1u32.to_le_bytes()); // 1 sector
        v1[0x7C..0x80].copy_from_slice(&36u32.to_le_bytes()); // node_size
        v1[0x80..0x84].copy_from_slice(&20u32.to_le_bytes()); // edge_size

        let mut sec = Vec::new();
        sec.extend_from_slice(&1u32.to_le_bytes()); // 1 node
        sec.extend_from_slice(&2u32.to_le_bytes()); // 2 edges
        sec.extend_from_slice(&[0xAA; 36]); // 1 node (36 B)
        // Edge 0 (20 B)
        sec.extend_from_slice(&0x8000_0001u32.to_le_bytes());
        sec.extend_from_slice(&0u32.to_le_bytes());
        sec.extend_from_slice(&1u32.to_le_bytes());
        sec.extend_from_slice(&[0u8; 8]);
        // Edge 1 (20 B)
        sec.extend_from_slice(&0x8000_0002u32.to_le_bytes());
        sec.extend_from_slice(&1u32.to_le_bytes());
        sec.extend_from_slice(&0u32.to_le_bytes());
        sec.extend_from_slice(&[0u8; 8]);
        // Trailing mesh marker
        sec.extend_from_slice(b"Kynogon InPlace Mesh\x02\x00\x00\x00");

        let mut entry = [0u8; 48];
        entry[0..4].copy_from_slice(&42u32.to_le_bytes()); // sector id
        entry[0x20..0x24].copy_from_slice(&(sec.len() as u32).to_le_bytes());
        entry[0x24..0x28].copy_from_slice(&0u32.to_le_bytes());
        v1.extend_from_slice(&entry);
        v1.extend_from_slice(&[0x00, 0x00, 0x80, 0x3F, 0, 0, 0, 0]); // 8B gap
        v1.extend_from_slice(&sec);

        let (v2, rep) = upgrade_kynogon_hpd(&v1).unwrap();
        assert_eq!(rep.sectors, 1);
        assert_eq!(rep.nodes, 1);
        assert_eq!(rep.edges, 2);
        assert!(v2.starts_with(b"Kynogon HPD\0"));
        assert_eq!(u32::from_le_bytes(v2[0x0C..0x10].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(v2[0x14..0x18].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(v2[0x8C..0x90].try_into().unwrap()), 36);
        assert_eq!(u32::from_le_bytes(v2[0x90..0x94].try_into().unwrap()), 12);
        // Sector payload shrunk by 2 * (20 - 12) = 16 bytes
        let new_sz = u32::from_le_bytes(v2[0x9C + 0x20..0x9C + 0x24].try_into().unwrap()) as usize;
        assert_eq!(new_sz, sec.len() - 16);
        let mesh_pos = 0x9C + 48 + 8 + 36 + 2 * 12;
        assert_eq!(&v2[mesh_pos..mesh_pos + 20], b"Kynogon InPlace Mesh");
    }

    #[test]
    fn builds_pathdata_sector_offsets_dbx() {
        let raw = build_sector_offsets_dbx("SP_01_GreenAcres", "Infantry", 0, 0);
        let parsed = dbx::parse(&raw).unwrap();
        let inst = parsed.instances().next().unwrap();
        assert_eq!(inst.attr("type"), Some("GameSharedResources.PathdataSectorOffsets"));
        assert_eq!(
            inst.child("Name").map(|e| e.text.as_str()),
            Some("Levels/SP_01_GreenAcres/precomputed/Infantry/SectorOffsets")
        );
        assert_eq!(inst.child("XOffset").map(|e| e.text.as_str()), Some("0"));
        assert_eq!(inst.child("ZOffset").map(|e| e.text.as_str()), Some("0"));
    }
}
