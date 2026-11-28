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
    /// Convert Frostbite 92-byte `XenonTexture` -> PC `ITexture` (supports `RES\x02` & 3D volumes).
    #[cfg(feature = "frostbite")]
    FrostbiteTexture,
    /// Export Frostbite `{binary}` `.dbx` partition to readable XML.
    #[cfg(feature = "frostbite")]
    FrostbiteDbxToXml,
    /// Upgrade a BFBC1 (Frostbite 1.0) `{binary}` `.dbx` partition to Frostbite 1.5 schema.
    #[cfg(feature = "frostbite")]
    #[allow(dead_code)]
    Bfbc1DbxUpgradeToFb15,
    /// Unpack all entries from a Frostbite `FbRB` archive into a folder.
    #[cfg(feature = "frostbite")]
    #[allow(dead_code)]
    FrostbiteFbrbUnpack,
    /// Port all recognized Xbox 360 entries inside a Frostbite `FbRB` archive to PC format.
    #[cfg(feature = "frostbite")]
    FrostbiteFbrbPortToPc,
    /// Convert Xbox 360 EA APT `BIGF` movie (big-endian -> little-endian).
    #[cfg(feature = "frostbite")]
    FrostbiteAptToPc,
    /// Decode a Frostbite console `Wave` resource to PCM16 WAV.
    #[cfg(all(feature = "frostbite", feature = "xma"))]
    FrostbiteWaveToWav,
    /// Repack a Frostbite SPS v1 `.sns` streaming audio bank from Xbox 360 EA-XMA to PC EA-XAS.
    #[cfg(all(feature = "frostbite", feature = "xma"))]
    FrostbiteSnsRepackToPc,
    /// Validate and repack a Frostbite `ShaderDatabase` (stripping DXBC debug chunks & recomputing checksums).
    #[cfg(feature = "frostbite")]
    FrostbiteShaderDbRepack,
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
            #[cfg(feature = "frostbite")]
            Self::FrostbiteDbxToXml => "Frostbite Binary DBX -> XML",
            #[cfg(feature = "frostbite")]
            Self::Bfbc1DbxUpgradeToFb15 => "Upgrade BFBC1 DBX -> Frostbite 1.5",
            #[cfg(feature = "frostbite")]
            Self::FrostbiteFbrbUnpack => "Unpack Frostbite FbRB Archive",
            #[cfg(feature = "frostbite")]
            Self::FrostbiteFbrbPortToPc => "Port Frostbite FbRB Bundle -> PC",
            #[cfg(feature = "frostbite")]
            Self::FrostbiteAptToPc => "Frostbite APT BIGF (BE -> LE)",
            #[cfg(all(feature = "frostbite", feature = "xma"))]
            Self::FrostbiteWaveToWav => "Frostbite Wave -> PCM16 WAV",
            #[cfg(all(feature = "frostbite", feature = "xma"))]
            Self::FrostbiteSnsRepackToPc => "Repack Frostbite .sns Audio -> PC",
            #[cfg(feature = "frostbite")]
            Self::FrostbiteShaderDbRepack => "Repack Frostbite ShaderDatabase (DXBC)",
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
            #[cfg(feature = "frostbite")]
            Self::FrostbiteDbxToXml => "xml",
            #[cfg(feature = "frostbite")]
            Self::Bfbc1DbxUpgradeToFb15 => "fb15.dbx",
            #[cfg(feature = "frostbite")]
            Self::FrostbiteFbrbUnpack => "unpacked",
            #[cfg(feature = "frostbite")]
            Self::FrostbiteFbrbPortToPc => "pc.fbrb",
            #[cfg(feature = "frostbite")]
            Self::FrostbiteAptToPc => "res",
            #[cfg(all(feature = "frostbite", feature = "xma"))]
            Self::FrostbiteWaveToWav => "wav",
            #[cfg(all(feature = "frostbite", feature = "xma"))]
            Self::FrostbiteSnsRepackToPc => "pc.sns",
            #[cfg(feature = "frostbite")]
            Self::FrostbiteShaderDbRepack => "repacked.shaderdb",
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

/// Encodes interleaved 16-bit PCM samples as a standard 44-byte RIFF WAVE file. Only the XMA2 jobs write one, so a
/// build without the `xma` feature leaves it out (it would be dead code there).
#[must_use]
#[cfg(any(feature = "xma", test))]
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
#[allow(clippy::too_many_lines)]
pub fn inspect_file(path: &Path, engine: EngineProfile) -> Result<Inspection, String> {
    let data = fs::read(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let mut fields = vec![("Size".into(), format!("{} bytes", data.len()))];

    // 1. XDK Shader (0x102a1100 / 0x102a1101)
    if let Some(mbytes) = data.get(0..4).and_then(|s| <[u8; 4]>::try_from(s).ok()) {
        let magic = u32::from_be_bytes(mbytes);
        if (magic == 0x102a_1100 || magic == 0x102a_1101)
            && let Ok(sh) = xenos::parse(&data, None)
        {
            fields.push((
                "Stage".into(),
                if sh.is_pixel { "Pixel Shader" } else { "Vertex Shader" }.into(),
            ));
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
        #[cfg(feature = "frostbite")]
        if let Ok(hk) = x360_frostbite::bfbc1::inspect_havok_header(&data) {
            fields.push(("SDK Version".into(), hk.sdk_version.clone()));
            fields.push(("File Version".into(), hk.file_version.to_string()));
            fields.push((
                "Endianness".into(),
                if hk.little_endian {
                    "Little (PC)"
                } else {
                    "Big (Xbox 360)"
                }
                .into(),
            ));
            fields.push(("Sections".into(), hk.section_count.to_string()));
            return Ok(Inspection {
                path: path.to_path_buf(),
                detected: "Havok Packfile (hkPackfile)".into(),
                fields,
                primary_text: format!(
                    "Havok packfile ({}, v{}, {} sections, {}).",
                    hk.sdk_version,
                    hk.file_version,
                    hk.section_count,
                    if hk.little_endian { "PC LE" } else { "Xbox 360 BE" }
                ),
                secondary_text: None,
                suggested: None,
            });
        }
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

    // 4b. Xbox 360 XEX2 Executable
    if data.starts_with(x360_containers::xex::MAGIC)
        && let Ok(hdr) = x360_containers::xex::parse(&data)
    {
        if let Some(ex) = hdr.execution_info {
            fields.push(("Title ID".into(), format!("0x{:08X}", ex.title_id)));
            fields.push(("Media ID".into(), format!("0x{:08X}", ex.media_id)));
        }
        if let Some(ep) = hdr.entry_point {
            fields.push(("Entry Point".into(), format!("0x{ep:08X}")));
        }
        fields.push(("Load Address".into(), format!("0x{:08X}", hdr.security.load_address)));
        fields.push(("Image Size".into(), format!("{} bytes", hdr.security.image_size)));
        fields.push(("Page Size".into(), format!("{} bytes", hdr.security.page_size())));
        fields.push(("Page Descriptors".into(), hdr.security.descriptors.len().to_string()));
        return Ok(Inspection {
            path: path.to_path_buf(),
            detected: "Xbox 360 Executable (XEX2)".into(),
            fields,
            primary_text: format!(
                "XEX2 executable: {} optional headers, {} SHA-1 page descriptors ({} KiB pages).",
                hdr.optional_headers.len(),
                hdr.security.descriptors.len(),
                hdr.security.page_size() / 1024
            ),
            secondary_text: None,
            suggested: None,
        });
    }

    // 4c. Frostbite FbRB Archive
    #[cfg(feature = "frostbite")]
    if engine == EngineProfile::Frostbite
        && data.starts_with(b"FbRB")
        && let Ok(arc) = x360_frostbite::fbrb::FbrbArchive::parse(&data)
    {
        fields.push(("Entries".into(), arc.entries.len().to_string()));
        let preview = arc
            .entries
            .iter()
            .take(80)
            .map(|e| format!("[{}] {} ({} bytes)", e.ext, e.path, e.data.len()))
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(Inspection {
            path: path.to_path_buf(),
            detected: "Frostbite Archive (FbRB)".into(),
            fields,
            primary_text: preview,
            secondary_text: None,
            suggested: Some(JobKind::FrostbiteFbrbPortToPc),
        });
    }

    // 4d. Frostbite {binary} DBX Partition
    #[cfg(feature = "frostbite")]
    if engine == EngineProfile::Frostbite
        && x360_frostbite::dbx::is_binary(&data)
        && let Ok(dbx_file) = x360_frostbite::dbx::parse(&data)
    {
        let inst_count = dbx_file.instances().count();
        fields.push(("Instances".into(), inst_count.to_string()));
        if let Some(guid) = dbx_file.partition().and_then(|p| p.attr("guid")) {
            fields.push(("Partition GUID".into(), guid.to_owned()));
        }
        let xml = dbx_file.to_xml();
        let preview: String = xml.chars().take(16_384).collect();
        return Ok(Inspection {
            path: path.to_path_buf(),
            detected: "Frostbite Binary DBX Partition".into(),
            fields,
            primary_text: preview,
            secondary_text: None,
            suggested: Some(JobKind::FrostbiteDbxToXml),
        });
    }

    // 4e. EA APT BIGF Movie
    #[cfg(feature = "frostbite")]
    if engine == EngineProfile::Frostbite
        && data.starts_with(b"BIGF")
        && let Ok(members) = x360_frostbite::apt::parse_bigf(&data)
    {
        fields.push(("Members".into(), members.len().to_string()));
        let listing = members
            .iter()
            .map(|(n, b)| format!("{n} ({} bytes)", b.len()))
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(Inspection {
            path: path.to_path_buf(),
            detected: "Frostbite EA APT Movie (BIGF)".into(),
            fields,
            primary_text: listing,
            secondary_text: None,
            suggested: Some(JobKind::FrostbiteAptToPc),
        });
    }

    // 4f. Frostbite Console Wave Resource (XMA2/SPS -> PCM16 WAV)
    #[cfg(all(feature = "frostbite", feature = "xma"))]
    if engine == EngineProfile::Frostbite
        && data.len() >= 64
        && path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.to_ascii_lowercase().contains("wave"))
        && let Some((pcm, ch, rate)) = x360_frostbite::wavport::decode(&data, "bf1943", &[None, None, None])
    {
        fields.push(("Channels".into(), ch.to_string()));
        fields.push(("Sample Rate".into(), format!("{rate} Hz")));
        fields.push(("Decoded Samples".into(), pcm.len().to_string()));
        return Ok(Inspection {
            path: path.to_path_buf(),
            detected: "Frostbite Console Wave".into(),
            fields,
            primary_text: format!("Frostbite Wave audio: {ch} ch @ {rate} Hz ({} PCM samples).", pcm.len()),
            secondary_text: None,
            suggested: Some(JobKind::FrostbiteWaveToWav),
        });
    }

    // 4g. Frostbite Streaming SPS v1 .sns Audio Bank (header-only scan)
    #[cfg(all(feature = "frostbite", feature = "xma"))]
    if engine == EngineProfile::Frostbite
        && data.starts_with(b"\x48\x00\x00\x0c")
        && let Ok(rep) = x360_frostbite::sns::inspect_sps_bank(&data)
    {
        fields.push(("Streams".into(), rep.entries.len().to_string()));
        fields.push(("EA-XMA Streams".into(), rep.converted.to_string()));
        fields.push(("EA-XAS Streams".into(), rep.passthrough.to_string()));
        let listing = rep
            .entries
            .iter()
            .take(64)
            .map(|e| {
                format!(
                    "#{}: off={:#x} len={} ch={} rate={} Hz samples={} codec={}",
                    e.index, e.old_offset, e.old_size, e.channels, e.rate, e.samples, e.codec_from
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(Inspection {
            path: path.to_path_buf(),
            detected: "Frostbite Streaming Audio Bank (.sns / SPS)".into(),
            fields,
            primary_text: listing,
            secondary_text: None,
            suggested: Some(JobKind::FrostbiteSnsRepackToPc),
        });
    }

    // 4h. Direct3D DXBC / Frostbite ShaderDatabase (PC DXBC or Xbox 360 Xenos)
    #[cfg(feature = "frostbite")]
    if engine == EngineProfile::Frostbite
        && let Ok((_, rep)) =
            x360_frostbite::shader_db::repack_dxbc_database(&data, &x360_frostbite::shader_db::RepackOptions::default())
        && rep.total_dxbc > 0
    {
        fields.push(("DXBC Shaders".into(), rep.total_dxbc.to_string()));
        for (stage, count) in &rep.stages {
            fields.push((format!("Stage {stage}"), count.to_string()));
        }
        return Ok(Inspection {
            path: path.to_path_buf(),
            detected: "Frostbite ShaderDatabase / DXBC".into(),
            fields,
            primary_text: format!(
                "Detected {} DXBC shader container(s) across {} stage(s).",
                rep.total_dxbc,
                rep.stages.len()
            ),
            secondary_text: None,
            suggested: Some(JobKind::FrostbiteShaderDbRepack),
        });
    }

    #[cfg(feature = "frostbite")]
    if engine == EngineProfile::Frostbite && data.len() >= 0x40 {
        let xenos_blobs = x360_frostbite::shader_db::extract_xenos_shaders(&data);
        if !xenos_blobs.is_empty() {
            let ps_count = xenos_blobs.iter().filter(|e| e.is_pixel).count();
            let vs_count = xenos_blobs.len() - ps_count;
            fields.push(("Xenos Shaders".into(), xenos_blobs.len().to_string()));
            fields.push(("Vertex Shaders".into(), vs_count.to_string()));
            fields.push(("Pixel Shaders".into(), ps_count.to_string()));
            let preview = xenos_blobs
                .iter()
                .take(64)
                .map(|e| {
                    format!(
                        "{} @ {:#x} ({} bytes)",
                        if e.is_pixel { "PS" } else { "VS" },
                        e.offset,
                        e.size
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(Inspection {
                path: path.to_path_buf(),
                detected: "Xbox 360 Frostbite ShaderDatabase (Xenos)".into(),
                fields,
                primary_text: preview,
                secondary_text: None,
                suggested: None,
            });
        }
    }

    // 5. Frostbite XenonTexture (92-byte BE header, with optional 64-byte RES\x02 wrapper)
    #[cfg(feature = "frostbite")]
    if engine == EngineProfile::Frostbite {
        let tex_raw = x360_frostbite::texture::unwrap_res_header(&data);
        if tex_raw.len() >= x360_frostbite::texture::HEADER {
            let be = |i: usize| -> u32 {
                tex_raw
                    .get(4 * i..4 * i + 4)
                    .and_then(|s| <[u8; 4]>::try_from(s).ok())
                    .map_or(0, u32::from_be_bytes)
            };
            let (typ, fmt, w, h, depth, mips) = (be(1), be(2), be(4), be(5), be(6), be(7));
            if (1..=16384).contains(&w)
                && (1..=16384).contains(&h)
                && (1..=15).contains(&mips)
                && x360_frostbite::texture::xenos_format(fmt).is_some()
            {
                fields.push(("Dimensions".into(), format!("{w} x {h}")));
                if typ == x360_frostbite::texture::VOLUME && depth >= 2 {
                    fields.push(("Depth".into(), depth.to_string()));
                }
                fields.push(("Mip Levels".into(), mips.to_string()));
                fields.push((
                    "Frostbite Format".into(),
                    format!("{fmt} ({:?})", x360_frostbite::texture::xenos_format(fmt)),
                ));
                fields.push((
                    "Type".into(),
                    match typ {
                        1 => "Cube Map",
                        2 if depth >= 2 => "3D Volume",
                        _ => "2D",
                    }
                    .into(),
                ));
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
        #[cfg(feature = "frostbite")]
        JobKind::FrostbiteDbxToXml => x360_frostbite::dbx::parse(&raw)
            .map_err(|e| e.to_string())?
            .to_xml()
            .into_bytes(),
        #[cfg(feature = "frostbite")]
        JobKind::Bfbc1DbxUpgradeToFb15 => {
            let rules = x360_frostbite::bfbc1::DbxSchemaRules::bfbc1_to_fb15();
            let (upgraded, _report) =
                x360_frostbite::bfbc1::upgrade_dbx_bytes(&raw, &rules).map_err(|e| e.to_string())?;
            upgraded
        }
        #[cfg(feature = "frostbite")]
        JobKind::FrostbiteFbrbUnpack => {
            let arc = x360_frostbite::fbrb::FbrbArchive::parse(&raw).map_err(|e| e.to_string())?;
            let stem = input.file_stem().and_then(|s| s.to_str()).unwrap_or("archive");
            let dest = out_dir.join(format!("{stem}_unpacked"));
            let mut total = 0usize;
            for entry in &arc.entries {
                if entry.data.is_empty() || entry.path.contains("..") {
                    continue;
                }
                let p = dest.join(entry.path.replace('/', std::path::MAIN_SEPARATOR_STR));
                if let Some(parent) = p.parent() {
                    fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
                }
                fs::write(&p, &entry.data).map_err(|e| format!("write {}: {e}", p.display()))?;
                total += entry.data.len();
            }
            return Ok((dest, total));
        }
        #[cfg(feature = "frostbite")]
        JobKind::FrostbiteFbrbPortToPc => {
            let (pc_fbrb, _report) = x360_frostbite::bundle_port::port_fbrb_bytes(
                &raw,
                &x360_frostbite::bundle_port::PortOptions::default(),
            )
            .map_err(|e| e.to_string())?;
            pc_fbrb
        }
        #[cfg(feature = "frostbite")]
        JobKind::FrostbiteAptToPc => x360_frostbite::apt::convert_bigf_be_to_le(&raw).map_err(|e| e.to_string())?,
        #[cfg(all(feature = "frostbite", feature = "xma"))]
        JobKind::FrostbiteWaveToWav => {
            let (pcm, ch, rate) = x360_frostbite::wavport::decode(&raw, "bf1943", &[None, None, None])
                .ok_or_else(|| "unable to decode Frostbite Wave resource".to_owned())?;
            encode_wav_pcm16(&pcm, ch as u16, rate)
        }
        #[cfg(all(feature = "frostbite", feature = "xma"))]
        JobKind::FrostbiteSnsRepackToPc => {
            let (repacked, _report) = x360_frostbite::sns::repack_sps_bank(&raw).map_err(|e| e.to_string())?;
            repacked
        }
        #[cfg(feature = "frostbite")]
        JobKind::FrostbiteShaderDbRepack => {
            let opts = x360_frostbite::shader_db::RepackOptions {
                strip_debug: true,
                replacements: std::collections::BTreeMap::new(),
            };
            let (repacked, _report) =
                x360_frostbite::shader_db::repack_dxbc_database(&raw, &opts).map_err(|e| e.to_string())?;
            repacked
        }
        JobKind::XenosShaderToHlsl => {
            let sh = xenos::parse(&raw, None).map_err(|e| e.to_string())?;
            hlsl::translate(&sh, &hlsl::Interface::new(Vec::new()))
                .map_err(|e| e.to_string())?
                .into_bytes()
        }
        #[cfg(feature = "xma")]
        JobKind::Xma2ToWav => {
            let riff = x360_xma::parse_riff(&raw).map_err(|e| e.to_string())?;
            let decoded =
                x360_xma::decode(riff.data, riff.channels, riff.rate, riff.byte_rate).map_err(|e| e.to_string())?;
            let pcm16 = x360_xma::to_s16(&decoded.planar);
            encode_wav_pcm16(&pcm16, decoded.planar.len() as u16, riff.rate)
        }
        JobKind::GrannyToPc => x360_granny::convert::to_pc::<std::collections::hash_map::RandomState>(&raw, None)
            .map_err(|e| e.to_string())?,
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
