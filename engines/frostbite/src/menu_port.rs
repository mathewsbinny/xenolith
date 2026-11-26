//! Battlefield 1943 Xbox 360 -> PC `mainmenu-00.fbrb` converter.
//!
//! Reads `dist/xenon/menu/main/mainmenu-00.fbrb` from the user's Xbox 360 BF1943 STFS package
//! (read-only), converts every Xbox 360 resource to PC Win32 format, and writes a native PC
//! `mainmenu-00.fbrb` bundle:
//! - `SwfMovie` (`BIGF` `.apt`/`.const`/`.geo`): big-endian -> little-endian via [`crate::apt`]
//! - `XenonTexture`: Xenos tiled BE -> PC `ITexture` linear LE via [`crate::texture::convert`]
//! - `Wave`: EA-XMA2 -> PC EA-XAS via [`crate::wavport::convert`]
//! - `.dbx` (`<non-resource>`): in-place 6-byte length-preserving `_Xenon`/`_xenon` -> `_Win32`/`_win32`

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::BufReader;
use std::path::Path;

use x360_containers::stfs::Package;

use crate::apt;
use crate::fbrb::{FbrbArchive, FbrbEntry, FbrbError};
use crate::texture;
use crate::wavport;

/// Errors returned while porting BF1943's Xbox 360 menu bundle to PC.
#[derive(Debug, thiserror::Error)]
pub enum MenuPortError {
    /// Failed to read or write files on disk.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Failed to open or read the Xbox 360 STFS package.
    #[error("STFS error: {0}")]
    Stfs(#[from] x360_containers::stfs::Error),
    /// Target `mainmenu-00.fbrb` was not found inside the STFS container.
    #[error("mainmenu-00.fbrb not found inside STFS package")]
    MissingMainMenu,
    /// Failed to parse or write `FbRB`.
    #[error("FbRB error: {0}")]
    Fbrb(#[from] FbrbError),
    /// Failed to convert an APT `SwfMovie`.
    #[error("APT conversion error in '{0}': {1}")]
    Apt(String, apt::AptError),
    /// Failed to convert a `XenonTexture`.
    #[error("texture conversion error in '{0}': {1}")]
    Texture(String, texture::TextureError),
}

/// Summary statistics from converting a BF1943 Xbox 360 menu bundle.
#[derive(Debug, Clone, Default)]
pub struct MenuPortReport {
    /// Total entries written to the PC `FbRB`.
    pub total_entries: usize,
    /// Number of `SwfMovie` (`BIGF` APT) resources converted BE -> LE.
    pub swf_converted: usize,
    /// Number of `XenonTexture` resources converted to PC `ITexture`.
    pub textures_converted: usize,
    /// Number of `Wave` resources converted from XMA2 to PC EA-XAS.
    pub waves_converted: usize,
    /// Number of `Wave` resources kept verbatim (e.g. streamed stubs).
    pub waves_passthrough: usize,
    /// Number of `.dbx` entries with `_Xenon` -> `_Win32` path/string rewrites.
    pub dbx_rewritten: usize,
}

fn replace_xenon_in_path(path: &str) -> String {
    path.replace("_Xenon", "_Win32")
        .replace("_xenon", "_win32")
        .replace("_XENON", "_WIN32")
}

/// Length-preserving 6-byte -> 6-byte replacement of `_Xenon` / `_xenon` / `_XENON` -> `_Win32` / `_win32` / `_WIN32`
/// inside binary `{binary}` DBX string tables.
fn rewrite_dbx_xenon_to_win32(data: &[u8]) -> (Vec<u8>, bool) {
    let mut out = data.to_vec();
    let mut changed = false;
    let pairs: &[(&[u8; 6], &[u8; 6])] = &[(b"_Xenon", b"_Win32"), (b"_xenon", b"_win32"), (b"_XENON", b"_WIN32")];
    for &(needle, repl) in pairs {
        let mut i = 0usize;
        while i + 6 <= out.len() {
            if out.get(i..i + 6) == Some(needle.as_slice()) {
                if let Some(dst) = out.get_mut(i..i + 6) {
                    dst.copy_from_slice(repl);
                    changed = true;
                }
                i += 6;
            } else {
                i += 1;
            }
        }
    }
    (out, changed)
}

/// Converts `dist/xenon/menu/main/mainmenu-00.fbrb` from the Xbox 360 BF1943 STFS package into a PC
/// `mainmenu-00.fbrb` at `out_path`.
///
/// # Errors
/// Returns [`MenuPortError`] if reading the STFS or converting any required asset fails.
pub fn convert_bf1943_mainmenu(stfs_path: &Path, out_path: &Path) -> Result<MenuPortReport, MenuPortError> {
    let file = File::open(stfs_path)?;
    let mut pkg = Package::open(BufReader::new(file))?;
    let mm_entry = pkg
        .entries
        .iter()
        .find(|e| String::from_utf8_lossy(&e.path).eq_ignore_ascii_case("dist/xenon/menu/main/mainmenu-00.fbrb"))
        .cloned()
        .ok_or(MenuPortError::MissingMainMenu)?;

    let raw_fbrb = pkg.read(
        u64::from(mm_entry.start),
        u64::from(mm_entry.count),
        mm_entry.consecutive,
        u64::from(mm_entry.size),
    )?;
    let src_arc = FbrbArchive::parse(&raw_fbrb)?;
    let mut out_entries = Vec::with_capacity(src_arc.entries.len());
    let mut report = MenuPortReport::default();

    for entry in src_arc.entries {
        let new_path = replace_xenon_in_path(&entry.path);
        match entry.ext.as_str() {
            "SwfMovie" => {
                let mut converted =
                    apt::convert_bigf_be_to_le(&entry.data).map_err(|e| MenuPortError::Apt(entry.path.clone(), e))?;
                if entry.path.eq_ignore_ascii_case("Menu/main_nav.res") {
                    let mut patcher = apt::AptPatcher::from_bigf_le(&converted)
                        .map_err(|e| MenuPortError::Apt(entry.path.clone(), e))?;
                    patcher
                        .patch_bf1943_main_nav_entry()
                        .map_err(|e| MenuPortError::Apt(entry.path.clone(), e))?;
                    converted = patcher.into_bigf();
                }
                report.swf_converted += 1;
                out_entries.push(FbrbEntry {
                    path: new_path,
                    ext: entry.ext,
                    flags: entry.flags,
                    data: converted,
                });
            }
            "XenonTexture" => {
                let converted =
                    texture::convert(&entry.data).map_err(|e| MenuPortError::Texture(entry.path.clone(), e))?;
                report.textures_converted += 1;
                out_entries.push(FbrbEntry {
                    path: new_path,
                    ext: "ITexture".to_owned(),
                    flags: entry.flags,
                    data: converted,
                });
            }
            "Wave" => {
                if let Some(pc_wave) = wavport::convert(&entry.data, "bf1943", &[None, None, None], 0) {
                    report.waves_converted += 1;
                    out_entries.push(FbrbEntry {
                        path: new_path,
                        ext: entry.ext,
                        flags: entry.flags,
                        data: pc_wave,
                    });
                } else {
                    report.waves_passthrough += 1;
                    out_entries.push(FbrbEntry {
                        path: new_path,
                        ext: entry.ext,
                        flags: entry.flags,
                        data: entry.data,
                    });
                }
            }
            _ => {
                let (rewritten, changed) = rewrite_dbx_xenon_to_win32(&entry.data);
                if changed || new_path != entry.path {
                    report.dbx_rewritten += 1;
                }
                out_entries.push(FbrbEntry {
                    path: new_path,
                    ext: entry.ext,
                    flags: entry.flags,
                    data: rewritten,
                });
            }
        }
    }

    report.total_entries = out_entries.len();
    let out_arc = FbrbArchive {
        version: src_arc.version,
        entries: out_entries,
    };
    let bytes = out_arc.to_bytes(true)?;
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(out_path, bytes)?;
    Ok(report)
}

/// Returns `true` if a BF1943 `mainmenu-00.fbrb` entry is a UI movie, UI texture atlas, rank icon,
/// or UI SFX resource (`.res`) that should be overlaid onto BFBC2 PC's base `mainmenu-00.fbrb`.
///
/// Raw BF1943 `.dbx` files use Frostbite 1.0 schema types (such as `TofARGB8888`) that stall
/// BFBC2 PC's Frostbite 1.5 DBX loader, so we preserve BFBC2 PC's `.dbx` descriptors and overlay
/// the converted binary resources (`SwfMovie`, `ITexture`, `Wave`).
fn is_bf1943_ui_overlay_entry(path: &str, ext: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if ext == "<non-resource>" {
        return lower == "menu/menu_shared_images.dbx"
            || lower == "menu/shared_images.dbx"
            || lower == "menu/common_screens.dbx"
            || lower == "menu/menu_common.dbx"
            || lower == "menu/igm_comp_images.dbx"
            || lower.starts_with("menu/menu_shared_images_textures_xenon/")
            || lower.starts_with("menu/shared_images_textures_xenon/")
            || lower.starts_with("menu/main_nav_textures_xenon/")
            || lower.starts_with("menu/common_screens_textures_xenon/")
            || lower.starts_with("menu/menu_common_textures_xenon/")
            || lower.starts_with("menu/igm_comp_images_textures_xenon/");
    }
    if lower == "menu/main_root.res"
        || lower == "menu/main_popups.res"
        || lower == "menu/main_popups_textures_xenon/atlas0.res"
        || lower == "menu/textmanagercomponent.res"
        || lower.starts_with("sound/default_sound")
        || lower.starts_with("sound/ui/")
    {
        return false;
    }
    matches!(ext, "SwfMovie" | "XenonTexture") || (ext == "Wave" && lower.starts_with("sound/beach_sfx/ui/"))
}

/// Converts BF1943's Xbox 360 UI movies, textures, and UI sounds and merges them cleanly onto
/// BFBC2 PC's base `mainmenu-00.fbrb` so Frostbite 1.5 PC retains its native `menuconfig.dbx`,
/// `loadingscreen_mp.res`, and `server_browser.res` while running the literal BF1943 menu.
///
/// # Errors
/// Returns [`MenuPortError`] if reading the STFS/PC base bundle or converting any asset fails.
#[allow(clippy::too_many_lines)]
pub fn merge_bf1943_into_pc_mainmenu(
    stfs_path: &Path,
    pc_base_mainmenu: &Path,
    out_path: &Path,
) -> Result<MenuPortReport, MenuPortError> {
    let pc_raw = fs::read(pc_base_mainmenu)?;
    let pc_arc = FbrbArchive::parse(&pc_raw)?;

    let file = File::open(stfs_path)?;
    let mut pkg = Package::open(BufReader::new(file))?;
    let mm_entry = pkg
        .entries
        .iter()
        .find(|e| String::from_utf8_lossy(&e.path).eq_ignore_ascii_case("dist/xenon/menu/main/mainmenu-00.fbrb"))
        .cloned()
        .ok_or(MenuPortError::MissingMainMenu)?;

    let raw_fbrb = pkg.read(
        u64::from(mm_entry.start),
        u64::from(mm_entry.count),
        mm_entry.consecutive,
        u64::from(mm_entry.size),
    )?;
    let src_arc = FbrbArchive::parse(&raw_fbrb)?;

    let mut report = MenuPortReport::default();
    let mut overlay: BTreeMap<(String, String), FbrbEntry> = BTreeMap::new();

    for entry in src_arc.entries {
        if !is_bf1943_ui_overlay_entry(&entry.path, &entry.ext) {
            continue;
        }
        let new_path = replace_xenon_in_path(&entry.path);
        let converted_entry = match entry.ext.as_str() {
            "SwfMovie" => {
                let mut converted =
                    apt::convert_bigf_be_to_le(&entry.data).map_err(|e| MenuPortError::Apt(entry.path.clone(), e))?;
                if entry.path.eq_ignore_ascii_case("Menu/main_nav.res") {
                    let mut patcher = apt::AptPatcher::from_bigf_le(&converted)
                        .map_err(|e| MenuPortError::Apt(entry.path.clone(), e))?;
                    patcher
                        .patch_bf1943_main_nav_entry()
                        .map_err(|e| MenuPortError::Apt(entry.path.clone(), e))?;
                    converted = patcher.into_bigf();
                }
                report.swf_converted += 1;
                FbrbEntry {
                    path: new_path,
                    ext: entry.ext,
                    flags: entry.flags,
                    data: converted,
                }
            }
            "XenonTexture" => {
                let converted =
                    texture::convert(&entry.data).map_err(|e| MenuPortError::Texture(entry.path.clone(), e))?;
                report.textures_converted += 1;
                FbrbEntry {
                    path: new_path,
                    ext: "ITexture".to_owned(),
                    flags: entry.flags,
                    data: converted,
                }
            }
            "Wave" => {
                if let Some(pc_wave) = wavport::convert(&entry.data, "bf1943", &[None, None, None], 0) {
                    report.waves_converted += 1;
                    FbrbEntry {
                        path: new_path,
                        ext: entry.ext,
                        flags: entry.flags,
                        data: pc_wave,
                    }
                } else {
                    report.waves_passthrough += 1;
                    FbrbEntry {
                        path: new_path,
                        ext: entry.ext,
                        flags: entry.flags,
                        data: entry.data,
                    }
                }
            }
            _ => {
                let (rewritten, changed) = rewrite_dbx_xenon_to_win32(&entry.data);
                if changed || new_path != entry.path {
                    report.dbx_rewritten += 1;
                }
                FbrbEntry {
                    path: new_path,
                    ext: entry.ext,
                    flags: entry.flags,
                    data: rewritten,
                }
            }
        };
        let key = (
            converted_entry.path.to_ascii_lowercase(),
            converted_entry.ext.to_ascii_lowercase(),
        );
        overlay.insert(key, converted_entry);
    }

    let mut merged = Vec::with_capacity(pc_arc.entries.len() + overlay.len());
    for base_entry in pc_arc.entries {
        let key = (
            base_entry.path.to_ascii_lowercase(),
            base_entry.ext.to_ascii_lowercase(),
        );
        // Preserve BFBC2 PC's SwfMovie .dbx GUIDs (main_root.dbx, main_nav.dbx, main_popups.dbx, etc.)
        // so PC menuconfig.dbx resolves them, while replacing their .res SwfMovie payloads and
        // replacing texture atlas .dbx / atlas_meta.dbx with BF1943's.
        let keep_pc_dbx = base_entry.ext == "<non-resource>" && !key.0.contains("_textures_win32/");
        if let Some(repl) = overlay.remove(&key) {
            if keep_pc_dbx {
                merged.push(base_entry);
            } else {
                merged.push(repl);
            }
        } else {
            merged.push(base_entry);
        }
    }
    for (_, extra) in overlay {
        merged.push(extra);
    }

    report.total_entries = merged.len();
    let out_arc = FbrbArchive {
        version: pc_arc.version,
        entries: merged,
    };
    let bytes = out_arc.to_bytes(true)?;
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(out_path, bytes)?;
    Ok(report)
}
