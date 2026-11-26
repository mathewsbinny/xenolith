//! Headless CLI subcommands for scripted batch porting via `xenolith.exe`.
//!
//! When `xenolith.exe` is launched with no arguments, the desktop GUI starts.
//! Passing a subcommand runs headlessly and exits with code `0` on success or `1` on error.
#![deny(unsafe_code)]

use std::fs;
use std::io::Write as _;
use std::path::Path;

use crate::pipeline::{EngineProfile, inspect_file};

const USAGE: &str = "\
Xenolith — Xbox 360 -> PC Porting Toolkit

Usage:
  xenolith                                     Launch desktop GUI
  xenolith inspect <file>                      Inspect asset/container metadata
  xenolith port-fbrb [--bfbc1] [--streaming-sounds <sns.fbrb>] [--havok-exe <BFBC2Game.exe>] [--havok-template <tpl.res>] <in> <out>
                                               Port Frostbite FbRB bundle -> PC
  xenolith unpack-fbrb <in> <out_dir>          Unpack Frostbite FbRB bundle
  xenolith repack-sns <in> <out>               Repack SPS v1 .sns audio bank -> PC EA-XAS
  xenolith port-streaming-sounds --waves <lvl.fbrb> <in_sns.fbrb> <out_sns.fbrb>
                                               Port BFBC1/BF1943 streaming_sounds-00.fbrb -> PC EA-XAS SPS v1
  xenolith repack-shaderdb <in> <out>          Strip debug & recompute DXBC checksums
  xenolith compile-xenos-shaders [--profile dx10|dx11|dx9] <in> <out_dir>
                                               Translate & compile Xenos ShaderDB via fxc.exe
  xenolith upgrade-dbx <in> <out>              Upgrade BFBC1 binary DBX -> Frostbite 1.5
  xenolith dbx-to-xml <in> <out>               Export binary DBX partition -> XML
  xenolith inspect-apt <in.res>                List FSCommand, extern.*, and console prompt symbols
  xenolith unwrap-bink <in.res> <out.bik>      Strip RES\\x02 wrapper from BFBC1 movie -> .bik
  xenolith upgrade-hpd <in.hpd> <out.hpd>      Upgrade BFBC1 Kynogon HPD v1 navmesh -> FB 1.5 v2
  xenolith make-sector-offsets <lvl> <kind> <pguid> <iguid> <x> <z> <out.dbx>
                                               Generate Frostbite 1.5 PathdataSectorOffsets DBX
  xenolith register-level <startup.fbrb> <level> [donor]
                                               Clone donor LevelDescription + register in LevelListQuery
  xenolith make-sector-offsets <lvl> <layer> <x> <z> <out.dbx>
                                               Build FB 1.5 PathdataSectorOffsets binary DBX
  xenolith dedup-fbrb <lvl> <common> <out>     Remove shared common entries from BFBC1 level FbRB
  xenolith export-atlas-sprite <fbrb> <prefix> <id> <out.png>
                                               Crop a UI.UITextureAtlasInfo sprite to RGBA PNG
  xenolith export-bf1943-menu <stfs> <out_dir> Export BF1943 menu PNGs, fonts, SFX, and Hub logo
";

fn outln(msg: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{msg}");
}

fn errln(msg: &str) {
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "error: {msg}");
}

/// Runs headless CLI mode for `args` (excluding `argv[0]`). Returns process exit code.
#[must_use]
pub fn run(args: &[String]) -> i32 {
    match dispatch(args) {
        Ok(()) => 0,
        Err(msg) => {
            errln(&msg);
            1
        }
    }
}

fn dispatch(args: &[String]) -> Result<(), String> {
    let Some(cmd) = args.first().map(String::as_str) else {
        outln(USAGE);
        return Ok(());
    };
    match cmd {
        "-h" | "--help" | "help" => {
            outln(USAGE);
            Ok(())
        }
        "inspect" => {
            let input = args.get(1).ok_or_else(|| "usage: xenolith inspect <file>".to_owned())?;
            #[cfg(feature = "frostbite")]
            let profile = EngineProfile::Frostbite;
            #[cfg(not(feature = "frostbite"))]
            let profile = EngineProfile::GenericX360;
            let info = inspect_file(Path::new(input), profile)?;
            outln(&format!("Detected: {}", info.detected));
            for (k, v) in &info.fields {
                outln(&format!("  {k}: {v}"));
            }
            Ok(())
        }
        #[cfg(feature = "frostbite")]
        "port-fbrb" => {
            let mut bfbc1 = false;
            let mut streaming_path: Option<String> = None;
            let mut havok_exe_path: Option<String> = None;
            let mut havok_tpl_path: Option<String> = None;
            let mut positional = Vec::new();
            let mut i = 1usize;
            while let Some(arg) = args.get(i) {
                if arg == "--bfbc1" {
                    bfbc1 = true;
                    i += 1;
                } else if arg == "--streaming-sounds" {
                    let val = args
                        .get(i + 1)
                        .ok_or_else(|| "missing path after --streaming-sounds".to_owned())?;
                    streaming_path = Some(val.clone());
                    i += 2;
                } else if arg == "--havok-exe" {
                    let val = args
                        .get(i + 1)
                        .ok_or_else(|| "missing path after --havok-exe".to_owned())?;
                    havok_exe_path = Some(val.clone());
                    i += 2;
                } else if arg == "--havok-template" {
                    let val = args
                        .get(i + 1)
                        .ok_or_else(|| "missing path after --havok-template".to_owned())?;
                    havok_tpl_path = Some(val.clone());
                    i += 2;
                } else {
                    positional.push(arg.as_str());
                    i += 1;
                }
            }
            let (Some(&inp), Some(&outp)) = (positional.first(), positional.get(1)) else {
                return Err(
                    "usage: xenolith port-fbrb [--bfbc1] [--streaming-sounds <sns.fbrb>] [--havok-exe <exe>] [--havok-template <tpl>] <in.fbrb> <out.fbrb>".into(),
                );
            };
            let raw = fs::read(inp).map_err(|e| format!("read {inp}: {e}"))?;
            let streaming_arc = if let Some(sp) = &streaming_path {
                let sraw = fs::read(sp).map_err(|e| format!("read {sp}: {e}"))?;
                Some(x360_frostbite::fbrb::FbrbArchive::parse(&sraw).map_err(|e| e.to_string())?)
            } else {
                None
            };
            let havok_exe = if let Some(ep) = &havok_exe_path {
                let eraw = fs::read(ep).map_err(|e| format!("read {ep}: {e}"))?;
                Some(x360_frostbite::bfbc1::load_havok_port_data(&eraw).map_err(|e| e.to_string())?)
            } else {
                None
            };
            let havok_tpl = if let Some(tp) = &havok_tpl_path {
                Some(fs::read(tp).map_err(|e| format!("read {tp}: {e}"))?)
            } else {
                None
            };
            let havok_ctx = havok_exe
                .as_ref()
                .zip(havok_tpl.as_deref())
                .map(|(exe_data, tpl)| exe_data.as_context(tpl));
            let opts = x360_frostbite::bundle_port::PortOptions {
                source_game: if bfbc1 { "bfbc1" } else { "bf1943" },
                bfbc1_upgrade: bfbc1,
                havok_classes: havok_exe.as_ref().map(|h| &h.classes),
                havok_port: havok_ctx.as_ref(),
                streaming_sounds: streaming_arc.as_ref(),
                ..Default::default()
            };
            let (pc, rep) = x360_frostbite::bundle_port::port_fbrb_bytes(&raw, &opts).map_err(|e| e.to_string())?;
            fs::write(outp, &pc).map_err(|e| format!("write {outp}: {e}"))?;
            outln(&format!(
                "Ported {} entries: {} converted, {} passthrough, {} skipped -> {} ({} bytes)",
                rep.total_entries,
                rep.converted,
                rep.passthrough,
                rep.skipped,
                outp,
                pc.len()
            ));
            if !rep.warnings.is_empty() {
                for (path, reason) in rep.warnings.iter().take(25) {
                    outln(&format!("  WARN {path}: {reason}"));
                }
                if rep.warnings.len() > 25 {
                    outln(&format!("  ... ({} more warnings)", rep.warnings.len() - 25));
                }
            }
            Ok(())
        }
        #[cfg(feature = "frostbite")]
        "unpack-fbrb" => {
            let (Some(inp), Some(out_dir)) = (args.get(1), args.get(2)) else {
                return Err("usage: xenolith unpack-fbrb <in.fbrb> <out_dir>".into());
            };
            let raw = fs::read(inp).map_err(|e| format!("read {inp}: {e}"))?;
            let arc = x360_frostbite::fbrb::FbrbArchive::parse(&raw).map_err(|e| e.to_string())?;
            let dest = Path::new(out_dir);
            let mut written = 0usize;
            for entry in &arc.entries {
                if entry.data.is_empty() || entry.path.contains("..") {
                    continue;
                }
                let p = dest.join(entry.path.replace('/', std::path::MAIN_SEPARATOR_STR));
                if let Some(parent) = p.parent() {
                    fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
                }
                fs::write(&p, &entry.data).map_err(|e| format!("write {}: {e}", p.display()))?;
                written += 1;
            }
            outln(&format!("Unpacked {written} entries -> {out_dir}"));
            Ok(())
        }
        #[cfg(all(feature = "frostbite", feature = "xma"))]
        "port-streaming-sounds" => {
            let mut wave_paths: Vec<String> = Vec::new();
            let mut positional: Vec<&str> = Vec::new();
            let mut i = 1usize;
            while let Some(arg) = args.get(i) {
                if arg == "--waves" {
                    let val = args
                        .get(i + 1)
                        .ok_or_else(|| "missing path after --waves".to_owned())?;
                    wave_paths.push(val.clone());
                    i += 2;
                } else {
                    positional.push(arg.as_str());
                    i += 1;
                }
            }
            let (Some(&inp), Some(&outp)) = (positional.first(), positional.get(1)) else {
                return Err(
                    "usage: xenolith port-streaming-sounds --waves <level.fbrb> <in_sns.fbrb> <out_sns.fbrb>".into(),
                );
            };
            let sns_raw = fs::read(inp).map_err(|e| format!("read {inp}: {e}"))?;
            let sns_arc = x360_frostbite::fbrb::FbrbArchive::parse(&sns_raw).map_err(|e| e.to_string())?;
            let mut wave_arcs = Vec::with_capacity(wave_paths.len());
            for wp in &wave_paths {
                let wraw = fs::read(wp).map_err(|e| format!("read {wp}: {e}"))?;
                wave_arcs.push(x360_frostbite::fbrb::FbrbArchive::parse(&wraw).map_err(|e| e.to_string())?);
            }
            let wave_refs: Vec<&x360_frostbite::fbrb::FbrbArchive> = wave_arcs.iter().collect();
            let (pc_arc, rep) = x360_frostbite::sns::port_streaming_sounds_archive(&sns_arc, &wave_refs);
            // Store streaming_sounds uncompressed for fast engine seek/streaming
            let pc_bytes = pc_arc.to_bytes(false).map_err(|e| e.to_string())?;
            fs::write(outp, &pc_bytes).map_err(|e| format!("write {outp}: {e}"))?;
            outln(&format!(
                "Ported streaming_sounds: {} total ({} converted -> PC EA-XAS SPS v1, {} passthrough, {} skipped) -> {} ({} bytes)",
                rep.total,
                rep.converted,
                rep.passthrough,
                rep.skipped,
                outp,
                pc_bytes.len()
            ));
            Ok(())
        }
        #[cfg(all(feature = "frostbite", feature = "xma"))]
        "repack-sns" => {
            let (Some(inp), Some(outp)) = (args.get(1), args.get(2)) else {
                return Err("usage: xenolith repack-sns <in.sns> <out.sns>".into());
            };
            let raw = fs::read(inp).map_err(|e| format!("read {inp}: {e}"))?;
            let (pc, rep) = x360_frostbite::sns::repack_sps_bank(&raw).map_err(|e| e.to_string())?;
            fs::write(outp, &pc).map_err(|e| format!("write {outp}: {e}"))?;
            outln(&format!(
                "Repacked {} streams ({} converted, {} passthrough) -> {} ({} bytes)",
                rep.entries.len(),
                rep.converted,
                rep.passthrough,
                outp,
                pc.len()
            ));
            Ok(())
        }
        #[cfg(feature = "frostbite")]
        "repack-shaderdb" => {
            let (Some(inp), Some(outp)) = (args.get(1), args.get(2)) else {
                return Err("usage: xenolith repack-shaderdb <in> <out>".into());
            };
            let raw = fs::read(inp).map_err(|e| format!("read {inp}: {e}"))?;
            let opts = x360_frostbite::shader_db::RepackOptions {
                strip_debug: true,
                replacements: std::collections::BTreeMap::new(),
            };
            let (pc, rep) = x360_frostbite::shader_db::repack_dxbc_database(&raw, &opts).map_err(|e| e.to_string())?;
            fs::write(outp, &pc).map_err(|e| format!("write {outp}: {e}"))?;
            outln(&format!(
                "Repacked {} DXBC shaders ({} debug chunks stripped, {} bytes saved) -> {}",
                rep.total_dxbc, rep.stripped_chunks, rep.bytes_saved, outp
            ));
            Ok(())
        }
        #[cfg(feature = "frostbite")]
        "compile-xenos-shaders" => {
            use x360_frostbite::shader_db::{TargetProfile, compile_xenos_database, find_fxc};
            let mut profile = TargetProfile::Dx10;
            let mut positional = Vec::new();
            let mut i = 1usize;
            while let Some(arg) = args.get(i) {
                if arg == "--profile" {
                    let val = args
                        .get(i + 1)
                        .ok_or_else(|| "missing value after --profile".to_owned())?;
                    profile = match val.to_ascii_lowercase().as_str() {
                        "dx9" => TargetProfile::Dx9,
                        "dx10" => TargetProfile::Dx10,
                        "dx11" => TargetProfile::Dx11,
                        other => return Err(format!("unknown profile `{other}` (expected dx9, dx10, or dx11)")),
                    };
                    i += 2;
                } else {
                    positional.push(arg.as_str());
                    i += 1;
                }
            }
            let (Some(&inp), Some(&out_dir)) = (positional.first(), positional.get(1)) else {
                return Err("usage: xenolith compile-xenos-shaders [--profile dx10|dx11|dx9] <in> <out_dir>".into());
            };
            let fxc = find_fxc().ok_or_else(|| "fxc.exe not found (install Windows SDK or set FXC_PATH)".to_owned())?;
            let raw = fs::read(inp).map_err(|e| format!("read {inp}: {e}"))?;
            fs::create_dir_all(out_dir).map_err(|e| format!("mkdir {out_dir}: {e}"))?;
            let iface = x360_frostbite::shader::hlsl::Interface::new(Vec::new());
            let results = compile_xenos_database(&raw, &fxc, profile, &iface);
            let mut ok_count = 0usize;
            let mut err_count = 0usize;
            for (idx, (entry, res)) in results.into_iter().enumerate() {
                match res {
                    Ok(bytes) => {
                        let stage = if entry.is_pixel { "ps" } else { "vs" };
                        let ext = if profile == TargetProfile::Dx9 {
                            "dx9.bin"
                        } else {
                            "dxbc"
                        };
                        let p = Path::new(out_dir).join(format!("{idx:04}_{stage}_{:08x}.{ext}", entry.offset));
                        fs::write(&p, &bytes).map_err(|e| format!("write {}: {e}", p.display()))?;
                        ok_count += 1;
                    }
                    Err(_) => err_count += 1,
                }
            }
            outln(&format!(
                "Compiled {ok_count} Xenos shaders ({err_count} failed) using {} -> {out_dir}",
                fxc.display()
            ));
            Ok(())
        }
        #[cfg(feature = "frostbite")]
        "upgrade-dbx" => {
            let (Some(inp), Some(outp)) = (args.get(1), args.get(2)) else {
                return Err("usage: xenolith upgrade-dbx <in.dbx> <out.dbx>".into());
            };
            let raw = fs::read(inp).map_err(|e| format!("read {inp}: {e}"))?;
            let rules = x360_frostbite::bfbc1::DbxSchemaRules::bfbc1_to_fb15();
            let (pc, rep) = x360_frostbite::bfbc1::upgrade_dbx_bytes(&raw, &rules).map_err(|e| e.to_string())?;
            fs::write(outp, &pc).map_err(|e| format!("write {outp}: {e}"))?;
            outln(&format!(
                "Upgraded DBX: {} instances dropped, {} dangling refs cleared, {} types renamed -> {}",
                rep.instances_dropped, rep.dangling_refs_cleared, rep.types_renamed, outp
            ));
            Ok(())
        }
        #[cfg(feature = "frostbite")]
        "dbx-to-xml" => {
            let (Some(inp), Some(outp)) = (args.get(1), args.get(2)) else {
                return Err("usage: xenolith dbx-to-xml <in.dbx> <out.xml>".into());
            };
            let raw = fs::read(inp).map_err(|e| format!("read {inp}: {e}"))?;
            let xml = x360_frostbite::dbx::parse(&raw).map_err(|e| e.to_string())?.to_xml();
            fs::write(outp, xml.as_bytes()).map_err(|e| format!("write {outp}: {e}"))?;
            outln(&format!("Exported XML -> {outp} ({} bytes)", xml.len()));
            Ok(())
        }
        #[cfg(feature = "frostbite")]
        "inspect-apt" => {
            let inp = args
                .get(1)
                .ok_or_else(|| "usage: xenolith inspect-apt <in.res>".to_owned())?;
            let raw = fs::read(inp).map_err(|e| format!("read {inp}: {e}"))?;
            let rep = x360_frostbite::apt::inspect_apt_symbols(&raw).map_err(|e| e.to_string())?;
            outln(&format!(
                "APT symbols: {} strings ({} FSCommands, {} externs, {} console prompts)",
                rep.string_count,
                rep.fscommands.len(),
                rep.externs.len(),
                rep.prompts.len()
            ));
            for cmd in &rep.fscommands {
                outln(&format!("  FSCommand: {cmd}"));
            }
            for ext in &rep.externs {
                outln(&format!("  extern:    {ext}"));
            }
            for pr in &rep.prompts {
                outln(&format!("  prompt:    {pr}"));
            }
            Ok(())
        }
        #[cfg(feature = "frostbite")]
        "unwrap-bink" => {
            let (Some(inp), Some(outp)) = (args.get(1), args.get(2)) else {
                return Err("usage: xenolith unwrap-bink <in.res> <out.bik>".into());
            };
            let raw = fs::read(inp).map_err(|e| format!("read {inp}: {e}"))?;
            let bik = x360_frostbite::bfbc1::unwrap_bink_movie(&raw).map_err(|e| e.to_string())?;
            fs::write(outp, bik).map_err(|e| format!("write {outp}: {e}"))?;
            outln(&format!("Unwrapped Bink movie -> {outp} ({} bytes)", bik.len()));
            Ok(())
        }
        #[cfg(feature = "frostbite")]
        "upgrade-hpd" => {
            let (Some(inp), Some(outp)) = (args.get(1), args.get(2)) else {
                return Err("usage: xenolith upgrade-hpd <in.hpd> <out.hpd>".into());
            };
            let raw = fs::read(inp).map_err(|e| format!("read {inp}: {e}"))?;
            let (v2, rep) = x360_frostbite::bfbc1::upgrade_kynogon_hpd(&raw).map_err(|e| e.to_string())?;
            if let Some(parent) = Path::new(outp).parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
            }
            fs::write(outp, &v2).map_err(|e| format!("write {outp}: {e}"))?;
            outln(&format!(
                "Upgraded Kynogon HPD v1 -> v2: {} sectors, {} nodes, {} edges -> {outp} ({} bytes)",
                rep.sectors,
                rep.nodes,
                rep.edges,
                v2.len()
            ));
            Ok(())
        }
        #[cfg(feature = "frostbite")]
        "make-sector-offsets" => {
            let (Some(lvl), Some(layer), Some(xs), Some(zs), Some(outp)) = (
                args.get(1),
                args.get(2),
                args.get(3),
                args.get(4),
                args.get(5),
            ) else {
                return Err(
                    "usage: xenolith make-sector-offsets <level_name> <Infantry|Vehicle> <x> <z> <out.dbx>".into(),
                );
            };
            let x: i32 = xs.parse().map_err(|e| format!("invalid x offset '{xs}': {e}"))?;
            let z: i32 = zs.parse().map_err(|e| format!("invalid z offset '{zs}': {e}"))?;
            let dbx_bytes = x360_frostbite::bfbc1::build_sector_offsets_dbx(lvl, layer, x, z);
            fs::write(outp, &dbx_bytes).map_err(|e| format!("write {outp}: {e}"))?;
            outln(&format!(
                "Built PathdataSectorOffsets DBX ({lvl}/{layer}, offset=({x},{z})) -> {outp} ({} bytes)",
                dbx_bytes.len()
            ));
            Ok(())
        }
        #[cfg(feature = "frostbite")]
        "register-level" => {
            let (Some(startup_path), Some(level)) = (args.get(1), args.get(2)) else {
                return Err("usage: xenolith register-level <startup.fbrb> <level> [donor]".into());
            };
            let donor = args.get(3).map_or("MP_012GR", String::as_str);
            let raw = fs::read(startup_path).map_err(|e| format!("read {startup_path}: {e}"))?;
            let mut arc = x360_frostbite::fbrb::FbrbArchive::parse(&raw).map_err(|e| e.to_string())?;
            let donor_key = format!("levels/{}_description.dbx", donor.to_ascii_lowercase());
            let target_name = format!("Levels/{level}_description.dbx");
            let target_key = target_name.to_ascii_lowercase();
            let level_lower = level.to_ascii_lowercase();

            let donor_entry = arc
                .entries
                .iter()
                .find(|e| e.path.to_ascii_lowercase() == donor_key)
                .cloned()
                .ok_or_else(|| format!("donor not found in startup: {donor_key}"))?;
            let llq_idx = arc
                .entries
                .iter()
                .position(|e| e.path.eq_ignore_ascii_case("levellistquery.dbx"))
                .ok_or_else(|| "LevelListQuery.dbx not found in startup".to_owned())?;

            let mut desc_dbx = x360_frostbite::dbx::parse(&donor_entry.data).map_err(|e| e.to_string())?;
            let new_guid = {
                // Deterministic 128-bit FNV-1a GUID for `opfor.bfbc1.<level>.description`
                let seed = format!("opfor.bfbc1.{level_lower}.description");
                let mut h: u128 = 0x6C62_272E_07BB_0142_62B8_2175_6295_C58D;
                for &b in seed.as_bytes() {
                    h ^= u128::from(b);
                    h = h.wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013B);
                }
                let d = h.to_be_bytes();
                format!(
                    "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
                    d[0], d[1], d[2], d[3], d[4], d[5], (d[6] & 0x0F) | 0x50, d[7], (d[8] & 0x3F) | 0x80, d[9],
                    d[10], d[11], d[12], d[13], d[14], d[15]
                )
            };
            let old_guid = desc_dbx
                .partition()
                .and_then(|p| p.attr("guid"))
                .map(ToOwned::to_owned)
                .unwrap_or_default();
            let old_desc_path = format!("Levels/{donor}_description");
            let new_desc_path = format!("Levels/{level}_description");
            let old_lvl_path = format!("Levels/{donor}");
            let new_lvl_path = format!("Levels/{level}");
            let old_id = format!("{donor}_description");
            let new_id = format!("{level}_description");
            #[allow(clippy::too_many_arguments)]
            fn rewrite_elem(
                e: &mut x360_frostbite::dbx::Element,
                old_guid: &str,
                new_guid: &str,
                old_desc: &str,
                new_desc: &str,
                old_lvl: &str,
                new_lvl: &str,
                old_id: &str,
                new_id: &str,
            ) {
                for (_, v) in &mut e.attributes {
                    if !old_guid.is_empty() && v.eq_ignore_ascii_case(old_guid) {
                        *v = new_guid.to_owned();
                    } else if v.eq_ignore_ascii_case(old_id) {
                        *v = new_id.to_owned();
                    }
                }
                if !e.text.is_empty() {
                    if e.text.eq_ignore_ascii_case(old_desc) {
                        e.text = new_desc.to_owned();
                    } else if e.text.eq_ignore_ascii_case(old_lvl) {
                        e.text = new_lvl.to_owned();
                    } else if e.text.contains(old_desc) {
                        e.text = e.text.replace(old_desc, new_desc);
                    } else if e.text.contains(old_lvl) {
                        e.text = e.text.replace(old_lvl, new_lvl);
                    }
                }
                for c in &mut e.children {
                    rewrite_elem(c, old_guid, new_guid, old_desc, new_desc, old_lvl, new_lvl, old_id, new_id);
                }
            }
            for r in &mut desc_dbx.roots {
                rewrite_elem(
                    r,
                    &old_guid,
                    &new_guid,
                    &old_desc_path,
                    &new_desc_path,
                    &old_lvl_path,
                    &new_lvl_path,
                    &old_id,
                    &new_id,
                );
            }
            let new_desc_bytes = desc_dbx.to_bytes();

            let mut llq_dbx = x360_frostbite::dbx::parse(&arc.entries[llq_idx].data).map_err(|e| e.to_string())?;
            fn add_to_first_array(e: &mut x360_frostbite::dbx::Element, items: &[String]) -> bool {
                if e.tag == "array" {
                    for want in items {
                        let exists = e
                            .children
                            .iter()
                            .any(|c| c.tag == "item" && c.text.eq_ignore_ascii_case(want));
                        if !exists {
                            e.children.push(x360_frostbite::dbx::Element {
                                tag: "item".into(),
                                kind: x360_frostbite::dbx::KIND_TEXT,
                                attributes: Vec::new(),
                                text: want.clone(),
                                raw: Vec::new(),
                                children: Vec::new(),
                            });
                        }
                    }
                    return true;
                }
                for c in &mut e.children {
                    if add_to_first_array(c, items) {
                        return true;
                    }
                }
                false
            }
            let want_items = vec![format!("{level_lower}.dbx"), format!("{level_lower}_description.dbx")];
            for r in &mut llq_dbx.roots {
                if add_to_first_array(r, &want_items) {
                    break;
                }
            }
            let new_llq_bytes = llq_dbx.to_bytes();
            arc.entries[llq_idx].data = new_llq_bytes.clone();

            if let Some(existing) = arc.entries.iter_mut().find(|e| e.path.to_ascii_lowercase() == target_key) {
                existing.data = new_desc_bytes.clone();
            } else {
                arc.entries.push(x360_frostbite::fbrb::FbrbEntry {
                    path: target_name.clone(),
                    ext: "<non-resource>".into(),
                    flags: donor_entry.flags,
                    data: new_desc_bytes.clone(),
                });
            }

            let out_bytes = arc.to_bytes(true).map_err(|e| e.to_string())?;
            fs::write(startup_path, &out_bytes).map_err(|e| format!("write {startup_path}: {e}"))?;
            outln(&format!(
                "Registered {target_name} ({} B) + LevelListQuery ({} B) in {startup_path}",
                new_desc_bytes.len(),
                new_llq_bytes.len()
            ));
            Ok(())
        }
        #[cfg(feature = "frostbite")]
        "dedup-fbrb" => {
            let (Some(lvl_path), Some(common_path), Some(outp)) = (args.get(1), args.get(2), args.get(3)) else {
                return Err("usage: xenolith dedup-fbrb <level.fbrb> <common.fbrb> <out.fbrb>".into());
            };
            let lvl_raw = fs::read(lvl_path).map_err(|e| format!("read {lvl_path}: {e}"))?;
            let com_raw = fs::read(common_path).map_err(|e| format!("read {common_path}: {e}"))?;
            let lvl_arc = x360_frostbite::fbrb::FbrbArchive::parse(&lvl_raw).map_err(|e| e.to_string())?;
            let com_arc = x360_frostbite::fbrb::FbrbArchive::parse(&com_raw).map_err(|e| e.to_string())?;
            let (deduped, removed) =
                x360_frostbite::bfbc1::deduplicate_common_entries_guid_aware(&lvl_arc, &com_arc);
            let bytes = deduped.to_bytes(true).map_err(|e| e.to_string())?;
            fs::write(outp, &bytes).map_err(|e| format!("write {outp}: {e}"))?;
            outln(&format!(
                "Deduplicated {removed} shared entries ({} kept) -> {outp} ({} bytes)",
                deduped.entries.len(),
                bytes.len()
            ));
            Ok(())
        }
        #[cfg(feature = "frostbite")]
        "export-atlas-sprite" => {
            let (Some(fbrb_path), Some(prefix), Some(id_str), Some(outp)) =
                (args.get(1), args.get(2), args.get(3), args.get(4))
            else {
                return Err(
                    "usage: xenolith export-atlas-sprite <mainmenu.fbrb> <atlas_prefix> <texture_file_id> <out.png>"
                        .into(),
                );
            };
            let raw = fs::read(fbrb_path).map_err(|e| format!("read {fbrb_path}: {e}"))?;
            let arc = x360_frostbite::fbrb::FbrbArchive::parse(&raw).map_err(|e| e.to_string())?;
            let pref_low = prefix.trim_end_matches('/').to_ascii_lowercase();
            let atlas_key = format!("{pref_low}/atlas0.res");
            let meta_key = format!("{pref_low}/atlas_meta.dbx");

            let atlas_entry = arc
                .entries
                .iter()
                .find(|e| e.path.to_ascii_lowercase() == atlas_key)
                .ok_or_else(|| format!("missing {atlas_key} in {fbrb_path}"))?;
            let meta_entry = arc
                .entries
                .iter()
                .find(|e| e.path.to_ascii_lowercase() == meta_key)
                .ok_or_else(|| format!("missing {meta_key} in {fbrb_path}"))?;

            let itex = if atlas_entry.ext == "XenonTexture" {
                x360_frostbite::texture::convert(&atlas_entry.data).map_err(|e| e.to_string())?
            } else {
                atlas_entry.data.clone()
            };
            let (aw, ah, rgba) = x360_frostbite::menu_export::decode_itexture_rgba(&itex)
                .ok_or_else(|| format!("failed to decode ITexture {atlas_key}"))?;

            let dbx_file = x360_frostbite::dbx::parse(&meta_entry.data).map_err(|e| e.to_string())?;
            let target_id = id_str.trim();
            let mut found_rect: Option<(usize, usize, usize, usize)> = None;
            for inst in dbx_file.instances() {
                if inst.attr("type") != Some("UI.UITextureAtlasInfo") {
                    continue;
                }
                let tf_match = inst.child("TextureFile").is_some_and(|c| {
                    c.text.trim() == target_id
                        || c.as_i32s()
                            .and_then(|v| v.first().copied())
                            .is_some_and(|v| v.to_string() == target_id)
                });
                if !tf_match {
                    continue;
                }
                let uv = |name: &str| -> Option<f32> {
                    let c = inst.child(name)?;
                    c.as_f32s()
                        .and_then(|v| v.first().copied())
                        .or_else(|| c.text.trim().parse::<u32>().ok().map(f32::from_bits))
                };
                if let (Some(u0), Some(u1), Some(v0), Some(v1)) = (uv("MinU"), uv("MaxU"), uv("MinV"), uv("MaxV")) {
                    let atlas_w = f64::from(aw);
                    let atlas_h = f64::from(ah);
                    let x0 = (f64::from(u0) * atlas_w).round().clamp(0.0, atlas_w) as usize;
                    let x1 = (f64::from(u1) * atlas_w).round().clamp(0.0, atlas_w) as usize;
                    let y0 = (f64::from(v0) * atlas_h).round().clamp(0.0, atlas_h) as usize;
                    let y1 = (f64::from(v1) * atlas_h).round().clamp(0.0, atlas_h) as usize;
                    if x1 > x0 && y1 > y0 {
                        found_rect = Some((x0, y0, x1, y1));
                        break;
                    }
                }
            }
            let (mut x0, mut y0, mut x1, mut y1) =
                found_rect.ok_or_else(|| format!("TextureFile={target_id} not found in {meta_key}"))?;

            // Trim fully transparent border rows/cols for crisp Hub placement.
            let aw_u = aw as usize;
            let pixel_alpha = |x: usize, y: usize| -> u8 { rgba.get((y * aw_u + x) * 4 + 3).copied().unwrap_or(0) };
            while y0 < y1 && (x0..x1).all(|x| pixel_alpha(x, y0) == 0) {
                y0 += 1;
            }
            while y1 > y0 && (x0..x1).all(|x| pixel_alpha(x, y1 - 1) == 0) {
                y1 -= 1;
            }
            while x0 < x1 && (y0..y1).all(|y| pixel_alpha(x0, y) == 0) {
                x0 += 1;
            }
            while x1 > x0 && (y0..y1).all(|y| pixel_alpha(x1 - 1, y) == 0) {
                x1 -= 1;
            }

            let cw = x1 - x0;
            let ch = y1 - y0;
            let mut crop = Vec::with_capacity(cw * ch * 4);
            for y in y0..y1 {
                let row_start = (y * aw_u + x0) * 4;
                if let Some(slice) = rgba.get(row_start..row_start + cw * 4) {
                    crop.extend_from_slice(slice);
                }
            }
            let png = x360_frostbite::menu_export::encode_rgba_png(cw as u32, ch as u32, &crop)
                .map_err(|e| format!("encode png: {e}"))?;
            if let Some(parent) = Path::new(outp).parent() {
                fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
            }
            fs::write(outp, &png).map_err(|e| format!("write {outp}: {e}"))?;
            outln(&format!(
                "Exported sprite {target_id} ({cw}x{ch}) -> {outp} ({} bytes)",
                png.len()
            ));
            Ok(())
        }
        #[cfg(feature = "frostbite")]
        "export-bf1943-menu" => {
            let (Some(stfs_path), Some(out_dir)) = (args.get(1), args.get(2)) else {
                return Err("usage: xenolith export-bf1943-menu <stfs_pkg> <module_dir>".into());
            };
            let base = Path::new(out_dir);
            let art_dir = base.join("art").join("x360");
            let sfx_dir = base.join("sfx");
            let rep = x360_frostbite::menu_export::export_bf1943_menu_assets(Path::new(stfs_path), &art_dir, &sfx_dir)
                .map_err(|e| e.to_string())?;
            outln(&format!(
                "Exported BF1943 menu assets: {} PNGs, {} WAVs, {} fonts -> {out_dir}",
                rep.pngs_exported, rep.wavs_exported, rep.fonts_exported
            ));
            Ok(())
        }
        other => Err(format!("unknown subcommand `{other}` (run `xenolith --help`)")),
    }
}
