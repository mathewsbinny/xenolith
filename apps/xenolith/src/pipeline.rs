//! Conversion & inspection pipeline connecting `xenolith` to the `platform/`, `middleware/`, and `engines/`
//! crates.

use std::fs;
use std::path::{Path, PathBuf};

use x360_xenos_shader::{disasm, hlsl, xenos};
use x360_xenos_texture::{SurfaceDesc, XenosFormat, untile};

/// Active engine profile in the studio top bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum EngineProfile {
    /// Engine-agnostic Xbox 360 hardware & middleware tools.
    #[default]
    GenericX360,
    /// Frostbite 1 / 1.5 engine adapters (`x360-frostbite`).
    #[cfg(feature = "frostbite")]
    Frostbite,
}

impl EngineProfile {
    /// Human-readable display name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::GenericX360 => "Generic Xbox 360",
            #[cfg(feature = "frostbite")]
            Self::Frostbite => "Frostbite 1 / 1.5",
        }
    }

    /// All compiled engine profiles.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        #[cfg(feature = "frostbite")]
        {
            &[Self::GenericX360, Self::Frostbite]
        }
        #[cfg(not(feature = "frostbite"))]
        {
            &[Self::GenericX360]
        }
    }
}

/// Conversion operation for a queued file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobKind {
    /// Untile raw Xenos texture payload (no engine header).
    RawXenosTexture {
        /// Width in texels.
        width: usize,
        /// Height in texels.
        height: usize,
        /// Mip count.
        mips: usize,
        /// Slice count (1 for 2D, 6 for cube).
        slices: usize,
        /// Console pixel format.
        format: XenosFormat,
    },
    /// Convert Frostbite 92-byte `XenonTexture` -> PC `ITexture`.
    #[cfg(feature = "frostbite")]
    FrostbiteTexture,
    /// Disassemble & translate Xbox 360 XDK shader blob to HLSL.
    XenosShaderToHlsl,
    /// Decode XMA2 RIFF audio to standard 16-bit PCM WAV.
    #[cfg(feature = "xma")]
    Xma2ToWav,
    /// Convert big-endian Granny 2 (`.gr2`) resource to PC little-endian.
    GrannyToPc,
}

impl JobKind {
    /// Short label for the queue table.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::RawXenosTexture { .. } => "Xenos Raw Texture -> Linear",
            #[cfg(feature = "frostbite")]
            Self::FrostbiteTexture => "Frostbite XenonTexture -> ITexture",
            Self::XenosShaderToHlsl => "Xenos Shader -> HLSL",
            #[cfg(feature = "xma")]
            Self::Xma2ToWav => "XMA2 RIFF -> PCM16 WAV",
            Self::GrannyToPc => "Granny 2 (.gr2) -> PC",
        }
    }

    /// Default output file extension.
    #[must_use]
    pub const fn out_ext(&self) -> &'static str {
        match self {
            Self::RawXenosTexture { .. } => "bin",
            #[cfg(feature = "frostbite")]
            Self::FrostbiteTexture => "itexture",
            Self::XenosShaderToHlsl => "hlsl",
            #[cfg(feature = "xma")]
            Self::Xma2ToWav => "wav",
            Self::GrannyToPc => "gr2",
        }
    }
}

/// One queued conversion item.
#[derive(Debug, Clone)]
pub struct QueueItem {
    /// Input file path.
    pub input: PathBuf,
    /// Conversion kind.
    pub kind: JobKind,
    /// Latest execution status.
    pub status: JobStatus,
}

/// Status of a queued job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    /// Not yet run.
    Pending,
    /// Succeeded with output path and byte count.
    Done {
        /// Output file path written.
        output: PathBuf,
        /// Output size in bytes.
        bytes: usize,
    },
    /// Failed with an error message.
    Failed(String),
}

/// Result of inspecting a single file in the Inspector panel.
#[derive(Debug, Clone)]
pub struct Inspection {
    /// Source file path.
    pub path: PathBuf,
    /// Detected format label.
    pub detected: String,
    /// Key-value metadata rows.
    pub fields: Vec<(String, String)>,
    /// Primary text preview (disassembly, container listing, etc.).
    pub primary_text: String,
    /// Secondary text preview (e.g. translated HLSL).
    pub secondary_text: Option<String>,
    /// Suggested default conversion job for this file.
    pub suggested: Option<JobKind>,
}

/// Encodes interleaved 16-bit PCM samples as a standard 44-byte RIFF WAVE file.
#[must_use]
pub fn encode_wav_pcm16(samples: &[i16], channels: u16, sample_rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let byte_rate = sample_rate * u32::from(channels) * 2;
    let block_align = channels * 2;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Inspects a file on disk by magic bytes and returns metadata + preview text.
///
/// # Errors
/// Returns an error string if the file cannot be read.
pub fn inspect_file(path: &Path, engine: EngineProfile) -> Result<Inspection, String> {
    let data = fs::read(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let mut fields = vec![("Size".into(), format!("{} bytes", data.len()))];

    // 1. XDK Shader (0x102a1100 / 0x102a1101)
    if let Some(mbytes) = data.get(0..4).and_then(|s| <[u8; 4]>::try_from(s).ok()) {
        let magic = u32::from_be_bytes(mbytes);
        if (magic == 0x102a_1100 || magic == 0x102a_1101)
            && let Ok(sh) = xenos::parse(&data, None)
        {
            fields.push(("Stage".into(), if sh.is_pixel { "Pixel Shader" } else { "Vertex Shader" }.into()));
            fields.push(("CF Instructions".into(), sh.cf.len().to_string()));
            fields.push(("ALU / Fetch Slots".into(), sh.instructions.len().to_string()));
            fields.push(("Temp Registers".into(), sh.header.num_temp_regs.to_string()));
            let listing = disasm::body(&sh, false, sh.is_pixel).join("\n");
            let hlsl_out = hlsl::translate(&sh, &hlsl::Interface::new(Vec::new()))
                .unwrap_or_else(|e| format!("// HLSL translation refused: {e}"));
            return Ok(Inspection {
                path: path.to_path_buf(),
                detected: "Xbox 360 XDK Shader (Xenos)".into(),
                fields,
                primary_text: listing,
                secondary_text: Some(hlsl_out),
                suggested: Some(JobKind::XenosShaderToHlsl),
            });
        }
    }

    // 2. XMA2 RIFF WAVE
    #[cfg(feature = "xma")]
    if data.starts_with(b"RIFF")
        && let Ok(riff) = x360_xma::parse_riff(&data)
    {
            fields.push(("Channels".into(), riff.channels.to_string()));
            fields.push(("Sample Rate".into(), format!("{} Hz", riff.rate)));
            fields.push(("Byte Rate".into(), format!("{} B/s", riff.byte_rate)));
            fields.push(("Payload".into(), format!("{} bytes", riff.data.len())));
            return Ok(Inspection {
                path: path.to_path_buf(),
                detected: "Xbox 360 XMA2 RIFF Audio".into(),
                fields,
                primary_text: format!(
                    "XMA2 RIFF stream: {} ch @ {} Hz ({} bytes compressed).",
                    riff.channels,
                    riff.rate,
                    riff.data.len()
                ),
            secondary_text: None,
            suggested: Some(JobKind::Xma2ToWav),
        });
    }

    // 3. Granny 2 (.gr2)
    if let Ok(g) = x360_granny::format::Granny::parse(&data) {
        fields.push(("Endianness".into(), format!("{:?}", g.endian)));
        fields.push(("Sections".into(), g.sections.len().to_string()));
        return Ok(Inspection {
            path: path.to_path_buf(),
            detected: "RAD Granny 2 (.gr2)".into(),
            fields,
            primary_text: format!("Granny 2 resource ({:?}, {} sections).", g.endian, g.sections.len()),
            secondary_text: None,
            suggested: Some(JobKind::GrannyToPc),
        });
    }

    // 4. Havok Packfile
    if data.starts_with(&x360_havok::convert::MAGIC) {
        return Ok(Inspection {
            path: path.to_path_buf(),
            detected: "Havok Packfile (hkPackfile)".into(),
            fields,
            primary_text: "Havok Physics packfile detected (supply game hkClass metadata to convert endianness)."
                .into(),
            secondary_text: None,
            suggested: None,
        });
    }

    // 5. Frostbite XenonTexture (92-byte BE header)
    #[cfg(feature = "frostbite")]
    if engine == EngineProfile::Frostbite && data.len() >= x360_frostbite::texture::HEADER {
        let be = |i: usize| -> u32 {
            data.get(4 * i..4 * i + 4)
                .and_then(|s| <[u8; 4]>::try_from(s).ok())
                .map_or(0, u32::from_be_bytes)
        };
        let (typ, fmt, w, h, mips) = (be(1), be(2), be(4), be(5), be(7));
        if (1..=16384).contains(&w)
            && (1..=16384).contains(&h)
            && (1..=15).contains(&mips)
            && x360_frostbite::texture::xenos_format(fmt).is_some()
        {
            fields.push(("Dimensions".into(), format!("{w} x {h}")));
            fields.push(("Mip Levels".into(), mips.to_string()));
            fields.push(("Frostbite Format".into(), format!("{fmt} ({:?})", x360_frostbite::texture::xenos_format(fmt))));
            fields.push(("Type".into(), if typ == 1 { "Cube Map" } else { "2D" }.into()));
            return Ok(Inspection {
                path: path.to_path_buf(),
                detected: "Frostbite XenonTexture".into(),
                fields,
                primary_text: format!("Frostbite XenonTexture {w}x{h}, {mips} mips, format {fmt}."),
                secondary_text: None,
                suggested: Some(JobKind::FrostbiteTexture),
            });
        }
    }

    let _ = engine;
    Ok(Inspection {
        path: path.to_path_buf(),
        detected: "Raw / Unrecognized Binary".into(),
        fields,
        primary_text: "No known container/asset magic matched. You can still queue it as a Raw Xenos Texture with manual dimensions.".into(),
        secondary_text: None,
        suggested: None,
    })
}

/// Runs a single conversion job and writes its output to `out_dir`.
///
/// # Errors
/// Returns a descriptive error string on I/O or conversion failure.
pub fn run_job(input: &Path, kind: &JobKind, out_dir: &Path) -> Result<(PathBuf, usize), String> {
    let raw = fs::read(input).map_err(|e| format!("read {}: {e}", input.display()))?;
    let bytes: Vec<u8> = match kind {
        JobKind::RawXenosTexture {
            width,
            height,
            mips,
            slices,
            format,
        } => {
            let desc = SurfaceDesc {
                width: *width,
                height: *height,
                mips: *mips,
                slices: *slices,
                format: *format,
            };
            untile(&raw, &desc).map_err(|e| e.to_string())?.data
        }
        #[cfg(feature = "frostbite")]
        JobKind::FrostbiteTexture => x360_frostbite::texture::convert(&raw).map_err(|e| e.to_string())?,
        JobKind::XenosShaderToHlsl => {
            let sh = xenos::parse(&raw, None).map_err(|e| e.to_string())?;
            hlsl::translate(&sh, &hlsl::Interface::new(Vec::new()))
                .map_err(|e| e.to_string())?
                .into_bytes()
        }
        #[cfg(feature = "xma")]
        JobKind::Xma2ToWav => {
            let riff = x360_xma::parse_riff(&raw).map_err(|e| e.to_string())?;
            let decoded = x360_xma::decode(riff.data, riff.channels, riff.rate, riff.byte_rate)
                .map_err(|e| e.to_string())?;
            let pcm16 = x360_xma::to_s16(&decoded.planar);
            encode_wav_pcm16(&pcm16, decoded.planar.len() as u16, riff.rate)
        }
        JobKind::GrannyToPc => {
            x360_granny::convert::to_pc::<std::collections::hash_map::RandomState>(&raw, None)
                .map_err(|e| e.to_string())?
        }
    };
    fs::create_dir_all(out_dir).map_err(|e| format!("create {}: {e}", out_dir.display()))?;
    let stem = input.file_stem().and_then(|s| s.to_str()).unwrap_or("output");
    let out_path = out_dir.join(format!("{stem}.{}", kind.out_ext()));
    fs::write(&out_path, &bytes).map_err(|e| format!("write {}: {e}", out_path.display()))?;
    Ok((out_path, bytes.len()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn encodes_valid_pcm16_wav_header() {
        let wav = encode_wav_pcm16(&[0, 1000, -1000, 0], 2, 48_000);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(wav.len(), 44 + 8);
    }

    #[test]
    fn runs_raw_xenos_texture_job_end_to_end() {
        let dir = std::env::temp_dir().join("x360_studio_test_pipeline");
        let _ = fs::create_dir_all(&dir);
        let inp = dir.join("sample.raw");
        let (_, need) = x360_xenos_texture::layout(8, 8, 1, XenosFormat::L8, 1);
        fs::write(&inp, vec![42u8; need]).unwrap();
        let (out, n) = run_job(
            &inp,
            &JobKind::RawXenosTexture {
                width: 8,
                height: 8,
                mips: 1,
                slices: 1,
                format: XenosFormat::L8,
            },
            &dir,
        )
        .unwrap();
        assert_eq!(n, 64);
        assert!(out.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
