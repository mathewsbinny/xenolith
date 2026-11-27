//! End-to-end pure-Rust Frostbite `FbRB` level/asset bundle porting orchestrator.
//!
//! Walks an Xbox 360 [`FbrbArchive`], dispatches each entry in parallel via Rayon to the
//! appropriate `x360-frostbite`, `x360-granny`, or `x360-havok` converter, preserves
//! passthrough entries (such as `{binary}` DBX partitions and tombstones) byte-for-byte,
//! and repacks the converted entries into a PC-ready `FbRB` bundle alongside a structured
//! [`BundlePortReport`].
#![deny(unsafe_code)]

use std::collections::{BTreeMap, HashMap};

use rayon::prelude::*;

use crate::fbrb::{FbrbArchive, FbrbEntry, FbrbError};
use crate::{aimanim, apt, bfbc1, dbx, meshconv, meshdata, shader_db, sns, terrain, texture, wavport};

/// Configuration options for [`port_archive`] and [`port_fbrb_bytes`].
#[allow(clippy::struct_excessive_bools)]
pub struct PortOptions<'a> {
    /// Source game tag forwarded to [`wavport::convert`] (e.g. `"bf1943"` or `"bfbc1"`).
    pub source_game: &'a str,
    /// Passed to [`terrain::heightfield`] (`false` for BF1943/BFBC1, `true` for BFBC2).
    pub bfbc2_heightfield: bool,
    /// Whether to apply BFBC1 (Frostbite 1.0 -> 1.5) DBX, heightfield, and Havok upgrades.
    pub bfbc1_upgrade: bool,
    /// Whether to gzip-compress the output `FbRB` payload.
    pub compress_output: bool,
    /// Whether to structurally convert EA APT `BIGF` (`SwfMovie`) entries from BE to LE.
    pub convert_apt: bool,
    /// Whether to strip `SPDB`/`SDBG`/`PRIV` debug chunks when repacking `DXBC` `ShaderDatabase` entries.
    pub strip_shader_debug: bool,
    /// Optional Havok `hkClass` reflection table for same-version `hkPackfile` endian conversion.
    pub havok_classes: Option<&'a x360_havok::convert::Classes>,
    /// Optional Havok `4.6.1 -> 5.5.0` structural porting context (BFBC1 / BF1943 physics).
    pub havok_port: Option<&'a bfbc1::HavokPortContext<'a>>,
    /// Optional `ske01` bone-index remap (`old_index -> new_index`) threaded consistently
    /// through Granny2, `AimAnimation`, skinned `MeshData`, and DBX `BoneId` fields.
    pub bone_map: Option<&'a HashMap<u32, u32>>,
    /// Optional external streaming audio archive (e.g. BFBC1 `streaming_sounds-00.fbrb`)
    /// used to resolve header-only `Wave` stubs (`<stem>_wave.res` -> `<stem>_waveDataSns_{0,1,2}.res`).
    pub streaming_sounds: Option<&'a FbrbArchive>,
}

impl Default for PortOptions<'_> {
    fn default() -> Self {
        Self {
            source_game: "bf1943",
            bfbc2_heightfield: false,
            bfbc1_upgrade: false,
            compress_output: true,
            convert_apt: true,
            strip_shader_debug: false,
            havok_classes: None,
            havok_port: None,
            bone_map: None,
            streaming_sounds: None,
        }
    }
}

/// Summary report produced by [`port_archive`] and [`port_fbrb_bytes`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BundlePortReport {
    /// Total number of entries processed.
    pub total_entries: usize,
    /// Number of entries converted from Xbox 360 to PC format.
    pub converted: usize,
    /// Number of entries passed through unchanged (DBX, tombstones, non-console assets).
    pub passthrough: usize,
    /// Number of recognized console entries that could not be converted (original bytes kept).
    pub skipped: usize,
    /// Count of converted entries grouped by resource type (deterministic sorted order).
    pub by_kind: BTreeMap<String, usize>,
    /// Non-fatal `(entry_path, reason)` warnings for skipped entries.
    pub warnings: Vec<(String, String)>,
}

enum EntryStatus {
    Converted(&'static str),
    Passthrough,
    Skipped(String),
}

type LayoutMap = BTreeMap<Vec<u8>, (Vec<u8>, Vec<u8>)>;
type StreamMap<'a> = BTreeMap<String, &'a [u8]>;

fn lookup_wave_streams<'a>(path: &str, streams: &StreamMap<'a>) -> [Option<&'a [u8]>; 3] {
    let lower = path.replace('\\', "/").to_ascii_lowercase();
    let Some(stem) = lower.strip_suffix("_wave.res") else {
        return [None, None, None];
    };
    let s0 = streams.get(&format!("{stem}_wavedatasns_0.res")).copied();
    let s1 = streams
        .get(&format!("{stem}_wavedatasns_1.res"))
        .or_else(|| streams.get(&format!("{stem}_wavedatasns.res")))
        .copied();
    let s2 = streams.get(&format!("{stem}_wavedatasns_2.res")).copied();
    [s0, s1, s2]
}

#[allow(clippy::too_many_lines)]
fn convert_one(
    entry: &FbrbEntry,
    opts: &PortOptions<'_>,
    layouts: &LayoutMap,
    streams: &StreamMap<'_>,
) -> (FbrbEntry, EntryStatus) {
    if entry.data.is_empty() || entry.flags == 0 {
        return (entry.clone(), EntryStatus::Passthrough);
    }

    let mut out = entry.clone();
    match entry.ext.as_str() {
        "XenonTexture" => match texture::convert(&entry.data) {
            Ok(pc) => {
                "ITexture".clone_into(&mut out.ext);
                out.data = pc;
                (out, EntryStatus::Converted("XenonTexture -> ITexture"))
            }
            Err(e) => (out, EntryStatus::Skipped(format!("XenonTexture: {e}"))),
        },
        "TerrainHeightfield" => {
            if opts.bfbc1_upgrade {
                match bfbc1::upgrade_heightfield(&entry.data) {
                    Ok((pc, _)) => {
                        out.data = pc;
                        (out, EntryStatus::Converted("TerrainHeightfield (FB1.0 -> 1.5)"))
                    }
                    Err(e) => (out, EntryStatus::Skipped(format!("TerrainHeightfield: {e}"))),
                }
            } else {
                match terrain::heightfield(&entry.data, opts.bfbc2_heightfield) {
                    Ok(pc) => {
                        out.data = pc;
                        (out, EntryStatus::Converted("TerrainHeightfield"))
                    }
                    Err(e) => (out, EntryStatus::Skipped(format!("TerrainHeightfield: {e:?}"))),
                }
            }
        }
        "WaterMesh" => match terrain::water_mesh(&entry.data) {
            Ok(pc) => {
                out.data = pc;
                (out, EntryStatus::Converted("WaterMesh"))
            }
            Err(e) => (out, EntryStatus::Skipped(format!("WaterMesh: {e:?}"))),
        },
        "TerrainMaterialMap" => match terrain::material_map(&entry.data) {
            Ok(pc) => {
                out.data = pc;
                (out, EntryStatus::Converted("TerrainMaterialMap"))
            }
            Err(e) => (out, EntryStatus::Skipped(format!("TerrainMaterialMap: {e:?}"))),
        },
        "VisualTerrain" => match terrain::visual_terrain(&entry.data) {
            Ok(pc) => {
                out.data = pc;
                (out, EntryStatus::Converted("VisualTerrain"))
            }
            Err(e) => (out, EntryStatus::Skipped(format!("VisualTerrain: {e:?}"))),
        },
        "VisualWater" => match terrain::visual_water(&entry.data) {
            Ok(pc) => {
                out.data = pc;
                (out, EntryStatus::Converted("VisualWater"))
            }
            Err(e) => (out, EntryStatus::Skipped(format!("VisualWater: {e:?}"))),
        },
        "SootMesh" => match terrain::soot_mesh(&entry.data) {
            Ok(pc) => {
                out.data = pc;
                (out, EntryStatus::Converted("SootMesh"))
            }
            Err(e) => (out, EntryStatus::Skipped(format!("SootMesh: {e:?}"))),
        },
        "Wave" => {
            let sns_slices = lookup_wave_streams(&entry.path, streams);
            if let Some(pc) = wavport::convert(&entry.data, opts.source_game, &sns_slices, 0) {
                out.data = pc;
                (out, EntryStatus::Converted("Wave -> EA-XAS"))
            } else if let Some(pc_stub) = wavport::convert_streaming_stub(&entry.data, opts.source_game, 0) {
                out.data = pc_stub;
                (out, EntryStatus::Converted("Wave -> PC Streaming Header"))
            } else {
                (
                    out,
                    EntryStatus::Skipped("Wave: unsupported codec or malformed".to_owned()),
                )
            }
        }
        "Sns" | "WaveDataSns" if entry.data.starts_with(b"\x48\x00\x00\x0c") => {
            match sns::repack_sps_bank(&entry.data) {
                Ok((pc, rep)) if rep.converted > 0 => {
                    out.data = pc;
                    (out, EntryStatus::Converted("SNS Bank -> EA-XAS"))
                }
                Ok(_) => (out, EntryStatus::Passthrough),
                Err(e) => (out, EntryStatus::Skipped(format!("SNS: {e}"))),
            }
        }
        "ShaderDatabase" => {
            let repack_opts = shader_db::RepackOptions {
                strip_debug: opts.strip_shader_debug,
                replacements: BTreeMap::new(),
            };
            match shader_db::repack_dxbc_database(&entry.data, &repack_opts) {
                Ok((pc, rep)) if rep.total_dxbc > 0 && (rep.replaced > 0 || rep.stripped_chunks > 0) => {
                    out.data = pc;
                    (out, EntryStatus::Converted("ShaderDatabase (DXBC)"))
                }
                Ok(_) => (out, EntryStatus::Passthrough),
                Err(e) => (out, EntryStatus::Skipped(format!("ShaderDatabase: {e}"))),
            }
        }
        "AimAnimation" => {
            let mapped = if let Some(bm) = opts.bone_map {
                let f = |b: u8| bm.get(&u32::from(b)).and_then(|&v| u8::try_from(v).ok());
                aimanim::to_pc(&entry.data, Some(&f))
            } else {
                aimanim::to_pc(&entry.data, None)
            };
            match mapped {
                Ok(pc) => {
                    out.data = pc;
                    (out, EntryStatus::Converted("AimAnimation"))
                }
                Err(e) => (out, EntryStatus::Skipped(format!("AimAnimation: {e}"))),
            }
        }
        "SwfMovie" | "Apt" if opts.convert_apt && entry.data.starts_with(b"BIGF") => {
            match apt::convert_bigf_be_to_le(&entry.data) {
                Ok(pc) => {
                    out.data = pc;
                    (out, EntryStatus::Converted("SwfMovie (APT BE -> LE)"))
                }
                Err(e) => (out, EntryStatus::Skipped(format!("SwfMovie: {e}"))),
            }
        }
        "GrannyModel" | "GrannyAnimation" | "DeltaAnimation" => {
            match x360_granny::convert::to_pc(&entry.data, opts.bone_map) {
                Ok(pc) => {
                    out.data = pc;
                    (out, EntryStatus::Converted("Granny2 -> PC"))
                }
                Err(e) => (out, EntryStatus::Skipped(format!("Granny2: {e}"))),
            }
        }
        "WeatherSystem" if entry.data.len() == 4 => {
            out.data = 0x48u32.to_le_bytes().to_vec();
            (out, EntryStatus::Converted("WeatherSystem -> PC v72"))
        }
        "AnimTreeInfo" if entry.data.len() >= 20 => {
            if let Some(pc) = convert_anim_tree_info(&entry.data) {
                out.data = pc;
                (out, EntryStatus::Converted("AnimTreeInfo v6 -> PC"))
            } else {
                (
                    out,
                    EntryStatus::Skipped("AnimTreeInfo: unsupported version/layout".to_owned()),
                )
            }
        }
        "IrradianceVolume" if entry.data.len() >= 36 => {
            out.data = convert_irradiance_volume(&entry.data);
            (out, EntryStatus::Converted("IrradianceVolume -> PC"))
        }
        "OccluderMesh" if entry.data.len() >= 44 => {
            out.data = convert_occluder_mesh(&entry.data);
            (out, EntryStatus::Converted("OccluderMesh -> PC"))
        }
        "RigidMeshSet" | "CompositeMeshSet" | "SkinnedMeshSet" | "TreeMeshSet" => {
            if let Some(kind) = meshdata::Kind::of(&entry.ext) {
                let flag_hint = match kind {
                    meshdata::Kind::Rigid | meshdata::Kind::Skinned => Some(false),
                    meshdata::Kind::Composite | meshdata::Kind::Tree => Some(true),
                };
                let parsed = meshdata::parse_set(&entry.data, kind, true, flag_hint)
                    .or_else(|_| meshdata::parse_set(&entry.data, kind, true, None));
                match parsed.and_then(|set| meshdata::write_set(&set, kind)) {
                    Ok(pc) => {
                        out.data = pc;
                        (out, EntryStatus::Converted("MeshSet -> PC"))
                    }
                    Err(_) => (
                        out,
                        EntryStatus::Skipped(format!("{}: unsupported MeshSet layout", entry.ext)),
                    ),
                }
            } else {
                (out, EntryStatus::Passthrough)
            }
        }
        "MeshData" => {
            if let Ok(hdr) = meshdata::parse_mesh(&entry.data, true) {
                let norm_path = entry.path.replace('\\', "/").to_ascii_lowercase();
                let lod_pref = norm_path.strip_suffix("_data.res").unwrap_or(&norm_path);
                let mut pairs: Vec<(&[u8], &[u8])> = Vec::with_capacity(hdr.parts.len());
                let mut key_buf: Vec<u8> = Vec::with_capacity(128);
                let mut all_found = !hdr.parts.is_empty();
                for part in &hdr.parts {
                    let pname = part.name.to_ascii_lowercase();
                    let ptail = pname.rsplit(|&b| b == b'|').next().unwrap_or(&pname);
                    let ptail_base = ptail.strip_suffix(b"_zonly").unwrap_or(ptail);
                    let pname_base = pname.strip_suffix(b"_zonly").unwrap_or(&pname);
                    let found = lookup_mesh_layout(layouts, &mut key_buf, lod_pref.as_bytes(), ptail, b"/standard")
                        .or_else(|| lookup_mesh_layout(layouts, &mut key_buf, lod_pref.as_bytes(), ptail, b""))
                        .or_else(|| {
                            lookup_mesh_layout(layouts, &mut key_buf, lod_pref.as_bytes(), &pname, b"/standard")
                        })
                        .or_else(|| lookup_mesh_layout(layouts, &mut key_buf, lod_pref.as_bytes(), &pname, b""))
                        .or_else(|| lookup_mesh_layout(layouts, &mut key_buf, lod_pref.as_bytes(), ptail_base, b""))
                        .or_else(|| lookup_mesh_layout(layouts, &mut key_buf, lod_pref.as_bytes(), pname_base, b""))
                        .or_else(|| layouts.get(&pname))
                        .or_else(|| layouts.get(&part.name));
                    if let Some((c_lay, p_lay)) = found {
                        pairs.push((c_lay.as_slice(), p_lay.as_slice()));
                    } else {
                        all_found = false;
                        break;
                    }
                }
                if all_found {
                    match meshconv::convert(&entry.data, &pairs, opts.bone_map) {
                        Ok(pc) => {
                            out.data = pc;
                            (out, EntryStatus::Converted("MeshData -> PC"))
                        }
                        Err(e) => (out, EntryStatus::Skipped(format!("MeshData: {e:?}"))),
                    }
                } else {
                    (
                        out,
                        EntryStatus::Skipped("MeshData: missing console vertex layout in ShaderDatabase".to_owned()),
                    )
                }
            } else {
                (out, EntryStatus::Skipped("MeshData: invalid BE header".to_owned()))
            }
        }
        _ if x360_havok::port::find_magic(&entry.data).is_some() => {
            if let Some(ctx) = opts.havok_port
                && entry.data.windows(11).any(|w| w == b"Havok-4.6.1")
            {
                match bfbc1::port_havok_461_to_550_pc(&entry.data, ctx) {
                    Ok(pc) => {
                        out.data = pc;
                        (out, EntryStatus::Converted("Havok 4.6.1 -> 5.5.0 PC"))
                    }
                    Err(e) => (out, EntryStatus::Skipped(format!("Havok 4.6.1: {e}"))),
                }
            } else if opts.bfbc1_upgrade && entry.data.starts_with(&x360_havok::convert::MAGIC) {
                match bfbc1::upgrade_havok_packfile(&entry.data, bfbc1::FB15_HAVOK_SDK, opts.havok_classes) {
                    Ok((pc, rep)) if rep.endian_converted || rep.sdk_patched => {
                        out.data = pc;
                        (out, EntryStatus::Converted("Havok hkPackfile (FB1.0 -> 1.5)"))
                    }
                    Ok(_) => (out, EntryStatus::Passthrough),
                    Err(e) => (out, EntryStatus::Skipped(format!("Havok: {e}"))),
                }
            } else if let Some(classes) = opts.havok_classes
                && let Some(off) = x360_havok::port::find_magic(&entry.data)
            {
                match x360_havok::convert::convert_packfile(&entry.data, off, classes) {
                    Ok(pc_pack) => {
                        if off > 0 {
                            match x360_havok::convert::convert_wrapper(&entry.data, off) {
                                Ok(mut pc_wrap) => {
                                    let pack_slice = pc_pack.get(off..).unwrap_or(&pc_pack);
                                    pc_wrap.extend_from_slice(pack_slice);
                                    out.data = pc_wrap;
                                    (out, EntryStatus::Converted("Havok hkPackfile"))
                                }
                                Err(e) => (out, EntryStatus::Skipped(format!("Havok wrapper: {e:?}"))),
                            }
                        } else {
                            out.data = pc_pack;
                            (out, EntryStatus::Converted("Havok hkPackfile"))
                        }
                    }
                    Err(e) => (out, EntryStatus::Skipped(format!("Havok: {e}"))),
                }
            } else {
                (
                    out,
                    EntryStatus::Skipped("Havok hkPackfile: no hkClass/HavokPortContext supplied".to_owned()),
                )
            }
        }
        _ if opts.bfbc1_upgrade && dbx::is_binary(&entry.data) => {
            let mut rules = bfbc1::DbxSchemaRules::bfbc1_to_fb15();
            if let Some(bm) = opts.bone_map {
                #[allow(clippy::iter_over_hash_type, reason = "inserting into deterministic BTreeMap")]
                for (&old, &new) in bm {
                    rules.bone_id_map.insert(old as i32, new as i32);
                }
            }
            match bfbc1::upgrade_dbx_bytes(&entry.data, &rules) {
                Ok((upgraded, rep)) if rep.changed() => {
                    out.data = upgraded;
                    (out, EntryStatus::Converted("DBX Schema (FB1.0 -> 1.5)"))
                }
                Ok(_) => (out, EntryStatus::Passthrough),
                Err(e) => (out, EntryStatus::Skipped(format!("DBX: {e}"))),
            }
        }
        _ => (out, EntryStatus::Passthrough),
    }
}

/// Converts all recognized Xbox 360 entries inside `arc` in parallel, preserving entry order.
#[must_use]
pub fn port_archive(arc: &FbrbArchive, opts: &PortOptions<'_>) -> (FbrbArchive, BundlePortReport) {
    let mut layouts = LayoutMap::new();
    let mut streams = StreamMap::new();
    for entry in &arc.entries {
        if (entry.ext == "ShaderDatabase"
            || entry.ext == "Dx9ShaderDatabase"
            || entry.ext == "Dx10ShaderDatabase"
            || entry.ext == "Dx11ShaderDatabase")
            && !entry.data.is_empty()
        {
            for (name, console_body) in meshconv::console_layouts(&entry.data) {
                if let Some(pc_body) = bfbc1::build_pc_layout_body(&console_body) {
                    let lower = name.to_ascii_lowercase();
                    if let Some(slash) = lower.iter().rposition(|&b| b == b'/')
                        && let Some(parent) = lower.get(..slash)
                    {
                        layouts
                            .entry(parent.to_vec())
                            .or_insert_with(|| (console_body.clone(), pc_body.clone()));
                    }
                    layouts.entry(lower).or_insert((console_body, pc_body));
                }
            }
        }
        if !entry.data.is_empty() && entry.path.to_ascii_lowercase().contains("wavedatasns") {
            let key = entry.path.replace('\\', "/").to_ascii_lowercase();
            streams.insert(key, entry.data.as_slice());
        }
    }
    if let Some(ext_arc) = opts.streaming_sounds {
        for entry in &ext_arc.entries {
            if !entry.data.is_empty() && entry.path.to_ascii_lowercase().contains("wavedatasns") {
                let key = entry.path.replace('\\', "/").to_ascii_lowercase();
                streams.entry(key).or_insert(entry.data.as_slice());
            }
        }
    }

    let results: Vec<(FbrbEntry, EntryStatus)> = arc
        .entries
        .par_iter()
        .map(|entry| convert_one(entry, opts, &layouts, &streams))
        .collect();

    let mut report = BundlePortReport {
        total_entries: results.len(),
        ..BundlePortReport::default()
    };
    let mut entries = Vec::with_capacity(results.len());
    for (entry, status) in results {
        match status {
            EntryStatus::Converted(kind) => {
                report.converted += 1;
                *report.by_kind.entry(kind.to_owned()).or_insert(0) += 1;
            }
            EntryStatus::Passthrough => {
                report.passthrough += 1;
            }
            EntryStatus::Skipped(reason) => {
                report.skipped += 1;
                report.warnings.push((entry.path.clone(), reason));
            }
        }
        entries.push(entry);
    }

    let out_arc = FbrbArchive {
        version: arc.version,
        entries,
    };
    (out_arc, report)
}

/// Parses raw console `FbRB` bytes, ports all supported entries to PC format, and repacks the
/// resulting `FbRB` archive.
///
/// # Errors
/// Returns [`FbrbError`] if parsing the input archive or compressing the output archive fails.
pub fn port_fbrb_bytes(raw: &[u8], opts: &PortOptions<'_>) -> Result<(Vec<u8>, BundlePortReport), FbrbError> {
    let arc = FbrbArchive::parse(raw)?;
    let (ported, report) = port_archive(&arc, opts);
    let bytes = ported.to_bytes(opts.compress_output)?;
    Ok((bytes, report))
}

fn read_be_u32(d: &[u8], off: usize) -> Option<u32> {
    let b: [u8; 4] = d.get(off..off + 4)?.try_into().ok()?;
    Some(u32::from_be_bytes(b))
}

fn read_be_u16(d: &[u8], off: usize) -> Option<u16> {
    let b: [u8; 2] = d.get(off..off + 2)?.try_into().ok()?;
    Some(u16::from_be_bytes(b))
}

fn swap_u32_be_to_le_in_place(buf: &mut [u8]) {
    for chunk in buf.as_chunks_mut::<4>().0 {
        chunk.reverse();
    }
}

fn swap_u16_be_to_le_in_place(buf: &mut [u8]) {
    for chunk in buf.as_chunks_mut::<2>().0 {
        chunk.reverse();
    }
}

fn convert_anim_tree_info(d: &[u8]) -> Option<Vec<u8>> {
    let ver = read_be_u32(d, 0)?;
    let base = read_be_u32(d, 4)?;
    let s0 = read_be_u32(d, 8)? as usize;
    let s1 = read_be_u32(d, 12)? as usize;
    let s2 = read_be_u32(d, 16)? as usize;
    if ver != 6 || 20usize.saturating_add(s0).saturating_add(s1).saturating_add(s2) != d.len() {
        return None;
    }
    let mut pc = d.to_vec();
    swap_u32_be_to_le_in_place(&mut pc);

    // Walk Section 0 node pointer table (`0x18`: node_count, `0x24`: node_ptrs[])
    let sec0_end = 20 + s0;
    if s0 >= 16 && d.len() >= 0x24 {
        let node_cnt = read_be_u32(d, 0x18).unwrap_or(0) as usize;
        let mut node_offs: Vec<usize> = Vec::with_capacity(node_cnt.min(sec0_end / 4));
        for i in 0..node_cnt {
            let po = 0x24 + i * 4;
            if po + 4 > sec0_end {
                break;
            }
            if let Some(nptr) = read_be_u32(d, po)
                && nptr >= base
            {
                let noff = 0x14 + (nptr - base) as usize;
                if noff < sec0_end {
                    node_offs.push(noff);
                }
            }
        }
        for (idx, &noff) in node_offs.iter().enumerate() {
            // Node +0x14: 3 x u16 + 2 x u8
            if noff + 0x1C <= sec0_end {
                for k in 0..3 {
                    let o = noff + 0x14 + k * 2;
                    if let (Some(v), Some(dst)) = (read_be_u16(d, o), pc.get_mut(o..o + 2)) {
                        dst.copy_from_slice(&v.to_le_bytes());
                    }
                }
                if let (Some(src), Some(dst)) = (d.get(noff + 0x1A..noff + 0x1C), pc.get_mut(noff + 0x1A..noff + 0x1C))
                {
                    dst.copy_from_slice(src);
                }
            }
            // Optional u16 child-index tail between +0x3C and next node (when node size > 0x40)
            let next_off = node_offs.get(idx + 1).copied().unwrap_or(sec0_end);
            if next_off > noff + 0x40
                && next_off <= sec0_end
                && next_off - noff <= 0x80
                && let (Some(src), Some(dst)) = (d.get(noff + 0x3C..next_off), pc.get_mut(noff + 0x3C..next_off))
            {
                dst.copy_from_slice(src);
                swap_u16_be_to_le_in_place(dst);
            }
        }
    }
    Some(pc)
}

fn convert_irradiance_volume(d: &[u8]) -> Vec<u8> {
    // 6 f32 AABB + 3 u32 grid dims (BE -> LE), followed by dx*dy*dz u8 voxels
    let mut pc = d.to_vec();
    if let Some(hdr) = pc.get_mut(..36) {
        swap_u32_be_to_le_in_place(hdr);
    }
    pc
}

fn convert_occluder_mesh(d: &[u8]) -> Vec<u8> {
    // 0x00..0x18: 6 f32 AABB; 0x18: u32 payload_size; 0x1C: u16 v_cnt, u16 i_cnt;
    // 0x20..0x2C: u32 vert_off, idx_off, edge_off (relative to 0x1C);
    // 0x1C+edge_off..0x1C+idx_off: u32/f32 words; 0x1C+idx_off..end: u16 indices.
    let mut pc = d.to_vec();
    if let Some(aabb_hdr) = pc.get_mut(..28) {
        swap_u32_be_to_le_in_place(aabb_hdr);
    }
    if let Some(counts) = pc.get_mut(28..32) {
        swap_u16_be_to_le_in_place(counts);
    }
    let idx_off = read_be_u32(d, 36).unwrap_or(0) as usize;
    if let Some(offs) = pc.get_mut(32..44) {
        swap_u32_be_to_le_in_place(offs);
    }
    let idx_abs = 28usize.saturating_add(idx_off).min(d.len());
    if let Some(words) = pc.get_mut(44..idx_abs) {
        swap_u32_be_to_le_in_place(words);
    }
    if let Some(indices) = pc.get_mut(idx_abs..) {
        swap_u16_be_to_le_in_place(indices);
    }
    pc
}

fn lookup_mesh_layout<'a>(
    layouts: &'a BTreeMap<Vec<u8>, (Vec<u8>, Vec<u8>)>,
    buf: &mut Vec<u8>,
    prefix: &[u8],
    name: &[u8],
    suffix: &[u8],
) -> Option<&'a (Vec<u8>, Vec<u8>)> {
    buf.clear();
    buf.extend_from_slice(prefix);
    buf.push(b'/');
    buf.extend_from_slice(name);
    buf.extend_from_slice(suffix);
    layouts.get(buf.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbx::{DbxFile, Element, KIND_CONTAINER};

    #[test]
    fn ports_mixed_fbrb_bundle_end_to_end() {
        // 1. Minimal 8x8 L8 XenonTexture
        let mut tex = Vec::new();
        for v in [3u32, 0, 10, 0, 8, 8, 0, 1] {
            tex.extend(v.to_be_bytes());
        }
        tex.extend([0u8; 60]);
        tex.extend(vec![7u8; 4096]);

        // 2. Binary DBX passthrough
        let dbx_bytes = DbxFile {
            roots: vec![Element {
                tag: "partition".to_owned(),
                kind: KIND_CONTAINER,
                attributes: vec![("guid".to_owned(), "TEST".to_owned())],
                text: String::new(),
                raw: Vec::new(),
                children: Vec::new(),
            }],
        }
        .to_bytes();

        // 3. Deleted tombstone entry
        let arc = FbrbArchive {
            version: 2,
            entries: vec![
                FbrbEntry {
                    path: "textures/test.res".to_owned(),
                    ext: "XenonTexture".to_owned(),
                    flags: 0x0001_0000,
                    data: tex,
                },
                FbrbEntry {
                    path: "levels/test.dbx".to_owned(),
                    ext: "<non-resource>".to_owned(),
                    flags: 0x0001_0000,
                    data: dbx_bytes.clone(),
                },
                FbrbEntry {
                    path: "deleted/slot".to_owned(),
                    ext: "<non-resource>".to_owned(),
                    flags: 0,
                    data: Vec::new(),
                },
            ],
        };

        let raw_in = arc.to_bytes(false).ok();
        assert!(raw_in.is_some());
        if let Some(raw) = raw_in {
            let res = port_fbrb_bytes(&raw, &PortOptions::default()).ok();
            assert!(res.is_some());
            if let Some((out_bytes, report)) = res {
                assert_eq!(report.total_entries, 3);
                assert_eq!(report.converted, 1);
                assert_eq!(report.passthrough, 2);
                assert_eq!(report.skipped, 0);
                assert_eq!(report.by_kind.get("XenonTexture -> ITexture"), Some(&1));

                let reparsed = FbrbArchive::parse(&out_bytes).ok();
                assert!(reparsed.is_some());
                if let Some(p) = reparsed {
                    assert_eq!(p.entries.first().map(|e| e.ext.as_str()), Some("ITexture"));
                    assert_eq!(p.entries.first().map(|e| e.data.len()), Some(texture::HEADER + 64));
                    assert_eq!(p.entries.get(1).map(|e| e.data.as_slice()), Some(dbx_bytes.as_slice()));
                }
            }
        }
    }
}
