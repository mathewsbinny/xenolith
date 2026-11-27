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

mod dbx;
mod dedup;
mod havok;
mod hpd;
mod terrain;

use thiserror::Error;

use crate::dbx::DbxError;

pub use self::dbx::{
    DbxSchemaRules, DbxUpgradeReport, DefaultField, build_sector_offsets_dbx, upgrade_dbx, upgrade_dbx_bytes,
};
pub use self::dedup::{deduplicate_common_entries, deduplicate_common_entries_guid_aware};
pub use self::havok::{
    HavokExeData, HavokHeaderInfo, HavokPortContext, HavokUpgradeReport, inspect_havok_header, load_havok_port_data,
    port_havok_461_to_550_pc, upgrade_havok_packfile,
};
pub use self::hpd::{HpdUpgradeReport, upgrade_kynogon_hpd};
pub use self::terrain::{
    PcVertexLayout, TerrainUpgradeReport, build_pc_layout_body, compute_pc_vertex_layout,
    downsample_sub_heightfield_256_to_128, upgrade_heightfield, upgrade_mesh_header,
};

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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use std::collections::BTreeSet;

    use crate::dbx::{self, DbxFile, Element};
    use crate::fbrb::FbrbArchive;
    use crate::terrain;

    use super::terrain::{read_u16_le, read_u32_le};
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
                        ("type".into(), "GameSharedResources.LevelReferencedAssetsData".into()),
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
                                attributes: vec![("ref".into(), "aaaaaaaa-0000-0000-0000-000000000001".into())],
                                text: String::new(),
                                raw: Vec::new(),
                                children: Vec::new(),
                            },
                            Element {
                                tag: "item".into(),
                                kind: dbx::KIND_CONTAINER,
                                attributes: vec![("ref".into(), "aaaaaaaa-0000-0000-0000-000000000002".into())],
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
