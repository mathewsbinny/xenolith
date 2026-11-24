//! Reference CPU interpreter for Xbox 360 (Xenos) shader microcode decoded by [`crate::xenos`], ported from
//! `bfbc2-shader-tools/xenos_exec.py` and extended for general-purpose Xbox 360 -> PC shader verification.
//!
//! Supports both **pixel shaders** (`oC0..oC3`, `kill_*`, `sc_output_screen_xy` / `front`, `tfetch2D`, `tfetchCube`,
//! `getWeights*`, `setTexLOD`) and **vertex shaders** (`vfetch`, `oPos`, `o0..o15`), plus full control flow
//! (`exec*`, `cond_exec*`, `cond_exec_pred*`, `cond_jmp`, `loop_start`/`loop_end` with `aL`, `cond_call`/`return`,
//! `nop`, `alloc`, `mark_vs_fetch_done`).
//!
//! Performance & safety design:
//! - Register file is a single flat `Vec<[f32; 4]>` (`num_regs * n`), with reusable per-machine scratch buffers so
//!   steady-state ALU/fetch execution performs **zero heap allocations per instruction**.
//! - Exact Xenos/D3D9 numerical semantics: `0 * x = 0` (including `inf`/`NaN`), `saturate(NaN) = 0`, `max`/`min`
//!   pick `b` on `NaN`, `max4` propagates `NaN`, bilinear quantization uses `f32::round_ties_even`, and wrap
//!   addressing uses Euclidean modulo (`rem_euclid`).
//! - Killed pixels stop executing subsequent instructions and preserve their outputs at the point of kill (configurable
//!   via [`Options::continue_after_kill`] for legacy `xenos_exec.py` parity).

#![allow(
    clippy::indexing_slicing,
    reason = "pixel buffers are allocated to self.n, vector lanes to 4, and constant indices clamped to 0..=255"
)]
#![allow(
    clippy::float_cmp,
    reason = "Xenos GPU comparison ops (seq/sne/kill_eq/kills_one/rcp(1.0)/log(1.0)) require exact IEEE-754 equality"
)]
#![allow(
    clippy::cast_precision_loss,
    reason = "texture dimensions, vertex counts, and filter quantization bits are well below 2^23"
)]

use std::collections::BTreeMap;

use thiserror::Error;

use crate::xenos::{Alu, CfKind, CfType, Dest, Fetch, FetchKind, File, InstrKind, Operand, Shader, VertexFetch};

const FLT_MAX: f32 = 3.402_823_5e38;

/// Why shader execution failed.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("{0}")]
pub struct ExecError(pub String);

/// Texture addressing mode for [`Texture`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AddressMode {
    /// Wrap coordinates modulo texture size.
    #[default]
    Wrap,
    /// Clamp coordinates to `[0, size - 1]`.
    Clamp,
    /// Return `[0, 0, 0, 0]` outside `[0, size - 1]` (Direct3D `Load` semantics).
    Zero,
}

/// A decoded float32 texture as seen by the Xenos texture unit: `faces * height * width` RGBA texels in row-major
/// order (`texels[((face * height + y) * width + x)]`).
#[derive(Debug, Clone, PartialEq)]
pub struct Texture {
    /// Number of faces (`1` for 2D, `6` for cube).
    pub faces: usize,
    /// Texture width in texels.
    pub width: usize,
    /// Texture height in texels.
    pub height: usize,
    /// Row-major RGBA float32 texels (`faces * height * width` entries).
    pub texels: Vec<[f32; 4]>,
    /// Whether this texture is a cube map.
    pub cube: bool,
    /// Addressing mode (`Wrap`, `Clamp`, or `Zero`).
    pub address: AddressMode,
    /// Force point filtering (e.g. shadow maps).
    pub point: bool,
}

impl Texture {
    /// Creates a 2D texture from `width * height` RGBA float32 texels.
    #[must_use]
    pub fn new_2d(width: usize, height: usize, texels: Vec<[f32; 4]>, address: AddressMode, point: bool) -> Self {
        Self {
            faces: 1,
            width,
            height,
            texels,
            cube: false,
            address,
            point,
        }
    }

    /// Creates a 6-face cube texture from `6 * width * height` RGBA float32 texels.
    #[must_use]
    pub fn new_cube(width: usize, height: usize, texels: Vec<[f32; 4]>, point: bool) -> Self {
        Self {
            faces: 6,
            width,
            height,
            texels,
            cube: true,
            address: AddressMode::Clamp,
            point,
        }
    }

    #[inline]
    fn texel(&self, face: usize, x: usize, y: usize) -> [f32; 4] {
        let idx = (face * self.height + y) * self.width + x;
        self.texels.get(idx).copied().unwrap_or([0.0; 4])
    }
}

/// Raw big-endian vertex buffer bound to a Xenos vertex fetch constant (`vf` / `fetch_const`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VertexBuffer {
    /// Raw vertex stream bytes (Xbox 360 big-endian).
    pub data: Vec<u8>,
    /// Byte stride between consecutive vertices (used when `vfetch.stride == 0`).
    pub default_stride: u32,
}

/// Loop constant register (`i0..i31`) configuring `loop_start` / `loop_end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LoopConst {
    /// Number of iterations (`0..=255`).
    pub count: u32,
    /// Initial `aL` value.
    pub start: i32,
    /// Per-iteration `aL` increment (signed).
    pub step: i32,
}

/// Execution options for [`run_with`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Bilinear weight quantization bits (`8` matches Direct3D's texture units; `0` = exact).
    pub filter_bits: u32,
    /// When `true`, point fetches on clamped textures return `0` outside bounds (Direct3D `Load` semantics).
    pub zero_outside: bool,
    /// When `true`, [`ExecResult::snaps`] records register & alive state after every executed instruction.
    pub trace: bool,
    /// Legacy `xenos_exec.py` quirk: when `true`, killed pixels keep executing subsequent instructions and writing
    /// outputs. Default `false` matches Xenos hardware (killed pixels stop executing and preserve prior outputs).
    pub continue_after_kill: bool,
    /// Bitmask of boolean constants `b0..b31` for `cond_exec` and `cond_jmp` (`CfType::Conditional`).
    pub bool_consts: u32,
    /// Loop constants keyed by `loop_id` for `loop_start` / `loop_end` and `aL`-relative constant reads.
    pub loop_consts: BTreeMap<u32, LoopConst>,
    /// Vertex buffers keyed by Xenos `fetch_const` index (or D3D `vf = 95 - fetch_const`, checked as fallback).
    pub vertex_buffers: BTreeMap<u32, VertexBuffer>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            filter_bits: 8,
            zero_outside: false,
            trace: false,
            continue_after_kill: false,
            bool_consts: 0,
            loop_consts: BTreeMap::new(),
            vertex_buffers: BTreeMap::new(),
        }
    }
}

/// State snapshot recorded after an instruction when [`Options::trace`] is enabled.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// Number of temporary registers in `regs`.
    pub num_regs: usize,
    /// Number of pixels/vertices per register in `regs`.
    pub n: usize,
    /// Flat temporary registers indexed as `regs[reg * n + pixel]`.
    pub regs: Vec<[f32; 4]>,
    /// Per-pixel survival mask.
    pub alive: Vec<bool>,
    /// Per-pixel execution mask of the instruction that just ran.
    pub last_mask: Vec<bool>,
}

impl Snapshot {
    /// Reads register `reg` for pixel/vertex `pixel`.
    #[must_use]
    pub fn reg(&self, reg: usize, pixel: usize) -> Option<[f32; 4]> {
        self.regs.get(reg.checked_mul(self.n)?.checked_add(pixel)?).copied()
    }
}

/// Output of [`run`] / [`run_with`].
#[derive(Debug, Clone, PartialEq)]
pub struct ExecResult {
    /// Pixel shader colour target outputs (`oC0..oC3`), each `Vec<[f32; 4]>` of length `n`.
    pub outputs: BTreeMap<u32, Vec<[f32; 4]>>,
    /// Vertex shader position export (`oPos`), populated for vertex shaders.
    pub pos: Option<Vec<[f32; 4]>>,
    /// Vertex shader interpolator exports (`o0..o15`), populated for vertex shaders.
    pub interpolators: BTreeMap<u32, Vec<[f32; 4]>>,
    /// Per-pixel survival mask (`true` = not killed).
    pub alive: Vec<bool>,
    /// Optional per-instruction snapshots keyed by instruction address (populated when `options.trace` is `true`).
    pub snaps: Option<BTreeMap<u32, Snapshot>>,
}

#[inline]
fn xmul(a: f32, b: f32) -> f32 {
    if a == 0.0 || b == 0.0 { 0.0 } else { a * b }
}

#[inline]
fn xsat(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}

#[inline]
fn xmax(a: f32, b: f32) -> f32 {
    if a >= b { a } else { b }
}

#[inline]
fn xmin(a: f32, b: f32) -> f32 {
    if a < b { a } else { b }
}

#[inline]
fn xmax4(v: [f32; 4]) -> f32 {
    if v.iter().any(|x| x.is_nan()) {
        return f32::NAN;
    }
    let mut m = v[0];
    for &x in &v[1..] {
        if x > m {
            m = x;
        }
    }
    m
}

#[inline]
fn comp_index(ch: u8) -> usize {
    match ch {
        b'y' => 1,
        b'z' => 2,
        b'w' => 3,
        _ => 0,
    }
}

/// IEEE 754 binary16 -> binary32.
fn f16_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits >> 15) << 31;
    let exp = u32::from((bits >> 10) & 0x1f);
    let frac = u32::from(bits & 0x3ff);
    let fbits = if exp == 0 {
        if frac == 0 {
            sign
        } else {
            let mut m = frac;
            let mut e: i32 = -14;
            while (m & 0x400) == 0 {
                m <<= 1;
                e -= 1;
            }
            m &= 0x3ff;
            sign | (((e + 127) as u32) << 23) | (m << 13)
        }
    } else if exp == 31 {
        sign | (0xff << 23) | (frac << 13)
    } else {
        sign | ((exp + 112) << 23) | (frac << 13)
    };
    f32::from_bits(fbits)
}

#[derive(Debug, Clone, Copy)]
struct LoopFrame {
    loop_id: u32,
    remaining: u32,
    step: i32,
    al: i32,
}

#[derive(Default)]
struct SideEffects {
    has_p0: bool,
    has_a0: bool,
    has_kill: bool,
    keep_ps: bool,
}

struct Machine<'a> {
    sh: &'a Shader,
    n: usize,
    num_regs: usize,
    /// Flat register file: `r[reg * n + pixel]`.
    r: Vec<[f32; 4]>,
    c: [[f32; 4]; 256],
    ps: Vec<f32>,
    p0: Vec<bool>,
    a0: Vec<i64>,
    al: Vec<i32>,
    alive: Vec<bool>,
    out: BTreeMap<u32, Vec<[f32; 4]>>,
    out_pos: Option<Vec<[f32; 4]>>,
    out_interp: BTreeMap<u32, Vec<[f32; 4]>>,
    tex: &'a BTreeMap<u32, Texture>,
    tex_lod: Vec<f32>,
    last_mask: Vec<bool>,
    snaps: Option<BTreeMap<u32, Snapshot>>,
    opts: &'a Options,
    // Reusable scratch buffers (zero heap allocations during steady-state ALU/fetch execution)
    op_bufs: [Vec<[f32; 4]>; 3],
    s_a: Vec<f32>,
    s_b: Vec<f32>,
    vres: Vec<[f32; 4]>,
    sres: Vec<f32>,
    mask_buf: Vec<bool>,
    p0_buf: Vec<bool>,
    a0_buf: Vec<i64>,
    kill_buf: Vec<bool>,
    fetch_src: Vec<[f32; 4]>,
    fetch_val: Vec<[f32; 4]>,
}

impl<'a> Machine<'a> {
    fn new(
        sh: &'a Shader,
        n: usize,
        consts: &BTreeMap<u32, [f32; 4]>,
        textures: &'a BTreeMap<u32, Texture>,
        opts: &'a Options,
    ) -> Self {
        let temps = if sh.is_pixel {
            ((sh.header.program_cntl >> 8) & 0x3f) + 1
        } else {
            (sh.header.program_cntl & 0x3f) + 1
        } as usize;
        let num_regs = temps.max(64);
        let r = vec![[0.0; 4]; num_regs * n];
        let mut c = [[0.0; 4]; 256];
        for (&k, &v) in consts {
            if let Some(slot) = c.get_mut(k as usize) {
                *slot = v;
            }
        }
        for &(k, v) in &sh.literals {
            if let Ok(idx) = usize::try_from(k)
                && let Some(slot) = c.get_mut(idx)
            {
                *slot = v;
            }
        }
        Self {
            sh,
            n,
            num_regs,
            r,
            c,
            ps: vec![0.0; n],
            p0: vec![false; n],
            a0: vec![0; n],
            al: vec![0; n],
            alive: vec![true; n],
            out: BTreeMap::new(),
            out_pos: (!sh.is_pixel).then(|| vec![[0.0; 4]; n]),
            out_interp: BTreeMap::new(),
            tex: textures,
            tex_lod: vec![0.0; n],
            last_mask: vec![false; n],
            snaps: opts.trace.then(BTreeMap::new),
            opts,
            op_bufs: [vec![[0.0; 4]; n], vec![[0.0; 4]; n], vec![[0.0; 4]; n]],
            s_a: vec![0.0; n],
            s_b: vec![0.0; n],
            vres: vec![[0.0; 4]; n],
            sres: vec![0.0; n],
            mask_buf: vec![false; n],
            p0_buf: vec![false; n],
            a0_buf: vec![0; n],
            kill_buf: vec![false; n],
            fetch_src: vec![[0.0; 4]; n],
            fetch_val: vec![[0.0; 4]; n],
        }
    }

    #[inline]
    fn reg_range(&self, reg: usize) -> Result<std::ops::Range<usize>, ExecError> {
        if reg >= self.num_regs {
            return Err(ExecError(format!("temp register r{reg} out of range")));
        }
        let start = reg * self.n;
        Ok(start..start + self.n)
    }

    fn read_into(&self, o: &Operand, dst_buf: &mut [[f32; 4]]) -> Result<(), ExecError> {
        let sw_bytes = o.swizzle().as_bytes();
        let sw = [
            comp_index(sw_bytes.first().copied().unwrap_or(b'x')),
            comp_index(sw_bytes.get(1).copied().unwrap_or(b'y')),
            comp_index(sw_bytes.get(2).copied().unwrap_or(b'z')),
            comp_index(sw_bytes.get(3).copied().unwrap_or(b'w')),
        ];
        match o.file {
            File::Temp => {
                if o.rel.is_some() {
                    return Err(ExecError("relative temp addressing".into()));
                }
                let reg = &self.r[self.reg_range(o.index as usize)?];
                for (dst, src) in dst_buf.iter_mut().zip(reg.iter()) {
                    for k in 0..4 {
                        let mut v = src[sw[k]];
                        if o.abs {
                            v = v.abs();
                        }
                        if o.negate {
                            v = -v;
                        }
                        dst[k] = v;
                    }
                }
            }
            File::Const | File::Export(_) => match o.rel {
                Some("a0") => {
                    for (p, dst) in dst_buf.iter_mut().enumerate() {
                        let idx = (i64::from(o.index) + self.a0[p]).clamp(0, 255) as usize;
                        let src = self.c[idx];
                        for k in 0..4 {
                            let mut v = src[sw[k]];
                            if o.abs {
                                v = v.abs();
                            }
                            if o.negate {
                                v = -v;
                            }
                            dst[k] = v;
                        }
                    }
                }
                Some("aL") => {
                    for (p, dst) in dst_buf.iter_mut().enumerate() {
                        let idx = (i64::from(o.index) + i64::from(self.al[p])).clamp(0, 255) as usize;
                        let src = self.c[idx];
                        for k in 0..4 {
                            let mut v = src[sw[k]];
                            if o.abs {
                                v = v.abs();
                            }
                            if o.negate {
                                v = -v;
                            }
                            dst[k] = v;
                        }
                    }
                }
                Some(other) => return Err(ExecError(format!("unsupported relative constant addressing {other}"))),
                None => {
                    let src = self.c.get(o.index as usize).copied().unwrap_or([0.0; 4]);
                    let mut val = [0.0; 4];
                    for k in 0..4 {
                        let mut v = src[sw[k]];
                        if o.abs {
                            v = v.abs();
                        }
                        if o.negate {
                            v = -v;
                        }
                        val[k] = v;
                    }
                    dst_buf.fill(val);
                }
            },
        }
        Ok(())
    }

    fn comp_into(&self, o: &Operand, ch: u8, dst_buf: &mut [f32]) -> Result<(), ExecError> {
        let lane = comp_index(ch);
        match o.file {
            File::Temp => {
                let reg = &self.r[self.reg_range(o.index as usize)?];
                for (dst, src) in dst_buf.iter_mut().zip(reg.iter()) {
                    let mut v = src[lane];
                    if o.abs {
                        v = v.abs();
                    }
                    if o.negate {
                        v = -v;
                    }
                    *dst = v;
                }
            }
            File::Const | File::Export(_) => match o.rel {
                Some("a0") => {
                    for (p, dst) in dst_buf.iter_mut().enumerate() {
                        let idx = (i64::from(o.index) + self.a0[p]).clamp(0, 255) as usize;
                        let mut v = self.c[idx][lane];
                        if o.abs {
                            v = v.abs();
                        }
                        if o.negate {
                            v = -v;
                        }
                        *dst = v;
                    }
                }
                Some("aL") => {
                    for (p, dst) in dst_buf.iter_mut().enumerate() {
                        let idx = (i64::from(o.index) + i64::from(self.al[p])).clamp(0, 255) as usize;
                        let mut v = self.c[idx][lane];
                        if o.abs {
                            v = v.abs();
                        }
                        if o.negate {
                            v = -v;
                        }
                        *dst = v;
                    }
                }
                Some(other) => return Err(ExecError(format!("unsupported relative constant addressing {other}"))),
                None => {
                    let mut v = self.c.get(o.index as usize).copied().unwrap_or([0.0; 4])[lane];
                    if o.abs {
                        v = v.abs();
                    }
                    if o.negate {
                        v = -v;
                    }
                    dst_buf.fill(v);
                }
            },
        }
        Ok(())
    }

    fn load_scalar_in(&mut self, ins: &Alu) -> Result<(), ExecError> {
        let mut s_a = std::mem::take(&mut self.s_a);
        let mut s_b = std::mem::take(&mut self.s_b);
        let res = match ins.scalar_operands.as_slice() {
            [o0, o1, ..] => {
                let c0 = o0
                    .components
                    .as_deref()
                    .unwrap_or("x")
                    .as_bytes()
                    .first()
                    .copied()
                    .unwrap_or(b'x');
                let c1 = o1
                    .components
                    .as_deref()
                    .unwrap_or("x")
                    .as_bytes()
                    .first()
                    .copied()
                    .unwrap_or(b'x');
                self.comp_into(o0, c0, &mut s_a)
                    .and_then(|()| self.comp_into(o1, c1, &mut s_b))
            }
            [o] => {
                let cs = o.components.as_deref().unwrap_or("x").as_bytes();
                let r0 = self.comp_into(o, cs.first().copied().unwrap_or(b'x'), &mut s_a);
                if let Some(&c1) = cs.get(1) {
                    r0.and_then(|()| self.comp_into(o, c1, &mut s_b))
                } else {
                    s_b.copy_from_slice(&s_a);
                    r0
                }
            }
            [] => {
                s_a.fill(0.0);
                s_b.fill(0.0);
                Ok(())
            }
        };
        self.s_a = s_a;
        self.s_b = s_b;
        res
    }

    #[allow(clippy::too_many_lines)]
    fn vector(&mut self, ins: &Alu) -> Result<SideEffects, ExecError> {
        let op = ins.vector_op();
        let need = match op {
            "mad" | "cndeq" | "cndge" | "cndgt" | "dp2add" => 3,
            "frc" | "trunc" | "floor" | "max4" => 1,
            _ => 2,
        };
        if ins.vector_operands.len() < need {
            return Err(ExecError(format!(
                "vector op {op}: expected {need} operands, got {}",
                ins.vector_operands.len()
            )));
        }
        let mut op_bufs = std::mem::take(&mut self.op_bufs);
        for (i, o) in ins.vector_operands.iter().take(need).enumerate() {
            if let Err(e) = self.read_into(o, &mut op_bufs[i]) {
                self.op_bufs = op_bufs;
                return Err(e);
            }
        }
        let [a0_op, a1_op, a2_op] = &op_bufs;
        let r = &mut self.vres;
        let mut fx = SideEffects::default();
        let res = match op {
            "add" => {
                for p in 0..self.n {
                    for k in 0..4 {
                        r[p][k] = a0_op[p][k] + a1_op[p][k];
                    }
                }
                Ok(())
            }
            "mul" => {
                for p in 0..self.n {
                    for k in 0..4 {
                        r[p][k] = xmul(a0_op[p][k], a1_op[p][k]);
                    }
                }
                Ok(())
            }
            "max" => {
                for p in 0..self.n {
                    for k in 0..4 {
                        r[p][k] = xmax(a0_op[p][k], a1_op[p][k]);
                    }
                }
                Ok(())
            }
            "min" => {
                for p in 0..self.n {
                    for k in 0..4 {
                        r[p][k] = xmin(a0_op[p][k], a1_op[p][k]);
                    }
                }
                Ok(())
            }
            "seq" | "sgt" | "sge" | "sne" => {
                for p in 0..self.n {
                    for k in 0..4 {
                        let c = match op {
                            "seq" => a0_op[p][k] == a1_op[p][k],
                            "sgt" => a0_op[p][k] > a1_op[p][k],
                            "sge" => a0_op[p][k] >= a1_op[p][k],
                            _ => a0_op[p][k] != a1_op[p][k],
                        };
                        r[p][k] = if c { 1.0 } else { 0.0 };
                    }
                }
                Ok(())
            }
            "frc" => {
                for p in 0..self.n {
                    for k in 0..4 {
                        r[p][k] = a0_op[p][k] - a0_op[p][k].floor();
                    }
                }
                Ok(())
            }
            "trunc" => {
                for p in 0..self.n {
                    for k in 0..4 {
                        r[p][k] = a0_op[p][k].trunc();
                    }
                }
                Ok(())
            }
            "floor" => {
                for p in 0..self.n {
                    for k in 0..4 {
                        r[p][k] = a0_op[p][k].floor();
                    }
                }
                Ok(())
            }
            "mad" => {
                for p in 0..self.n {
                    for k in 0..4 {
                        r[p][k] = xmul(a0_op[p][k], a1_op[p][k]) + a2_op[p][k];
                    }
                }
                Ok(())
            }
            "cndeq" | "cndge" | "cndgt" => {
                for p in 0..self.n {
                    for k in 0..4 {
                        let c = match op {
                            "cndeq" => a0_op[p][k] == 0.0,
                            "cndge" => a0_op[p][k] >= 0.0,
                            _ => a0_op[p][k] > 0.0,
                        };
                        r[p][k] = if c { a1_op[p][k] } else { a2_op[p][k] };
                    }
                }
                Ok(())
            }
            "dp4" => {
                for p in 0..self.n {
                    let s = (xmul(a0_op[p][0], a1_op[p][0]) + xmul(a0_op[p][1], a1_op[p][1]))
                        + (xmul(a0_op[p][2], a1_op[p][2]) + xmul(a0_op[p][3], a1_op[p][3]));
                    r[p] = [s; 4];
                }
                Ok(())
            }
            "dp3" => {
                for p in 0..self.n {
                    let s = xmul(a0_op[p][0], a1_op[p][0])
                        + xmul(a0_op[p][1], a1_op[p][1])
                        + xmul(a0_op[p][2], a1_op[p][2]);
                    r[p] = [s; 4];
                }
                Ok(())
            }
            "dp2add" => {
                for p in 0..self.n {
                    let s = xmul(a0_op[p][0], a1_op[p][0]) + xmul(a0_op[p][1], a1_op[p][1]) + a2_op[p][0];
                    r[p] = [s; 4];
                }
                Ok(())
            }
            "cube" => {
                for p in 0..self.n {
                    r[p] = Self::cube(a0_op[p]);
                }
                Ok(())
            }
            "max4" => {
                for p in 0..self.n {
                    r[p] = [xmax4(a0_op[p]); 4];
                }
                Ok(())
            }
            "setp_eq_push" | "setp_ne_push" | "setp_gt_push" | "setp_ge_push" => {
                for p in 0..self.n {
                    let cmp_w = match op {
                        "setp_eq_push" => a1_op[p][3] == 0.0,
                        "setp_ne_push" => a1_op[p][3] != 0.0,
                        "setp_gt_push" => a1_op[p][3] > 0.0,
                        _ => a1_op[p][3] >= 0.0,
                    };
                    let cmp_x = match op {
                        "setp_eq_push" => a1_op[p][0] == 0.0,
                        "setp_ne_push" => a1_op[p][0] != 0.0,
                        "setp_gt_push" => a1_op[p][0] > 0.0,
                        _ => a1_op[p][0] >= 0.0,
                    };
                    self.p0_buf[p] = (a0_op[p][3] == 0.0) && cmp_w;
                    let v = if (a0_op[p][0] == 0.0) && cmp_x {
                        0.0
                    } else {
                        a0_op[p][0] + 1.0
                    };
                    r[p] = [v; 4];
                }
                fx.has_p0 = true;
                Ok(())
            }
            "kill_eq" | "kill_gt" | "kill_ge" | "kill_ne" => {
                for p in 0..self.n {
                    let k = (0..4).any(|i| match op {
                        "kill_eq" => a0_op[p][i] == a1_op[p][i],
                        "kill_gt" => a0_op[p][i] > a1_op[p][i],
                        "kill_ge" => a0_op[p][i] >= a1_op[p][i],
                        _ => a0_op[p][i] != a1_op[p][i],
                    });
                    self.kill_buf[p] = k;
                    r[p] = [if k { 1.0 } else { 0.0 }; 4];
                }
                fx.has_kill = true;
                Ok(())
            }
            "dst" => {
                for p in 0..self.n {
                    r[p] = [1.0, xmul(a0_op[p][1], a1_op[p][1]), a0_op[p][2], a1_op[p][3]];
                }
                Ok(())
            }
            "maxa" => {
                for p in 0..self.n {
                    self.a0_buf[p] = ((a0_op[p][3] + 0.5).floor().clamp(-256.0, 255.0)) as i64;
                    for k in 0..4 {
                        r[p][k] = xmax(a0_op[p][k], a1_op[p][k]);
                    }
                }
                fx.has_a0 = true;
                Ok(())
            }
            _ => Err(ExecError(format!("vector op {op}"))),
        };
        self.op_bufs = op_bufs;
        res.map(|()| fx)
    }

    fn cube(s0: [f32; 4]) -> [f32; 4] {
        let (x, y, z) = (s0[2], s0[3], s0[0]);
        let (ax, ay, az) = (x.abs(), y.abs(), z.abs());
        let zmaj = az >= ax && az >= ay;
        let ymaj = !zmaj && ay >= ax;
        let tc = if zmaj {
            -y
        } else if ymaj {
            if y < 0.0 { -z } else { z }
        } else {
            -y
        };
        let sc = if zmaj {
            if z < 0.0 { -x } else { x }
        } else if ymaj {
            x
        } else if x < 0.0 {
            z
        } else {
            -z
        };
        let ma = if zmaj {
            2.0 * z
        } else if ymaj {
            2.0 * y
        } else {
            2.0 * x
        };
        let fid = if zmaj {
            if z < 0.0 { 5.0 } else { 4.0 }
        } else if ymaj {
            if y < 0.0 { 3.0 } else { 2.0 }
        } else if x < 0.0 {
            1.0
        } else {
            0.0
        };
        [tc, sc, ma, fid]
    }

    #[allow(clippy::too_many_lines)]
    fn scalar(&mut self, ins: &Alu) -> Result<SideEffects, ExecError> {
        let op = ins.scalar_op();
        self.load_scalar_in(ins)?;
        let a = &self.s_a;
        let b = &self.s_b;
        let r = &mut self.sres;
        let mut fx = SideEffects::default();
        match op {
            "adds" | "addsc" => {
                for p in 0..self.n {
                    r[p] = a[p] + b[p];
                }
            }
            "adds_prev" => {
                for p in 0..self.n {
                    r[p] = a[p] + self.ps[p];
                }
            }
            "muls" | "mulsc" => {
                for p in 0..self.n {
                    r[p] = xmul(a[p], b[p]);
                }
            }
            "muls_prev" => {
                for p in 0..self.n {
                    r[p] = xmul(a[p], self.ps[p]);
                }
            }
            "muls_prev2" => {
                for p in 0..self.n {
                    let ps = self.ps[p];
                    let bad = ps == -FLT_MAX || !ps.is_finite() || !b[p].is_finite() || b[p] <= 0.0;
                    r[p] = if bad { -FLT_MAX } else { xmul(a[p], ps) };
                }
            }
            "maxs" => {
                for p in 0..self.n {
                    r[p] = xmax(a[p], b[p]);
                }
            }
            "mins" => {
                for p in 0..self.n {
                    r[p] = xmin(a[p], b[p]);
                }
            }
            "seqs" | "sgts" | "sges" | "snes" => {
                for p in 0..self.n {
                    let c = match op {
                        "seqs" => a[p] == 0.0,
                        "sgts" => a[p] > 0.0,
                        "sges" => a[p] >= 0.0,
                        _ => a[p] != 0.0,
                    };
                    r[p] = if c { 1.0 } else { 0.0 };
                }
            }
            "frcs" => {
                for p in 0..self.n {
                    r[p] = a[p] - a[p].floor();
                }
            }
            "truncs" => {
                for p in 0..self.n {
                    r[p] = a[p].trunc();
                }
            }
            "floors" => {
                for p in 0..self.n {
                    r[p] = a[p].floor();
                }
            }
            "exp" => {
                for p in 0..self.n {
                    r[p] = if a[p] == 0.0 { 1.0 } else { a[p].exp2() };
                }
            }
            "log" | "logc" => {
                for p in 0..self.n {
                    let mut v = if a[p] == 1.0 { 0.0 } else { a[p].log2() };
                    if op == "logc" && v == f32::NEG_INFINITY {
                        v = -FLT_MAX;
                    }
                    r[p] = v;
                }
            }
            "rcp" | "rcpc" | "rcpf" => {
                for p in 0..self.n {
                    let mut v = if a[p] == 1.0 { 1.0 } else { 1.0 / a[p] };
                    if op == "rcpc" && !v.is_nan() {
                        v = v.clamp(-FLT_MAX, FLT_MAX);
                    } else if op == "rcpf" && v.is_infinite() {
                        v = 0.0f32.copysign(v);
                    }
                    r[p] = v;
                }
            }
            "rsq" | "rsqc" | "rsqf" => {
                for p in 0..self.n {
                    let mut v = if a[p] == 1.0 { 1.0 } else { 1.0 / a[p].sqrt() };
                    if op == "rsqc" && !v.is_nan() {
                        v = v.clamp(-FLT_MAX, FLT_MAX);
                    } else if op == "rsqf" && v.is_infinite() {
                        v = 0.0f32.copysign(v);
                    }
                    r[p] = v;
                }
            }
            "maxas" | "maxasf" => {
                let bias = if op == "maxas" { 0.5 } else { 0.0 };
                for p in 0..self.n {
                    self.a0_buf[p] = ((a[p] + bias).floor().clamp(-256.0, 255.0)) as i64;
                    r[p] = xmax(a[p], b[p]);
                }
                fx.has_a0 = true;
            }
            "subs" | "subsc" => {
                for p in 0..self.n {
                    r[p] = a[p] - b[p];
                }
            }
            "subs_prev" => {
                for p in 0..self.n {
                    r[p] = a[p] - self.ps[p];
                }
            }
            "setp_eq" | "setp_ne" | "setp_gt" | "setp_ge" => {
                for p in 0..self.n {
                    let c = match op {
                        "setp_eq" => a[p] == 0.0,
                        "setp_ne" => a[p] != 0.0,
                        "setp_gt" => a[p] > 0.0,
                        _ => a[p] >= 0.0,
                    };
                    self.p0_buf[p] = c;
                    r[p] = if c { 0.0 } else { 1.0 };
                }
                fx.has_p0 = true;
            }
            "setp_inv" => {
                for p in 0..self.n {
                    let c = a[p] == 1.0;
                    self.p0_buf[p] = c;
                    r[p] = if c {
                        0.0
                    } else if a[p] == 0.0 {
                        1.0
                    } else {
                        a[p]
                    };
                }
                fx.has_p0 = true;
            }
            "setp_pop" => {
                for p in 0..self.n {
                    let c = a[p] - 1.0 <= 0.0;
                    self.p0_buf[p] = c;
                    r[p] = if c { 0.0 } else { a[p] - 1.0 };
                }
                fx.has_p0 = true;
            }
            "setp_clr" => {
                self.p0_buf.fill(false);
                r.fill(FLT_MAX);
                fx.has_p0 = true;
            }
            "setp_rstr" => {
                for p in 0..self.n {
                    let c = a[p] == 0.0;
                    self.p0_buf[p] = c;
                    r[p] = if c { 0.0 } else { a[p] };
                }
                fx.has_p0 = true;
            }
            "kills_eq" | "kills_gt" | "kills_ge" | "kills_ne" | "kills_one" => {
                for p in 0..self.n {
                    let k = match op {
                        "kills_eq" => a[p] == 0.0,
                        "kills_gt" => a[p] > 0.0,
                        "kills_ge" => a[p] >= 0.0,
                        "kills_ne" => a[p] != 0.0,
                        _ => a[p] == 1.0,
                    };
                    self.kill_buf[p] = k;
                    r[p] = if k { 1.0 } else { 0.0 };
                }
                fx.has_kill = true;
            }
            "sqrt" => {
                for p in 0..self.n {
                    r[p] = a[p].sqrt();
                }
            }
            "sin" => {
                for p in 0..self.n {
                    r[p] = a[p].sin();
                }
            }
            "cos" => {
                for p in 0..self.n {
                    r[p] = a[p].cos();
                }
            }
            "retain_prev" => {
                r.copy_from_slice(&self.ps);
                fx.keep_ps = true;
            }
            _ => return Err(ExecError(format!("scalar op {op}"))),
        }
        Ok(fx)
    }

    fn dest_slice_mut(&mut self, dest: &Dest) -> Result<Option<&mut [[f32; 4]]>, ExecError> {
        match dest.file {
            File::Temp => {
                if dest.rel.is_some() {
                    return Err(ExecError("relative destination".into()));
                }
                let range = self.reg_range(dest.index.unwrap_or(0) as usize)?;
                Ok(Some(&mut self.r[range]))
            }
            File::Export("oC") => {
                let idx = dest.index.unwrap_or(0);
                let n = self.n;
                Ok(Some(self.out.entry(idx).or_insert_with(|| vec![[0.0; 4]; n])))
            }
            File::Export("oPos") => {
                let n = self.n;
                Ok(Some(self.out_pos.get_or_insert_with(|| vec![[0.0; 4]; n])))
            }
            File::Export("o") => {
                let idx = dest.index.unwrap_or(0);
                let n = self.n;
                Ok(Some(self.out_interp.entry(idx).or_insert_with(|| vec![[0.0; 4]; n])))
            }
            _ => Ok(None),
        }
    }

    fn write_vector(&mut self, dest: &Dest) -> Result<(), ExecError> {
        if dest.writes_nothing() {
            return Ok(());
        }
        let n = self.n;
        let vres = std::mem::take(&mut self.vres);
        let mask = std::mem::take(&mut self.mask_buf);
        let res = match self.dest_slice_mut(dest) {
            Ok(Some(tgt)) => {
                for (k, ch) in dest.write.bytes().enumerate().take(4) {
                    if ch == b'_' {
                        continue;
                    }
                    for p in 0..n {
                        if !mask[p] {
                            continue;
                        }
                        tgt[p][k] = match ch {
                            b'0' => 0.0,
                            b'1' => 1.0,
                            _ => {
                                let raw = vres[p][k];
                                if dest.saturate { xsat(raw) } else { raw }
                            }
                        };
                    }
                }
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(e) => Err(e),
        };
        self.vres = vres;
        self.mask_buf = mask;
        res
    }

    fn write_scalar(&mut self, dest: &Dest) -> Result<(), ExecError> {
        if dest.writes_nothing() {
            return Ok(());
        }
        let n = self.n;
        let sres = std::mem::take(&mut self.sres);
        let mask = std::mem::take(&mut self.mask_buf);
        let res = match self.dest_slice_mut(dest) {
            Ok(Some(tgt)) => {
                for (k, ch) in dest.write.bytes().enumerate().take(4) {
                    if ch == b'_' {
                        continue;
                    }
                    for p in 0..n {
                        if !mask[p] {
                            continue;
                        }
                        tgt[p][k] = match ch {
                            b'0' => 0.0,
                            b'1' => 1.0,
                            _ => {
                                let raw = sres[p];
                                if dest.saturate { xsat(raw) } else { raw }
                            }
                        };
                    }
                }
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(e) => Err(e),
        };
        self.sres = sres;
        self.mask_buf = mask;
        res
    }

    fn build_mask(&mut self, base_mask: &[bool], predicated: bool, pred_condition: bool) -> bool {
        let mut any = false;
        let gate_alive = !self.opts.continue_after_kill;
        for p in 0..self.n {
            let mut m = base_mask[p];
            if gate_alive {
                m &= self.alive[p];
            }
            if predicated {
                m &= self.p0[p] == pred_condition;
            }
            self.mask_buf[p] = m;
            self.last_mask[p] = m;
            any |= m;
        }
        any
    }

    fn alu(&mut self, ins: &Alu, base_mask: &[bool]) -> Result<(), ExecError> {
        if !self.build_mask(base_mask, ins.predicated, ins.pred_condition) {
            return Ok(());
        }
        let vfx = if ins.vector_nop {
            SideEffects::default()
        } else {
            self.vector(ins)?
        };
        let sfx = if ins.scalar_nop {
            SideEffects {
                keep_ps: true,
                ..SideEffects::default()
            }
        } else {
            self.scalar(ins)?
        };
        if !ins.vector_nop {
            self.write_vector(&ins.vector_dest)?;
        }
        if !ins.scalar_nop {
            self.write_scalar(&ins.scalar_dest)?;
            if !sfx.keep_ps {
                for p in 0..self.n {
                    if self.mask_buf[p] {
                        self.ps[p] = self.sres[p];
                    }
                }
            }
        }
        if vfx.has_p0 || sfx.has_p0 {
            for p in 0..self.n {
                if self.mask_buf[p] {
                    self.p0[p] = self.p0_buf[p];
                }
            }
        }
        if vfx.has_a0 || sfx.has_a0 {
            for p in 0..self.n {
                if self.mask_buf[p] {
                    self.a0[p] = self.a0_buf[p];
                }
            }
        }
        if vfx.has_kill || sfx.has_kill {
            for p in 0..self.n {
                if self.mask_buf[p] && self.kill_buf[p] {
                    self.alive[p] = false;
                }
            }
        }
        Ok(())
    }

    fn decode_vfetch_element(vf: &VertexFetch, data: &[u8], byte_off: usize) -> Result<[f32; 4], ExecError> {
        let be16 = |o: usize| -> Result<u16, ExecError> {
            let s = data
                .get(o..o + 2)
                .ok_or_else(|| ExecError(format!("vfetch out of bounds at offset {o}")))?;
            Ok(u16::from_be_bytes([s[0], s[1]]))
        };
        let be32 = |o: usize| -> Result<u32, ExecError> {
            let s = data
                .get(o..o + 4)
                .ok_or_else(|| ExecError(format!("vfetch out of bounds at offset {o}")))?;
            Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
        };
        let norm_u = |v: u32, bits: u32| -> f32 {
            if vf.is_integer {
                v as f32
            } else {
                v as f32 / ((1u64 << bits) - 1) as f32
            }
        };
        let norm_i = |v: i32, bits: u32| -> f32 {
            if vf.is_integer {
                v as f32
            } else {
                let max = ((1i64 << (bits - 1)) - 1) as f32;
                (v as f32 / max).clamp(-1.0, 1.0)
            }
        };
        let mut out = match vf.format {
            6 => {
                // FMT_8_8_8_8
                let s = data
                    .get(byte_off..byte_off + 4)
                    .ok_or_else(|| ExecError(format!("vfetch out of bounds at offset {byte_off}")))?;
                if vf.is_signed {
                    std::array::from_fn(|i| norm_i(i32::from(s[i] as i8), 8))
                } else {
                    std::array::from_fn(|i| norm_u(u32::from(s[i]), 8))
                }
            }
            7 => {
                // FMT_2_10_10_10 (X=0..10, Y=10..20, Z=20..30, W=30..32)
                let w = be32(byte_off)?;
                if vf.is_signed {
                    let s10 = |x: u32| ((x << 22) as i32) >> 22;
                    let s2 = |x: u32| ((x << 30) as i32) >> 30;
                    [
                        norm_i(s10(w & 0x3ff), 10),
                        norm_i(s10((w >> 10) & 0x3ff), 10),
                        norm_i(s10((w >> 20) & 0x3ff), 10),
                        norm_i(s2((w >> 30) & 0x3), 2),
                    ]
                } else {
                    [
                        norm_u(w & 0x3ff, 10),
                        norm_u((w >> 10) & 0x3ff, 10),
                        norm_u((w >> 20) & 0x3ff, 10),
                        norm_u((w >> 30) & 0x3, 2),
                    ]
                }
            }
            25 => {
                // FMT_16_16
                let (a, b) = (be16(byte_off)?, be16(byte_off + 2)?);
                if vf.is_signed {
                    [norm_i(i32::from(a as i16), 16), norm_i(i32::from(b as i16), 16), 0.0, 1.0]
                } else {
                    [norm_u(u32::from(a), 16), norm_u(u32::from(b), 16), 0.0, 1.0]
                }
            }
            26 => {
                // FMT_16_16_16_16
                let vals = [
                    be16(byte_off)?,
                    be16(byte_off + 2)?,
                    be16(byte_off + 4)?,
                    be16(byte_off + 6)?,
                ];
                if vf.is_signed {
                    std::array::from_fn(|i| norm_i(i32::from(vals[i] as i16), 16))
                } else {
                    std::array::from_fn(|i| norm_u(u32::from(vals[i]), 16))
                }
            }
            31 => [f16_to_f32(be16(byte_off)?), f16_to_f32(be16(byte_off + 2)?), 0.0, 1.0],
            32 => [
                f16_to_f32(be16(byte_off)?),
                f16_to_f32(be16(byte_off + 2)?),
                f16_to_f32(be16(byte_off + 4)?),
                f16_to_f32(be16(byte_off + 6)?),
            ],
            33 => {
                let v = be32(byte_off)?;
                let f = if vf.is_signed { norm_i(v as i32, 32) } else { norm_u(v, 32) };
                [f, 0.0, 0.0, 1.0]
            }
            34 => {
                let (a, b) = (be32(byte_off)?, be32(byte_off + 4)?);
                if vf.is_signed {
                    [norm_i(a as i32, 32), norm_i(b as i32, 32), 0.0, 1.0]
                } else {
                    [norm_u(a, 32), norm_u(b, 32), 0.0, 1.0]
                }
            }
            35 => {
                let vals = [
                    be32(byte_off)?,
                    be32(byte_off + 4)?,
                    be32(byte_off + 8)?,
                    be32(byte_off + 12)?,
                ];
                if vf.is_signed {
                    std::array::from_fn(|i| norm_i(vals[i] as i32, 32))
                } else {
                    std::array::from_fn(|i| norm_u(vals[i], 32))
                }
            }
            36 => [f32::from_bits(be32(byte_off)?), 0.0, 0.0, 1.0],
            37 => [
                f32::from_bits(be32(byte_off)?),
                f32::from_bits(be32(byte_off + 4)?),
                0.0,
                1.0,
            ],
            38 => [
                f32::from_bits(be32(byte_off)?),
                f32::from_bits(be32(byte_off + 4)?),
                f32::from_bits(be32(byte_off + 8)?),
                f32::from_bits(be32(byte_off + 12)?),
            ],
            57 => [
                f32::from_bits(be32(byte_off)?),
                f32::from_bits(be32(byte_off + 4)?),
                f32::from_bits(be32(byte_off + 8)?),
                1.0,
            ],
            other => return Err(ExecError(format!("unsupported vfetch format FMT_{other}"))),
        };
        if vf.exp_adjust != 0 {
            let scale = (vf.exp_adjust as f32).exp2();
            for v in &mut out {
                *v *= scale;
            }
        }
        Ok(out)
    }

    #[allow(clippy::too_many_lines)]
    fn fetch(&mut self, ins: &Fetch, base_mask: &[bool]) -> Result<(), ExecError> {
        if !self.build_mask(base_mask, ins.predicated, ins.pred_condition) {
            return Ok(());
        }
        match ins.kind {
            FetchKind::Vertex(ref vf) => {
                let vb = self
                    .opts
                    .vertex_buffers
                    .get(&vf.fetch_const)
                    .or_else(|| u32::try_from(vf.vf()).ok().and_then(|k| self.opts.vertex_buffers.get(&k)))
                    .ok_or_else(|| ExecError(format!("no vertex buffer for fetch_const {} (vf{})", vf.fetch_const, vf.vf())))?;
                let src_range = self.reg_range(ins.src as usize)?;
                let lane = comp_index(vf.src_swizzle as u8);
                let stride = if vf.stride > 0 { vf.stride } else { vb.default_stride };
                for p in 0..self.n {
                    if !self.mask_buf[p] {
                        continue;
                    }
                    let raw_idx = self.r[src_range.start + p][lane];
                    let idx = if vf.is_index_rounded {
                        raw_idx.round_ties_even()
                    } else {
                        raw_idx.floor()
                    } as i64;
                    let byte_off = idx * i64::from(stride) * 4 + i64::from(vf.offset) * 4;
                    let byte_off = usize::try_from(byte_off)
                        .map_err(|_| ExecError(format!("negative vfetch byte offset {byte_off}")))?;
                    self.fetch_val[p] = Self::decode_vfetch_element(vf, &vb.data, byte_off)?;
                }
                let dst_range = self.reg_range(ins.dst as usize)?;
                for (k, ch) in ins.dst_swizzle.bytes().enumerate().take(4) {
                    if ch == b'_' || ch == b'?' {
                        continue;
                    }
                    for p in 0..self.n {
                        if !self.mask_buf[p] {
                            continue;
                        }
                        self.r[dst_range.start + p][k] = match ch {
                            b'0' => 0.0,
                            b'1' => 1.0,
                            _ => self.fetch_val[p][comp_index(ch)],
                        };
                    }
                }
                return Ok(());
            }
            FetchKind::Texture(ref tf) => {
                if ins.op == "setTexLOD" {
                    let ch = tf.src_swizzle.as_bytes().first().copied().unwrap_or(b'x');
                    let src_range = self.reg_range(ins.src as usize)?;
                    for p in 0..self.n {
                        if self.mask_buf[p] {
                            self.tex_lod[p] = match ch {
                                b'0' => 0.0,
                                b'1' => 1.0,
                                _ => self.r[src_range.start + p][comp_index(ch)],
                            };
                        }
                    }
                    return Ok(());
                }
                if !tf.has_dest {
                    return Ok(());
                }
                let t = self
                    .tex
                    .get(&tf.const_index)
                    .ok_or_else(|| ExecError(format!("no texture for tf{}", tf.const_index)))?;
                if t.width == 0 || t.height == 0 || t.faces == 0 {
                    return Err(ExecError(format!(
                        "texture tf{} has zero dimension ({}x{}, {} faces)",
                        tf.const_index, t.width, t.height, t.faces
                    )));
                }
                let src_range = self.reg_range(ins.src as usize)?;
                let sw_bytes = tf.src_swizzle.as_bytes();
                for p in 0..self.n {
                    let reg = self.r[src_range.start + p];
                    let mut s = [0.0; 4];
                    for (k, &ch) in sw_bytes.iter().enumerate().take(4) {
                        s[k] = match ch {
                            b'0' => 0.0,
                            b'1' => 1.0,
                            _ => reg[comp_index(ch)],
                        };
                    }
                    self.fetch_src[p] = s;
                }
                if tf.unnormalized && t.cube {
                    return Err(ExecError("unnormalized cube sampling is not supported".into()));
                }
                let off = [tf.offset_x as f32, tf.offset_y as f32];
                if ins.op.starts_with("getWeights") {
                    let (wf, hf) = (t.width as f32, t.height as f32);
                    for p in 0..self.n {
                        let s = self.fetch_src[p];
                        let fx = if tf.unnormalized { s[0] } else { s[0] * wf } - 0.5 + off[0];
                        let fy = if tf.unnormalized { s[1] } else { s[1] * hf } - 0.5 + off[1];
                        self.fetch_val[p] = [fx - fx.floor(), fy - fy.floor(), 0.0, 0.0];
                    }
                } else if ins.op == "tfetchCube" {
                    for p in 0..self.n {
                        if !self.mask_buf[p] {
                            continue;
                        }
                        let s = self.fetch_src[p];
                        let face = s[2].round_ties_even().clamp(0.0, 5.0) as usize;
                        self.fetch_val[p] = if face < t.faces {
                            self.sample_2d_one(t, face, AddressMode::Clamp, [s[0] - 1.0, s[1] - 1.0], [0.0, 0.0], t.point, false)
                        } else {
                            [0.0; 4]
                        };
                    }
                } else if ins.op == "tfetch2D" {
                    let point = tf.mag_filter == "point" && tf.min_filter == "point";
                    let addr = if point && self.opts.zero_outside && t.address == AddressMode::Clamp {
                        AddressMode::Zero
                    } else {
                        t.address
                    };
                    let use_point = point || t.point;
                    for p in 0..self.n {
                        if !self.mask_buf[p] {
                            continue;
                        }
                        let s = self.fetch_src[p];
                        self.fetch_val[p] =
                            self.sample_2d_one(t, 0, addr, [s[0], s[1]], off, use_point, tf.unnormalized);
                    }
                } else {
                    return Err(ExecError(format!("fetch {}", ins.op)));
                }
                let dst_range = self.reg_range(ins.dst as usize)?;
                for (k, ch) in ins.dst_swizzle.bytes().enumerate().take(4) {
                    if ch == b'_' || ch == b'?' {
                        continue;
                    }
                    for p in 0..self.n {
                        if !self.mask_buf[p] {
                            continue;
                        }
                        self.r[dst_range.start + p][k] = match ch {
                            b'0' => 0.0,
                            b'1' => 1.0,
                            _ => self.fetch_val[p][comp_index(ch)],
                        };
                    }
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn sample_2d_one(
        &self,
        t: &Texture,
        face: usize,
        addr: AddressMode,
        uv: [f32; 2],
        off: [f32; 2],
        point: bool,
        unnormalized: bool,
    ) -> [f32; 4] {
        let (w, h) = (t.width as i64, t.height as i64);
        let (wf, hf) = (t.width as f32, t.height as f32);
        let at = |ix: i64, iy: i64| -> [f32; 4] {
            match addr {
                AddressMode::Zero => {
                    if ix < 0 || ix >= w || iy < 0 || iy >= h {
                        [0.0; 4]
                    } else {
                        t.texel(face, ix as usize, iy as usize)
                    }
                }
                AddressMode::Clamp => t.texel(face, ix.clamp(0, w - 1) as usize, iy.clamp(0, h - 1) as usize),
                AddressMode::Wrap => t.texel(face, ix.rem_euclid(w) as usize, iy.rem_euclid(h) as usize),
            }
        };
        let bias = if point { 0.0 } else { 0.5 };
        let x = if unnormalized { uv[0] } else { uv[0] * wf } + off[0] - bias;
        let y = if unnormalized { uv[1] } else { uv[1] * hf } + off[1] - bias;
        if point {
            return at(x.floor() as i64, y.floor() as i64);
        }
        let x0f = x.floor();
        let y0f = y.floor();
        let mut fx = x - x0f;
        let mut fy = y - y0f;
        if self.opts.filter_bits > 0 {
            let qv = (1u32 << self.opts.filter_bits) as f32;
            fx = (fx * qv).round_ties_even() / qv;
            fy = (fy * qv).round_ties_even() / qv;
        }
        let (x0, y0) = (x0f as i64, y0f as i64);
        let c00 = at(x0, y0);
        let c10 = at(x0 + 1, y0);
        let c01 = at(x0, y0 + 1);
        let c11 = at(x0 + 1, y0 + 1);
        std::array::from_fn(|k| {
            (c00[k] * (1.0 - fx) + c10[k] * fx) * (1.0 - fy) + (c01[k] * (1.0 - fx) + c11[k] * fx) * fy
        })
    }

    #[inline]
    fn bool_const(&self, bool_index: u32) -> bool {
        bool_index < 32 && ((self.opts.bool_consts >> bool_index) & 1) != 0
    }

    #[allow(clippy::too_many_lines)]
    fn run(&mut self) -> Result<(), ExecError> {
        let cfs = &self.sh.cf;
        let by_index: BTreeMap<u32, usize> = cfs.iter().enumerate().map(|(k, c)| (c.index, k)).collect();
        let mut pc = vec![0usize; self.n];
        let mut done = vec![false; self.n];
        let mut call_stack: Vec<Vec<usize>> = vec![Vec::new(); self.n];
        let mut loop_stack: Vec<Vec<LoopFrame>> = vec![Vec::new(); self.n];
        let mut here = vec![false; self.n];
        let mut m = vec![false; self.n];
        let mut guard = 0usize;

        while done.iter().any(|&d| !d) {
            guard += 1;
            if guard > 100_000 {
                return Err(ExecError("control flow does not terminate".into()));
            }
            let Some(k) = pc.iter().zip(&done).filter(|(_, d)| !**d).map(|(&p, _)| p).min() else {
                break;
            };
            for p in 0..self.n {
                here[p] = !done[p] && pc[p] == k;
            }
            if k >= cfs.len() {
                for p in 0..self.n {
                    done[p] |= here[p];
                }
                continue;
            }
            let c = &cfs[k];
            let op = c.op();
            let nxt = k + 1;
            match c.kind {
                CfKind::Exec {
                    is_end,
                    bool_index,
                    condition,
                    ..
                } => {
                    m.copy_from_slice(&here);
                    if op.starts_with("cond_exec_pred") {
                        let cond = condition.unwrap_or(0) != 0;
                        for p in 0..self.n {
                            m[p] &= self.p0[p] == cond;
                        }
                    } else if op.starts_with("cond_exec") {
                        let cond = condition.unwrap_or(1) != 0;
                        let bv = self.bool_const(bool_index.unwrap_or(0));
                        if bv != cond {
                            m.fill(false);
                        }
                    }
                    if let Some(ref instr_idxs) = c.instructions {
                        for &idx in instr_idxs {
                            let ins = &self.sh.instructions[idx];
                            match ins.kind {
                                InstrKind::Alu(ref a) => self.alu(a, &m)?,
                                InstrKind::Fetch(ref f) => self.fetch(f, &m)?,
                            }
                            if let Some(ref mut snaps) = self.snaps {
                                snaps.insert(
                                    ins.address,
                                    Snapshot {
                                        num_regs: self.num_regs,
                                        n: self.n,
                                        regs: self.r.clone(),
                                        alive: self.alive.clone(),
                                        last_mask: self.last_mask.clone(),
                                    },
                                );
                            }
                        }
                    }
                    for p in 0..self.n {
                        if here[p] {
                            pc[p] = nxt;
                            if is_end {
                                done[p] = true;
                            }
                        }
                    }
                }
                CfKind::Jump {
                    address,
                    bool_index,
                    condition,
                    type_,
                    ..
                } if op == "cond_jmp" || op == "cond_call" => {
                    let &tgt = by_index
                        .get(&address)
                        .ok_or_else(|| ExecError(format!("jump to CF {address}: no such instruction")))?;
                    let is_call = op == "cond_call";
                    for p in 0..self.n {
                        if !here[p] {
                            continue;
                        }
                        let take = match type_ {
                            CfType::Unconditional => true,
                            CfType::Predicated => self.p0[p] == (condition != 0),
                            CfType::Conditional => self.bool_const(bool_index) == (condition != 0),
                        };
                        if take {
                            if is_call {
                                call_stack[p].push(nxt);
                            }
                            pc[p] = tgt;
                        } else {
                            pc[p] = nxt;
                        }
                    }
                }
                CfKind::Plain if op == "return" => {
                    for p in 0..self.n {
                        if !here[p] {
                            continue;
                        }
                        if let Some(ret_pc) = call_stack[p].pop() {
                            pc[p] = ret_pc;
                        } else {
                            done[p] = true;
                        }
                    }
                }
                CfKind::LoopStart { address, loop_id, .. } => {
                    let lc = self.opts.loop_consts.get(&loop_id).copied().unwrap_or_default();
                    let skip_tgt = by_index
                        .get(&address)
                        .map_or(cfs.len(), |&end_k| end_k + 1);
                    for p in 0..self.n {
                        if !here[p] {
                            continue;
                        }
                        if lc.count == 0 {
                            pc[p] = skip_tgt;
                        } else {
                            loop_stack[p].push(LoopFrame {
                                loop_id,
                                remaining: lc.count,
                                step: lc.step,
                                al: lc.start,
                            });
                            self.al[p] = lc.start;
                            pc[p] = nxt;
                        }
                    }
                }
                CfKind::LoopEnd {
                    address,
                    loop_id,
                    is_predicated_break,
                    condition,
                } => {
                    let &body_tgt = by_index
                        .get(&address)
                        .ok_or_else(|| ExecError(format!("loop_end target CF {address}: no such instruction")))?;
                    for p in 0..self.n {
                        if !here[p] {
                            continue;
                        }
                        let mut cont = false;
                        if let Some(frame) = loop_stack[p].last_mut()
                            && frame.loop_id == loop_id
                        {
                            let pred_break = is_predicated_break != 0 && (self.p0[p] == (condition != 0));
                            frame.remaining = frame.remaining.saturating_sub(1);
                            if !pred_break && frame.remaining > 0 {
                                frame.al = frame.al.wrapping_add(frame.step);
                                self.al[p] = frame.al;
                                cont = true;
                            } else {
                                loop_stack[p].pop();
                                self.al[p] = loop_stack[p].last().map_or(0, |f| f.al);
                            }
                        }
                        pc[p] = if cont { body_tgt } else { nxt };
                    }
                }
                CfKind::Alloc { .. } | CfKind::Plain if matches!(op, "nop" | "alloc" | "mark_vs_fetch_done") => {
                    for p in 0..self.n {
                        if here[p] {
                            pc[p] = nxt;
                        }
                    }
                }
                _ => return Err(ExecError(format!("control flow {op}"))),
            }
        }
        Ok(())
    }
}

/// Runs a decoded Xenos pixel or vertex shader on `n` pixels/vertices with default [`Options`].
///
/// # Errors
/// Returns [`ExecError`] if the shader executes an unsupported op or references a missing texture / vertex buffer.
pub fn run(
    sh: &Shader,
    inputs: &BTreeMap<u32, Vec<[f32; 4]>>,
    consts: &BTreeMap<u32, [f32; 4]>,
    textures: &BTreeMap<u32, Texture>,
    n: usize,
    pos: Option<&[[f32; 2]]>,
    front: Option<&[bool]>,
) -> Result<ExecResult, ExecError> {
    run_with(sh, inputs, consts, textures, n, pos, front, &Options::default())
}

/// Runs a decoded Xenos pixel or vertex shader on `n` pixels/vertices with explicit [`Options`].
///
/// For pixel shaders, `inputs` populates interpolator registers listed in `sh.header.interpolators`.
/// For vertex shaders, `inputs` populates initial temp registers (e.g. `r0` containing vertex indices for `vfetch`).
///
/// # Errors
/// Returns [`ExecError`] if an input length does not match `n` or execution encounters an unsupported op / missing
/// texture or vertex buffer.
#[allow(clippy::too_many_arguments, reason = "matches xenos_exec.py::run signature")]
pub fn run_with(
    sh: &Shader,
    inputs: &BTreeMap<u32, Vec<[f32; 4]>>,
    consts: &BTreeMap<u32, [f32; 4]>,
    textures: &BTreeMap<u32, Texture>,
    n: usize,
    pos: Option<&[[f32; 2]]>,
    front: Option<&[bool]>,
    opts: &Options,
) -> Result<ExecResult, ExecError> {
    let mut m = Machine::new(sh, n, consts, textures, opts);
    if sh.is_pixel {
        for it in &sh.header.interpolators {
            if let Some(v) = inputs.get(&it.reg) {
                if v.len() != n {
                    return Err(ExecError(format!(
                        "interpolator r{} has {} pixels, expected {n}",
                        it.reg,
                        v.len()
                    )));
                }
                let range = m.reg_range(it.reg as usize)?;
                m.r[range].copy_from_slice(v);
            }
        }
        if sh.header.sc_output_screen_xy() {
            let pg = sh.header.param_gen_pos() as usize;
            let range = m.reg_range(pg)?;
            if let Some(p_in) = pos {
                for (dst, src) in m.r[range.clone()].iter_mut().zip(p_in.iter()) {
                    dst[0] = src[0].floor();
                    dst[1] = src[1].floor();
                }
            }
            if let Some(f_in) = front {
                for (dst, &is_front) in m.r[range].iter_mut().zip(f_in.iter()) {
                    if !is_front {
                        dst[0] = if dst[0] == 0.0 { -0.0 } else { -dst[0].abs() };
                    }
                }
            }
        }
    } else {
        for (&reg, v) in inputs {
            if v.len() != n {
                return Err(ExecError(format!("input r{reg} has {} vertices, expected {n}", v.len())));
            }
            let range = m.reg_range(reg as usize)?;
            m.r[range].copy_from_slice(v);
        }
    }
    m.run()?;
    let mut outputs = BTreeMap::new();
    if sh.is_pixel {
        for k in 0..4u32 {
            if (sh.header.outputs() >> k) & 1 != 0 {
                let v = m.out.remove(&k).unwrap_or_else(|| vec![[0.0; 4]; n]);
                outputs.insert(k, v);
            }
        }
    }
    Ok(ExecResult {
        outputs,
        pos: m.out_pos,
        interpolators: m.out_interp,
        alive: m.alive,
        snaps: m.snaps,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::xenos::{self, Cf, CfKind, CfType, Dest, File, Instr, InstrKind, Operand, Stage, VertexFetch};

    fn make_ps_blob(alu_words: [u32; 3], outputs: u32) -> Vec<u8> {
        let mut b = vec![0u8; 0x24 + 32 + 4 * 6];
        let put = |b: &mut Vec<u8>, o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_be_bytes());
        put(&mut b, 0, 0x102a_1100);
        put(&mut b, 4, 0x24 + 32);
        put(&mut b, 8, 24);
        put(&mut b, 0x18, 0x24);
        put(&mut b, 0x24 + 4, 24);
        put(&mut b, 0x24 + 0x1c, outputs);
        put(&mut b, 0x24 + 32, 0x0000_1001);
        put(&mut b, 0x24 + 36, 0x2000);
        put(&mut b, 0x24 + 32 + 12, alu_words[0]);
        put(&mut b, 0x24 + 32 + 16, alu_words[1]);
        put(&mut b, 0x24 + 32 + 20, alu_words[2]);
        b
    }

    fn op_temp(index: u32, swizzle: &str) -> Operand {
        Operand {
            two_op: false,
            negate: false,
            swizzle_raw: 0,
            swizzle: Some(swizzle.into()),
            file: File::Temp,
            index,
            rel: None,
            abs: false,
            components: Some(swizzle.into()),
        }
    }

    fn op_const(index: u32, swizzle: &str, rel: Option<&'static str>) -> Operand {
        Operand {
            two_op: false,
            negate: false,
            swizzle_raw: 0,
            swizzle: Some(swizzle.into()),
            file: File::Const,
            index,
            rel,
            abs: false,
            components: Some(swizzle.into()),
        }
    }

    fn dest(file: File, index: Option<u32>, write: &str, saturate: bool) -> Dest {
        Dest {
            file,
            index,
            rel: None,
            raw_index: None,
            write: write.into(),
            saturate,
        }
    }

    fn alu_vec(vec_op: u32, ops: Vec<Operand>, dst: Dest) -> Instr {
        Instr {
            address: 0,
            raw: [0; 3],
            serialize: false,
            cf_index: 0,
            kind: InstrKind::Alu(Alu {
                predicated: false,
                pred_condition: false,
                is_export: matches!(dst.file, File::Export(_)),
                abs_constants: false,
                const_addr_reg_relative: false,
                const_0_rel: false,
                const_1_rel: false,
                vector_opcode: vec_op,
                scalar_opcode: 0,
                vector_write_mask_raw: 0xf,
                scalar_write_mask_raw: 0,
                vector_dest: dst,
                scalar_dest: dest(File::Temp, Some(0), "____", false),
                vector_operands: ops,
                scalar_operands: Vec::new(),
                vector_nop: false,
                scalar_nop: true,
            }),
        }
    }

    fn synth_shader(is_pixel: bool, cf: Vec<Cf>, instructions: Vec<Instr>) -> Shader {
        let blob = make_ps_blob([0, 0, 0], 1);
        let mut sh = xenos::parse(&blob, Some(true)).unwrap();
        sh.is_pixel = is_pixel;
        if !is_pixel {
            sh.header.stage = Stage::Vertex {
                field18: 0,
                vertex_element_count: 0,
                field20: 0,
                field18_words: Vec::new(),
            };
        }
        sh.cf = cf;
        sh.instructions = instructions;
        sh
    }

    #[test]
    fn d3d9_zero_mul_and_saturate() {
        assert_eq!(xmul(0.0, f32::INFINITY), 0.0);
        assert_eq!(xmul(0.0, f32::NAN), 0.0);
        assert_eq!(xmul(f32::NEG_INFINITY, 0.0), 0.0);
        assert_eq!(xsat(f32::NAN), 0.0);
        assert_eq!(xsat(-2.0), 0.0);
        assert_eq!(xsat(2.5), 1.0);
        assert_eq!(xmax(f32::NAN, 3.0), 3.0);
        assert_eq!(xmin(f32::NAN, 3.0), 3.0);
        assert!(xmax4([1.0, f32::NAN, 2.0, 3.0]).is_nan());
    }

    #[test]
    fn cube_face_selection() {
        assert_eq!(Machine::cube([0.0, 0.0, 1.0, 0.0]), [0.0, 0.0, 2.0, 0.0]);
        assert_eq!(Machine::cube([-1.0, -1.0, 0.0, 0.0]), [0.0, 0.0, -2.0, 5.0]);
    }

    #[test]
    fn runs_minimal_pixel_shader_with_trace() {
        let blob = make_ps_blob([0, 0, 0], 1);
        let sh = xenos::parse(&blob, Some(true)).unwrap();
        let res = run_with(
            &sh,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            4,
            None,
            None,
            &Options {
                trace: true,
                ..Options::default()
            },
        )
        .unwrap();
        assert_eq!(res.alive, vec![true; 4]);
        assert_eq!(res.outputs.get(&0).unwrap().len(), 4);
        assert_eq!(res.snaps.unwrap().len(), 1);
    }

    #[test]
    fn kill_stops_subsequent_writes_by_default() {
        // Vector opcodes: 0 = add, 25 = kill_gt
        // 1) Write c0 ([1, 1, 1, 1]) + c0 -> oC0 = [2, 2, 2, 2]
        // 2) kill_gt r0, c1 (kills pixel 0 where r0 > 0)
        // 3) Write c2 ([9, 9, 9, 9]) + c1 ([0,0,0,0]) -> oC0 (only surviving pixel 1 should get 9!)
        let i0 = alu_vec(0, vec![op_const(0, "xyzw", None), op_const(0, "xyzw", None)], dest(File::Export("oC"), Some(0), "xyzw", false));
        let i1 = alu_vec(25, vec![op_temp(0, "xyzw"), op_const(1, "xyzw", None)], dest(File::Temp, Some(1), "____", false));
        let i2 = alu_vec(0, vec![op_const(2, "xyzw", None), op_const(1, "xyzw", None)], dest(File::Export("oC"), Some(0), "xyzw", false));
        let cf = vec![Cf {
            index: 0,
            opcode: 2,
            address_mode: 0,
            raw: (0, 0),
            kind: CfKind::Exec {
                address: 0,
                count: 3,
                is_yield: 0,
                sequence: 0,
                vc: 0,
                is_end: true,
                type_: CfType::Unconditional,
                bool_index: None,
                condition: None,
                is_predicate_clean: false,
            },
            instructions: Some(vec![0, 1, 2]),
        }];
        let mut sh = synth_shader(true, cf, vec![i0, i1, i2]);
        sh.header.interpolators.push(xenos::Interpolator {
            reg: 0,
            usage: "TEXCOORD",
            usage_index: 0,
            mask: 0xf,
            export_index: 0,
            export_addresses: None,
            raw: 0,
        });
        let inputs = BTreeMap::from([(0, vec![[1.0; 4], [-1.0; 4]])]);
        let consts = BTreeMap::from([(0, [1.0; 4]), (1, [0.0; 4]), (2, [9.0; 4])]);

        // Default (fixed hardware behavior): killed pixel 0 keeps [2,2,2,2], survivor pixel 1 gets [9,9,9,9]
        let res = run(&sh, &inputs, &consts, &BTreeMap::new(), 2, None, None).unwrap();
        assert_eq!(res.alive, vec![false, true]);
        assert_eq!(res.outputs[&0][0], [2.0; 4]);
        assert_eq!(res.outputs[&0][1], [9.0; 4]);

        // Legacy xenos_exec.py parity mode: killed pixel 0 gets overwritten to [9,9,9,9]
        let legacy = run_with(
            &sh,
            &inputs,
            &consts,
            &BTreeMap::new(),
            2,
            None,
            None,
            &Options {
                continue_after_kill: true,
                ..Options::default()
            },
        )
        .unwrap();
        assert_eq!(legacy.alive, vec![false, true]);
        assert_eq!(legacy.outputs[&0][0], [9.0; 4]);
    }

    #[test]
    fn loops_with_al_and_bool_consts_and_calls() {
        // Sum c[10 + aL] for aL = 0, 1, 2 into r0, then call subroutine that adds c[20] if bool_const b0 == true, then export r0 to oC0.
        // CF 0: loop_start loop_id=0, address=2 (loop_end at CF 2)
        // CF 1: exec [r0 = r0 + c[10 + aL]]
        // CF 2: loop_end loop_id=0, address=1
        // CF 3: cond_call Conditional b0==1 -> CF 5
        // CF 4: exec_end [oC0 = r0 + c[0]]
        // CF 5: exec [r0 = r0 + c[20]]
        // CF 6: return
        let instrs = vec![
            alu_vec(0, vec![op_temp(0, "xyzw"), op_const(10, "xyzw", Some("aL"))], dest(File::Temp, Some(0), "xyzw", false)),
            alu_vec(0, vec![op_temp(0, "xyzw"), op_const(0, "xyzw", None)], dest(File::Export("oC"), Some(0), "xyzw", false)),
            alu_vec(0, vec![op_temp(0, "xyzw"), op_const(20, "xyzw", None)], dest(File::Temp, Some(0), "xyzw", false)),
        ];
        let exec_cf = |index, is_end, idxs| Cf {
            index,
            opcode: if is_end { 2 } else { 1 },
            address_mode: 0,
            raw: (0, 0),
            kind: CfKind::Exec {
                address: 0,
                count: 1,
                is_yield: 0,
                sequence: 0,
                vc: 0,
                is_end,
                type_: CfType::Unconditional,
                bool_index: None,
                condition: None,
                is_predicate_clean: false,
            },
            instructions: Some(idxs),
        };
        let cfs = vec![
            Cf {
                index: 0,
                opcode: 7,
                address_mode: 0,
                raw: (0, 0),
                kind: CfKind::LoopStart {
                    address: 2,
                    is_repeat: 0,
                    loop_id: 0,
                },
                instructions: None,
            },
            exec_cf(1, false, vec![0]),
            Cf {
                index: 2,
                opcode: 8,
                address_mode: 0,
                raw: (0, 0),
                kind: CfKind::LoopEnd {
                    address: 1,
                    loop_id: 0,
                    is_predicated_break: 0,
                    condition: 0,
                },
                instructions: None,
            },
            Cf {
                index: 3,
                opcode: 9,
                address_mode: 0,
                raw: (0, 0),
                kind: CfKind::Jump {
                    address: 5,
                    is_unconditional: 0,
                    is_predicated: 0,
                    bool_index: 0,
                    condition: 1,
                    type_: CfType::Conditional,
                    direction: None,
                },
                instructions: None,
            },
            exec_cf(4, true, vec![1]),
            exec_cf(5, false, vec![2]),
            Cf {
                index: 6,
                opcode: 10,
                address_mode: 0,
                raw: (0, 0),
                kind: CfKind::Plain,
                instructions: None,
            },
        ];
        let sh = synth_shader(true, cfs, instrs);
        let consts = BTreeMap::from([
            (0, [0.0; 4]),
            (10, [1.0, 2.0, 3.0, 4.0]),
            (11, [10.0, 20.0, 30.0, 40.0]),
            (12, [100.0, 200.0, 300.0, 400.0]),
            (20, [5.0, 5.0, 5.0, 5.0]),
        ]);
        let opts = Options {
            bool_consts: 1,
            loop_consts: BTreeMap::from([(0, LoopConst { count: 3, start: 0, step: 1 })]),
            ..Options::default()
        };
        let res = run_with(&sh, &BTreeMap::new(), &consts, &BTreeMap::new(), 2, None, None, &opts).unwrap();
        assert_eq!(res.outputs[&0][0], [116.0, 227.0, 338.0, 449.0]);
        assert_eq!(res.outputs[&0][1], [116.0, 227.0, 338.0, 449.0]);
    }

    #[test]
    fn vertex_shader_vfetch_and_exports() {
        // Vertex buffer: 2 vertices of FMT_32_32_32_FLOAT (stride 3 dwords = 12 bytes)
        let mut vb_bytes = Vec::new();
        for f in [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0] {
            vb_bytes.extend(f.to_be_bytes());
        }
        let vfetch = Instr {
            address: 0,
            raw: [0; 3],
            serialize: false,
            cf_index: 0,
            kind: InstrKind::Fetch(Fetch {
                opcode: 0,
                op: "vfetch".into(),
                src: 0,
                src_rel: None,
                dst: 1,
                dst_rel: None,
                dst_swizzle: "xyzw".into(),
                predicated: false,
                pred_condition: false,
                kind: FetchKind::Vertex(VertexFetch {
                    must_be_one: 1,
                    const_index: 0,
                    const_index_sel: 0,
                    prefetch_count: 0,
                    src_swizzle: 'x',
                    is_signed: false,
                    is_integer: false,
                    signed_rf_mode: 0,
                    is_index_rounded: false,
                    format: 57, // FMT_32_32_32_FLOAT
                    exp_adjust: 0,
                    is_mini: false,
                    stride: 3,
                    offset: 0,
                    fetch_const: 0,
                }),
            }),
        };
        let export_pos = alu_vec(0, vec![op_temp(1, "xyzw"), op_const(0, "xyzw", None)], dest(File::Export("oPos"), None, "xyzw", false));
        let cf = vec![Cf {
            index: 0,
            opcode: 2,
            address_mode: 0,
            raw: (0, 0),
            kind: CfKind::Exec {
                address: 0,
                count: 2,
                is_yield: 0,
                sequence: 0,
                vc: 0,
                is_end: true,
                type_: CfType::Unconditional,
                bool_index: None,
                condition: None,
                is_predicate_clean: false,
            },
            instructions: Some(vec![0, 1]),
        }];
        let sh = synth_shader(false, cf, vec![vfetch, export_pos]);
        let inputs = BTreeMap::from([(0, vec![[0.0, 0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0]])]);
        let consts = BTreeMap::from([(0, [10.0, 0.0, 0.0, 0.0])]);
        let opts = Options {
            vertex_buffers: BTreeMap::from([(
                0,
                VertexBuffer {
                    data: vb_bytes,
                    default_stride: 3,
                },
            )]),
            ..Options::default()
        };
        let res = run_with(&sh, &inputs, &consts, &BTreeMap::new(), 2, None, None, &opts).unwrap();
        let pos = res.pos.unwrap();
        assert_eq!(pos[0], [11.0, 2.0, 3.0, 1.0]);
        assert_eq!(pos[1], [14.0, 5.0, 6.0, 1.0]);
    }

    #[test]
    fn bilinear_point_and_unnormalized_sampling() {
        let tex = Texture::new_2d(
            2,
            2,
            vec![
                [1.0, 0.0, 0.0, 1.0],
                [0.0, 1.0, 0.0, 1.0],
                [0.0, 0.0, 1.0, 1.0],
                [1.0, 1.0, 1.0, 1.0],
            ],
            AddressMode::Clamp,
            false,
        );
        let blob = make_ps_blob([0, 0, 0], 1);
        let sh = xenos::parse(&blob, Some(true)).unwrap();
        let textures = BTreeMap::from([(0, tex)]);
        let opts = Options::default();
        let m = Machine::new(&sh, 1, &BTreeMap::new(), &textures, &opts);
        assert_eq!(
            m.sample_2d_one(&textures[&0], 0, AddressMode::Clamp, [0.25, 0.25], [0.0, 0.0], true, false),
            [1.0, 0.0, 0.0, 1.0]
        );
        assert_eq!(
            m.sample_2d_one(&textures[&0], 0, AddressMode::Clamp, [0.5, 0.5], [0.0, 0.0], false, false),
            [0.5, 0.5, 0.5, 1.0]
        );
        assert_eq!(
            m.sample_2d_one(&textures[&0], 0, AddressMode::Clamp, [1.0, 1.0], [0.0, 0.0], false, true),
            [0.5, 0.5, 0.5, 1.0]
        );
        assert_eq!(
            m.sample_2d_one(&textures[&0], 0, AddressMode::Zero, [-0.5, 0.25], [0.0, 0.0], true, false),
            [0.0, 0.0, 0.0, 0.0]
        );
    }
}
