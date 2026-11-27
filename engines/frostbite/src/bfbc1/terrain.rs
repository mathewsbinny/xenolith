//! Frostbite 1.0 (BFBC1) terrain heightfield and PC mesh vertex-layout upgraders.
#![deny(unsafe_code)]

use crate::mesh::{self, Pair};
use crate::{meshconv, meshdata, terrain};

use super::{Bfbc1Error, FB10_EXT_HEIGHTFIELD_HEADER, FB10_HEIGHTFIELD_HEADER, FB15_HEIGHTFIELD_HEADER};
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

pub(super) fn read_u32_le(x: &[u8], at: usize) -> Option<u32> {
    let bytes: [u8; 4] = x.get(at..at + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

pub(super) fn read_u16_le(x: &[u8], at: usize) -> Option<u16> {
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
