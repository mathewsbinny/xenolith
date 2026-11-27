//! Frostbite 1.0 (BFBC1) -> Frostbite 1.5 DBX schema upgrader and `PathdataSectorOffsets` builder.
#![deny(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use crate::dbx::{self, DbxFile, Element};

use super::Bfbc1Error;
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
