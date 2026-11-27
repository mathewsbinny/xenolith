//! BFBC1 Kynogon HPD v1 -> Frostbite 1.5 Kynogon HPD v2 navmesh upgrader.
#![deny(unsafe_code)]

use super::Bfbc1Error;
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

const HPD_V1_MAGIC: u32 = 0x3FA6_6666; // 1.3f
const HPD_V1_HDR_LEN: usize = 0x84;
const HPD_V2_HDR_LEN: usize = 0x9C;
const HPD_SECTOR_ENTRY_LEN: usize = 48;
const HPD_NODE_LEN: usize = 36;
const HPD_V1_EDGE_LEN: usize = 20;
const HPD_V2_EDGE_LEN: usize = 12;

fn hpd_u32_le(buf: &[u8], off: usize) -> Option<u32> {
    let bytes: [u8; 4] = buf.get(off..off + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn upgrade_hpd_sector(
    data: &[u8],
    i: usize,
    pay_base: usize,
    new_table: &mut Vec<u8>,
    new_payload: &mut Vec<u8>,
    report: &mut HpdUpgradeReport,
) -> Result<(), Bfbc1Error> {
    let epos = HPD_V1_HDR_LEN + i * HPD_SECTOR_ENTRY_LEN;
    let entry_slice = data
        .get(epos..epos + HPD_SECTOR_ENTRY_LEN)
        .ok_or_else(|| Bfbc1Error::Hpd(format!("sector {i} header out of bounds")))?;
    let mut entry = [0u8; HPD_SECTOR_ENTRY_LEN];
    entry.copy_from_slice(entry_slice);
    let old_sz = hpd_u32_le(&entry, 0x20).unwrap_or(0) as usize;
    let old_off = hpd_u32_le(&entry, 0x24).unwrap_or(0) as usize;
    let s_start = pay_base
        .checked_add(old_off)
        .ok_or_else(|| Bfbc1Error::Hpd(format!("sector {i} offset overflow")))?;
    let s_end = s_start
        .checked_add(old_sz)
        .ok_or_else(|| Bfbc1Error::Hpd(format!("sector {i} size overflow")))?;
    let Some(sec) = data.get(s_start..s_end).filter(|_| old_sz >= 8) else {
        return Err(Bfbc1Error::Hpd(format!(
            "sector {i} range {s_start:#x}..{s_end:#x} out of bounds"
        )));
    };
    let n_nodes = hpd_u32_le(sec, 0).unwrap_or(0) as usize;
    let n_edges = hpd_u32_le(sec, 4).unwrap_or(0) as usize;
    let nodes_end = 8usize
        .checked_add(n_nodes.saturating_mul(HPD_NODE_LEN))
        .ok_or_else(|| Bfbc1Error::Hpd(format!("sector {i} node table overflow")))?;
    let edges_end = nodes_end
        .checked_add(n_edges.saturating_mul(HPD_V1_EDGE_LEN))
        .ok_or_else(|| Bfbc1Error::Hpd(format!("sector {i} edge table overflow")))?;
    let (Some(nodes_part), Some(edges_part), Some(mesh_tail)) = (
        sec.get(..nodes_end),
        sec.get(nodes_end..edges_end),
        sec.get(edges_end..),
    ) else {
        return Err(Bfbc1Error::Hpd(format!(
            "sector {i} graph ({n_nodes} nodes, {n_edges} edges) exceeds sector size {}",
            sec.len()
        )));
    };

    let new_off = new_payload.len();
    // Copy header + nodes verbatim
    new_payload.extend_from_slice(nodes_part);
    // Compact 20-byte edges -> 12-byte edges (tag, from, to)
    for edge in edges_part.as_chunks::<HPD_V1_EDGE_LEN>().0 {
        if let Some(compact) = edge.get(..HPD_V2_EDGE_LEN) {
            new_payload.extend_from_slice(compact);
        }
    }
    // Copy trailing Kynogon InPlace Mesh payload verbatim
    new_payload.extend_from_slice(mesh_tail);
    let new_sz = new_payload.len() - new_off;

    if let Some(dst) = entry.get_mut(0x20..0x24) {
        dst.copy_from_slice(&(new_sz as u32).to_le_bytes());
    }
    if let Some(dst) = entry.get_mut(0x24..0x28) {
        dst.copy_from_slice(&(new_off as u32).to_le_bytes());
    }
    new_table.extend_from_slice(&entry);

    report.nodes += n_nodes;
    report.edges += n_edges;
    Ok(())
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
    if data.starts_with(b"Kynogon HPD\0") {
        // Already HPD v2
        let sectors = hpd_u32_le(data, 0x14).unwrap_or(0) as usize;
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
    let magic = hpd_u32_le(data, 0).unwrap_or(0);
    if magic != HPD_V1_MAGIC {
        return Err(Bfbc1Error::Hpd(format!(
            "unexpected HPD v1 magic {magic:#010x} (expected {HPD_V1_MAGIC:#010x})"
        )));
    }
    let sector_count = hpd_u32_le(data, 4).unwrap_or(0) as usize;
    let node_size = hpd_u32_le(data, 0x7C).unwrap_or(0) as usize;
    let edge_size = hpd_u32_le(data, 0x80).unwrap_or(0) as usize;
    if node_size != HPD_NODE_LEN || edge_size != HPD_V1_EDGE_LEN {
        return Err(Bfbc1Error::Hpd(format!(
            "unsupported HPD v1 node/edge sizes ({node_size}/{edge_size}, expected {HPD_NODE_LEN}/{HPD_V1_EDGE_LEN})"
        )));
    }

    let tbl_end = HPD_V1_HDR_LEN
        .checked_add(sector_count.saturating_mul(HPD_SECTOR_ENTRY_LEN))
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
        let last_off_pos = HPD_V1_HDR_LEN + (sector_count - 1) * HPD_SECTOR_ENTRY_LEN;
        let last_sz = hpd_u32_le(data, last_off_pos + 0x20).unwrap_or(0) as usize;
        let last_off = hpd_u32_le(data, last_off_pos + 0x24).unwrap_or(0) as usize;
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
    out.extend_from_slice(&(HPD_NODE_LEN as u32).to_le_bytes()); // 0x8C: node_size = 36
    out.extend_from_slice(&(HPD_V2_EDGE_LEN as u32).to_le_bytes()); // 0x90: edge_size = 12
    out.extend_from_slice(&1u32.to_le_bytes()); // 0x94
    out.extend_from_slice(&0u32.to_le_bytes()); // 0x98
    debug_assert_eq!(out.len(), HPD_V2_HDR_LEN);

    let mut new_table = Vec::with_capacity(sector_count * HPD_SECTOR_ENTRY_LEN);
    let mut new_payload = Vec::with_capacity(data.len().saturating_sub(pay_base));
    let mut report = HpdUpgradeReport {
        sectors: sector_count,
        nodes: 0,
        edges: 0,
    };

    for i in 0..sector_count {
        upgrade_hpd_sector(data, i, pay_base, &mut new_table, &mut new_payload, &mut report)?;
    }

    out.extend_from_slice(&new_table);
    out.extend_from_slice(&new_payload);
    Ok((out, report))
}
