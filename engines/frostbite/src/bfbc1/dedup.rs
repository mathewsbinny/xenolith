//! Shared-common `FbRB` bundle deduplication (path-based and GUID-aware).
#![deny(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use crate::fbrb::FbrbArchive;
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

fn is_guid_bytes(s: &[u8]) -> bool {
    let Ok(arr): Result<&[u8; 36], _> = s.try_into() else {
        return false;
    };
    arr[8] == b'-'
        && arr[13] == b'-'
        && arr[18] == b'-'
        && arr[23] == b'-'
        && arr
            .iter()
            .enumerate()
            .all(|(i, &b)| matches!(i, 8 | 13 | 18 | 23) || b.is_ascii_hexdigit())
}

fn lower_guid_array(s: &[u8]) -> Option<[u8; 36]> {
    let arr: &[u8; 36] = s.try_into().ok()?;
    let mut out = [0u8; 36];
    for (dst, &src) in out.iter_mut().zip(arr.iter()) {
        *dst = src.to_ascii_lowercase();
    }
    Some(out)
}

/// Extracts all 36-byte ASCII GUIDs (lowercased) present in `data` using SIMD-accelerated
/// hyphen scanning (`memchr`) so subsequent membership queries are `O(log G)` instead of `O(N)`.
fn extract_guids_lower(data: &[u8]) -> BTreeSet<[u8; 36]> {
    let mut guids = BTreeSet::new();
    for dash_pos in memchr::memchr_iter(b'-', data) {
        if let Some(start) = dash_pos.checked_sub(8)
            && let Some(cand) = data.get(start..start + 36)
            && is_guid_bytes(cand)
            && let Some(lower) = lower_guid_array(cand)
        {
            guids.insert(lower);
        }
    }
    guids
}

fn is_stem_protected(lower: &str, protected_stems: &BTreeSet<String>) -> bool {
    if protected_stems.is_empty() {
        return false;
    }
    if let Some(stem) = lower.strip_suffix(".dbx").or_else(|| lower.strip_suffix(".res"))
        && protected_stems.contains(stem)
    {
        return true;
    }
    for (idx, _) in lower.match_indices('_') {
        if let Some(prefix) = lower.get(..idx)
            && protected_stems.contains(prefix)
        {
            return true;
        }
    }
    false
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

    // Lazily indexed GUID sets per referenced common DBX partition (`O(log G)` lookup).
    let mut common_guid_cache: BTreeMap<String, BTreeSet<[u8; 36]>> = BTreeMap::new();
    let mut protected_stems: BTreeSet<String> = BTreeSet::new();
    for entry in &level_arc.entries {
        if !entry.path.to_ascii_lowercase().ends_with(".dbx") || entry.data.len() < 40 {
            continue;
        }
        let d = entry.data.as_slice();
        for slash in memchr::memchr_iter(b'/', d) {
            let Some(guid_slice) = d.get(slash + 1..slash + 37) else {
                continue;
            };
            if !is_guid_bytes(guid_slice) {
                continue;
            }
            let mut start = slash;
            while start > 0
                && d.get(start - 1)
                    .is_some_and(|&c| (0x20..0x7F).contains(&c) && c != b'"')
            {
                start -= 1;
            }
            if let Some(part_bytes) = d.get(start..slash)
                && !part_bytes.is_empty()
            {
                let part = String::from_utf8_lossy(part_bytes)
                    .replace('\\', "/")
                    .to_ascii_lowercase();
                if protected_stems.contains(&part) {
                    continue;
                }
                if let Some(&com_bytes) = common_dbx.get(&part)
                    && let Some(guid_lower) = lower_guid_array(guid_slice)
                {
                    let guid_set = common_guid_cache
                        .entry(part.clone())
                        .or_insert_with(|| extract_guids_lower(com_bytes));
                    if !guid_set.contains(&guid_lower) {
                        protected_stems.insert(part);
                    }
                }
            }
        }
    }

    let mut kept = Vec::with_capacity(level_arc.entries.len());
    let mut removed = 0usize;
    for entry in &level_arc.entries {
        let lower = entry.path.replace('\\', "/").to_ascii_lowercase();
        if !is_stem_protected(&lower, &protected_stems) && common_paths.contains(&lower) {
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
