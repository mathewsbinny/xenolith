//! Streaming `.sns` audio bank parser, transcoder, and repacker for Frostbite 1 / 1.5.
//!
//! Handles both EA audio stream containers used by Xbox 360 Frostbite titles:
//! - **SPS v1** (BFBC2 Xbox 360 / PC `.sns` streaming banks and `Wave` payloads):
//!   `0x48` header block, `0x44` data blocks, `0x45` end block.
//! - **SNR + SNS v0** (Battlefield 1943 `Wave` 8-byte SNR header + companion `waveDataSns`
//!   block stream): `[flags:u8 | size:u24][samples:u32][payload...]` terminated by `0x80`.
//!
//! Transcodes Xbox 360 EA-XMA (codec `3`) streams to PC EA-XAS v1 (codec `4`) using the
//! pure-Rust [`x360_xma`] decoder and [`crate::audio`] EA-XAS encoder, and tracks byte
//! offset remapping across multi-stream `.sns` banks.
#![deny(unsafe_code)]

use rayon::prelude::*;
use thiserror::Error;

use crate::{audio, audioframe, wave, wavport};

/// EA stream codec ID for Xbox 360 EA-XMA.
pub const CODEC_EA_XMA: u32 = 3;
/// EA stream codec ID for PC EA-XAS v1.
pub const CODEC_EA_XAS: u32 = 4;

/// Errors returned by streaming `.sns` parsing, transcoding, or bank repacking.
#[derive(Debug, Error)]
pub enum SnsError {
    /// Underlying Wave / SPS / SNR container error.
    #[error(transparent)]
    Wave(#[from] wave::Error),
    /// EA-XAS encoding or decoding error.
    #[error(transparent)]
    Audio(#[from] audio::AudioError),
    /// EA-XMA / EA-XAS block framing error.
    #[error(transparent)]
    Frame(#[from] audioframe::Error),
    /// Stream uses a codec other than EA-XMA (3) or EA-XAS (4).
    #[error("unsupported EA audio codec id {0}")]
    UnsupportedCodec(u32),
    /// Stream parameters out of EA header bit-field bounds.
    #[error("stream parameters out of EA header bounds (channels={channels}, rate={rate}, samples={samples})")]
    InvalidHeader {
        /// Channel count.
        channels: u32,
        /// Sample rate in Hz.
        rate: u32,
        /// Total sample count.
        samples: u32,
    },
    /// XMA2 or XAS decoding returned no samples or failed.
    #[error("failed to decode {0} stream")]
    DecodeFailed(&'static str),
    /// Multi-stream `.sns` bank contained no streams or trailing unparsed bytes.
    #[error("malformed SPS bank at byte offset {0:#x}")]
    MalformedBank(usize),
}

/// Per-stream relocation record produced by [`repack_sps_bank`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnsBankEntry {
    /// Zero-based stream index inside the `.sns` bank.
    pub index: usize,
    /// Byte offset in the input bank.
    pub old_offset: usize,
    /// Byte length in the input bank.
    pub old_size: usize,
    /// Byte offset in the repacked PC bank.
    pub new_offset: usize,
    /// Byte length in the repacked PC bank.
    pub new_size: usize,
    /// Channel count.
    pub channels: u32,
    /// Sample rate in Hz.
    pub rate: u32,
    /// Total samples.
    pub samples: u32,
    /// Source codec ID (`3` = EA-XMA, `4` = EA-XAS).
    pub codec_from: u32,
    /// Output codec ID (`4` = EA-XAS).
    pub codec_to: u32,
}

/// Summary report produced by [`repack_sps_bank`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnsBankReport {
    /// Per-stream offset remapping table.
    pub entries: Vec<SnsBankEntry>,
    /// Number of streams transcoded from EA-XMA to EA-XAS.
    pub converted: usize,
    /// Number of streams already in EA-XAS format (passed through).
    pub passthrough: usize,
}

/// Decodes an EA [`wave::Stream`] (EA-XMA or EA-XAS) into interleaved 16-bit PCM.
///
/// # Errors
/// Returns [`SnsError`] on unsupported codecs or malformed stream payloads.
pub fn decode_stream_to_pcm(st: &wave::Stream) -> Result<Vec<i16>, SnsError> {
    match st.codec {
        CODEC_EA_XMA => decode_xma_stream(st),
        CODEC_EA_XAS => decode_xas_stream(st),
        other => Err(SnsError::UnsupportedCodec(other)),
    }
}

/// Decodes a standalone SPS v1 `.sns` stream into `(interleaved_pcm, channels, sample_rate)`.
///
/// # Errors
/// Returns [`SnsError`] if the SPS container or codec payload is invalid.
pub fn decode_sps_to_pcm(data: &[u8]) -> Result<(Vec<i16>, usize, u32), SnsError> {
    let st = wave::read_sps(data)?;
    let pcm = decode_stream_to_pcm(&st)?;
    Ok((pcm, st.channels as usize, st.rate))
}

/// Decodes a Battlefield 1943 SNR header + SNS v0 block stream into
/// `(interleaved_pcm, channels, sample_rate)`.
///
/// # Errors
/// Returns [`SnsError`] if the SNR/SNS container or codec payload is invalid.
pub fn decode_snr_sns_to_pcm(snr_header: &[u8], sns_data: &[u8]) -> Result<(Vec<i16>, usize, u32), SnsError> {
    let st = wave::read_snr(snr_header, Some(sns_data))?;
    let pcm = decode_stream_to_pcm(&st)?;
    Ok((pcm, st.channels as usize, st.rate))
}

fn decode_xma_stream(st: &wave::Stream) -> Result<Vec<i16>, SnsError> {
    let views: Vec<&[u8]> = st.blocks.iter().map(|(_, d)| d.as_slice()).collect();
    let parts = audioframe::xma_parts(i64::from(st.channels), &views)?;
    if parts.is_empty() {
        return Err(SnsError::DecodeFailed("EA-XMA"));
    }
    let ch_all = st.channels as usize;
    let mut cols: Vec<(usize, Vec<i16>)> = Vec::with_capacity(parts.len());
    for (i, p) in parts.iter().enumerate() {
        let ch = 2.min(ch_all.saturating_sub(2 * i));
        if ch == 0 {
            return Err(SnsError::DecodeFailed("EA-XMA"));
        }
        audioframe::xma2_riff(&[], ch as i64, i64::from(st.rate), i64::from(st.samples))?;
        let dec = wavport::decode_xma_part(p, ch, st.rate).ok_or(SnsError::DecodeFailed("EA-XMA"))?;
        cols.push((ch, dec));
    }
    let min_rows = cols.iter().map(|(ch, c)| c.len() / ch).min().unwrap_or(0);
    let want = st.samples as usize;
    let mut pcm = Vec::with_capacity(want.saturating_mul(ch_all));
    for row in 0..want {
        if row < min_rows {
            for (ch, c) in &cols {
                let slice = c
                    .get(row * ch..(row + 1) * ch)
                    .ok_or(SnsError::DecodeFailed("EA-XMA"))?;
                pcm.extend_from_slice(slice);
            }
        } else {
            pcm.extend(std::iter::repeat_n(0i16, ch_all));
        }
    }
    Ok(pcm)
}

fn decode_xas_stream(st: &wave::Stream) -> Result<Vec<i16>, SnsError> {
    let ch = st.channels as usize;
    let total_bytes: usize = st.blocks.iter().map(|(_, b)| b.len()).sum();
    let mut data = Vec::with_capacity(total_bytes);
    for (_, b) in &st.blocks {
        data.extend_from_slice(b);
    }
    if ch == 0 || !data.len().is_multiple_of(audio::FRAME * ch) {
        return Err(SnsError::DecodeFailed("EA-XAS"));
    }
    let dec = audio::decode_frames(&data)?;
    let frames = data.len() / (audio::FRAME * ch);
    let per = audio::FRAME_SAMPLES;
    let total = (frames * per).min(st.samples as usize);
    let mut out = vec![0i16; total * ch];
    for (c, chunk) in dec.chunks_exact(per).enumerate() {
        let (f, k) = (c / ch, c % ch);
        for (s, &v) in chunk.iter().enumerate() {
            let row = f * per + s;
            if row < total
                && let Some(slot) = out.get_mut(row * ch + k)
            {
                *slot = v;
            }
        }
    }
    Ok(out)
}

fn pack_ea_header(
    version: u32,
    codec: u32,
    channels: u32,
    rate: u32,
    type_: u32,
    samples: u32,
) -> Result<(u32, u32), SnsError> {
    if channels == 0 || channels > 64 || rate >= (1 << 18) || samples >= (1 << 29) {
        return Err(SnsError::InvalidHeader {
            channels,
            rate,
            samples,
        });
    }
    let h1 = (version << 28) | (codec << 24) | ((channels - 1) << 18) | rate;
    let h2 = (type_ << 30) | samples;
    Ok((h1, h2))
}

fn encode_stream_to_sps(st: &wave::Stream, pcm: &[i16]) -> Result<Vec<u8>, SnsError> {
    let ch = st.channels as usize;
    let frames = audio::encode(pcm, ch)?;
    let blocks = audioframe::xas_blocks(&frames, u64::from(st.samples), u64::from(st.channels), st.rate)?;
    let header = pack_ea_header(1, CODEC_EA_XAS, st.channels, st.rate, 1, st.samples)?;
    let views: Vec<(u32, &[u8])> = blocks
        .iter()
        .map(|(k, d)| u32::try_from(*k).map(|k| (k, *d)).map_err(|_| wave::Error::TooLarge))
        .collect::<Result<_, _>>()?;
    Ok(wave::write_sps(header, &views)?)
}

/// Transcodes a single SPS v1 `.sns` stream from Xbox 360 EA-XMA to PC EA-XAS v1.
///
/// Streams already encoded as EA-XAS (`codec == 4`) are returned unchanged.
///
/// # Errors
/// Returns [`SnsError`] on malformed SPS data, unsupported codecs, or encoding failures.
pub fn transcode_sps_stream(data: &[u8]) -> Result<Vec<u8>, SnsError> {
    let st = wave::read_sps(data)?;
    if st.codec == CODEC_EA_XAS {
        return Ok(data.to_vec());
    }
    if st.codec != CODEC_EA_XMA {
        return Err(SnsError::UnsupportedCodec(st.codec));
    }
    let pcm = decode_xma_stream(&st)?;
    encode_stream_to_sps(&st, &pcm)
}

/// Transcodes a Battlefield 1943 SNR header + SNS v0 block stream into a PC SPS v1 stream.
///
/// # Errors
/// Returns [`SnsError`] on malformed SNR/SNS data or codec errors.
pub fn transcode_snr_sns_to_sps(snr_header: &[u8], sns_data: &[u8]) -> Result<Vec<u8>, SnsError> {
    let st = wave::read_snr(snr_header, Some(sns_data))?;
    let pcm = decode_stream_to_pcm(&st)?;
    encode_stream_to_sps(&st, &pcm)
}

/// Transcodes a Battlefield 1943 SNR header + SNS v0 block stream into an EA-XAS v1
/// `(new_snr_header_8bytes, new_sns_v0_blocks)` pair.
///
/// # Errors
/// Returns [`SnsError`] on malformed SNR/SNS data or codec errors.
pub fn transcode_snr_sns_to_sns(snr_header: &[u8], sns_data: &[u8]) -> Result<(Vec<u8>, Vec<u8>), SnsError> {
    let st = wave::read_snr(snr_header, Some(sns_data))?;
    let pcm = decode_stream_to_pcm(&st)?;
    let ch = st.channels as usize;
    let frames = audio::encode(&pcm, ch)?;
    let blocks = audioframe::xas_blocks(&frames, u64::from(st.samples), u64::from(st.channels), st.rate)?;
    let (h1, h2) = pack_ea_header(0, CODEC_EA_XAS, st.channels, st.rate, st.type_, st.samples)?;
    let mut hdr_out = Vec::with_capacity(8);
    hdr_out.extend_from_slice(&h1.to_be_bytes());
    hdr_out.extend_from_slice(&h2.to_be_bytes());
    let views: Vec<(u32, &[u8])> = blocks
        .iter()
        .map(|(k, d)| u32::try_from(*k).map(|k| (k, *d)).map_err(|_| wave::Error::TooLarge))
        .collect::<Result<_, _>>()?;
    let blocks_out = wave::write_sns(&views)?;
    Ok((hdr_out, blocks_out))
}

/// Splits a concatenated SPS v1 `.sns` bank into `(offset, slice)` ranges.
fn split_sps_bank(bank: &[u8]) -> Result<Vec<(usize, &[u8])>, SnsError> {
    let mut slices = Vec::new();
    let mut o = 0usize;
    while o < bank.len() {
        let start = o;
        let mut saw_end = false;
        while o + 4 <= bank.len() {
            let word_bytes: [u8; 4] = bank
                .get(o..o + 4)
                .and_then(|s| s.try_into().ok())
                .ok_or(SnsError::MalformedBank(o))?;
            let word = u32::from_be_bytes(word_bytes);
            let kind = word >> 24;
            let size = (word & 0x00FF_FFFF) as usize;
            if size < 4 || o + size > bank.len() {
                return Err(SnsError::MalformedBank(o));
            }
            o += size;
            if kind == 0x45 {
                saw_end = true;
                break;
            }
        }
        if !saw_end {
            return Err(SnsError::MalformedBank(start));
        }
        let slice = bank.get(start..o).ok_or(SnsError::MalformedBank(start))?;
        slices.push((start, slice));
    }
    if slices.is_empty() {
        return Err(SnsError::MalformedBank(0));
    }
    Ok(slices)
}

/// Inspects a single- or multi-stream SPS v1 `.sns` audio bank without decoding or transcoding
/// any audio payloads (fast header-only scan for GUI inspection and offset indexing).
///
/// # Errors
/// Returns [`SnsError`] if the bank framing or any stream header is malformed.
pub fn inspect_sps_bank(bank: &[u8]) -> Result<SnsBankReport, SnsError> {
    let slices = split_sps_bank(bank)?;
    let mut report = SnsBankReport::default();
    for (index, &(old_offset, raw)) in slices.iter().enumerate() {
        let st = wave::read_sps(raw)?;
        if st.codec == CODEC_EA_XAS {
            report.passthrough += 1;
        } else if st.codec == CODEC_EA_XMA {
            report.converted += 1;
        } else {
            return Err(SnsError::UnsupportedCodec(st.codec));
        }
        report.entries.push(SnsBankEntry {
            index,
            old_offset,
            old_size: raw.len(),
            new_offset: old_offset,
            new_size: raw.len(),
            channels: st.channels,
            rate: st.rate,
            samples: st.samples,
            codec_from: st.codec,
            codec_to: CODEC_EA_XAS,
        });
    }
    Ok(report)
}

/// Repacks a single- or multi-stream SPS v1 `.sns` audio bank from Xbox 360 EA-XMA to PC
/// EA-XAS v1 in parallel via Rayon, returning the repacked bank bytes and per-stream
/// offset relocation table.
///
/// # Errors
/// Returns [`SnsError`] if any stream in the bank is malformed or uses an unsupported codec.
pub fn repack_sps_bank(bank: &[u8]) -> Result<(Vec<u8>, SnsBankReport), SnsError> {
    type RepackedStream = (usize, usize, wave::Stream, Vec<u8>);
    let slices = split_sps_bank(bank)?;
    let results: Vec<Result<RepackedStream, SnsError>> = slices
        .par_iter()
        .map(|&(old_offset, raw)| {
            let st = wave::read_sps(raw)?;
            let out = if st.codec == CODEC_EA_XAS {
                raw.to_vec()
            } else if st.codec == CODEC_EA_XMA {
                let pcm = decode_xma_stream(&st)?;
                encode_stream_to_sps(&st, &pcm)?
            } else {
                return Err(SnsError::UnsupportedCodec(st.codec));
            };
            Ok((old_offset, raw.len(), st, out))
        })
        .collect();

    let mut out_bank = Vec::with_capacity(bank.len());
    let mut report = SnsBankReport::default();
    for (index, item) in results.into_iter().enumerate() {
        let (old_offset, old_size, st, bytes) = item?;
        let new_offset = out_bank.len();
        let new_size = bytes.len();
        if st.codec == CODEC_EA_XAS {
            report.passthrough += 1;
        } else {
            report.converted += 1;
        }
        report.entries.push(SnsBankEntry {
            index,
            old_offset,
            old_size,
            new_offset,
            new_size,
            channels: st.channels,
            rate: st.rate,
            samples: st.samples,
            codec_from: st.codec,
            codec_to: CODEC_EA_XAS,
        });
        out_bank.extend_from_slice(&bytes);
    }
    Ok((out_bank, report))
}

/// Summary report produced by [`port_streaming_sounds_archive`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamingPortReport {
    /// Total entries in the input streaming archive.
    pub total: usize,
    /// Entries transcoded from Xbox 360 EA-XMA (SNS v0 or SPS v1) to PC EA-XAS SPS v1.
    pub converted: usize,
    /// Entries already in PC EA-XAS SPS v1 format (passed through).
    pub passthrough: usize,
    /// Entries skipped (missing SNR header or decode error).
    pub skipped: usize,
}

/// Ports an Xbox 360 `streaming_sounds-00.fbrb` archive (containing BFBC1/BF1943 SNS v0
/// `*_WaveDataSns_{0,1,2}.res` payloads or SPS v1 banks) into a PC `streaming_sounds-00.fbrb`
/// archive where every entry is a PC EA-XAS v1 SPS v1 stream (`0x48` header + `0x44` blocks +
/// `0x45` end).
///
/// `wave_archives` provides the companion `level-00.fbrb` (etc.) bundles whose `Wave` resources
/// hold the 8-byte big-endian SNR headers required to decode BFBC1/BF1943 SNS v0 payloads.
#[must_use]
pub fn port_streaming_sounds_archive(
    sns_arc: &crate::fbrb::FbrbArchive,
    wave_archives: &[&crate::fbrb::FbrbArchive],
) -> (crate::fbrb::FbrbArchive, StreamingPortReport) {
    use std::collections::HashMap;

    let mut snr_headers: HashMap<String, [u8; 8]> = HashMap::new();
    for arc in wave_archives {
        for entry in &arc.entries {
            if entry.ext != "Wave" {
                continue;
            }
            let lower = entry.path.replace('\\', "/").to_ascii_lowercase();
            let Some(stem) = lower.strip_suffix("_wave.res") else {
                continue;
            };
            let Ok(w) = wave::read_wave(&entry.data, wave::Source::Bf1943) else {
                continue;
            };
            for (seg_i, seg) in w.segments.iter().enumerate() {
                if let Some((data, _info)) = seg
                    && data.len() == 8
                    && let Ok(hdr) = <[u8; 8]>::try_from(data.as_slice())
                {
                    snr_headers.insert(format!("{stem}_wavedatasns_{seg_i}.res"), hdr);
                    if seg_i == 1 {
                        snr_headers.entry(format!("{stem}_wavedatasns.res")).or_insert(hdr);
                    }
                }
            }
        }
    }

    let results: Vec<(crate::fbrb::FbrbEntry, u8)> = sns_arc
        .entries
        .par_iter()
        .map(|entry| {
            let mut out = entry.clone();
            if entry.data.starts_with(b"\x48\x00\x00\x0c") {
                match repack_sps_bank(&entry.data) {
                    Ok((pc, rep)) if rep.converted > 0 => {
                        out.data = pc;
                        return (out, 1);
                    }
                    Ok(_) => return (out, 2),
                    Err(_) => return (out, 0),
                }
            }
            let key = entry.path.replace('\\', "/").to_ascii_lowercase();
            if let Some(hdr) = snr_headers.get(&key)
                && let Ok(sps) = transcode_snr_sns_to_sps(hdr, &entry.data)
            {
                out.data = sps;
                return (out, 1);
            }
            (out, 0)
        })
        .collect();

    let mut report = StreamingPortReport {
        total: results.len(),
        ..Default::default()
    };
    let mut entries = Vec::with_capacity(results.len());
    for (entry, tag) in results {
        match tag {
            1 => {
                report.converted += 1;
                entries.push(entry);
            }
            2 => {
                report.passthrough += 1;
                entries.push(entry);
            }
            _ => {
                report.skipped += 1;
            }
        }
    }
    (
        crate::fbrb::FbrbArchive {
            version: sns_arc.version,
            entries,
        },
        report,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn make_xas_sps(channels: u32, rate: u32, samples: u32) -> Vec<u8> {
        let pcm: Vec<i16> = (0..(samples as usize * channels as usize))
            .map(|i| ((i * 37) % 10_000) as i16)
            .collect();
        let frames = audio::encode(&pcm, channels as usize).unwrap();
        let blocks = audioframe::xas_blocks(&frames, u64::from(samples), u64::from(channels), rate).unwrap();
        let header = pack_ea_header(1, CODEC_EA_XAS, channels, rate, 1, samples).unwrap();
        let views: Vec<(u32, &[u8])> = blocks.iter().map(|(k, d)| (*k as u32, *d)).collect();
        wave::write_sps(header, &views).unwrap()
    }

    #[test]
    fn write_sns_round_trips_with_read_snr() {
        let header = pack_ea_header(0, CODEC_EA_XAS, 1, 24_000, 1, 256).unwrap();
        let mut snr = Vec::new();
        snr.extend_from_slice(&header.0.to_be_bytes());
        snr.extend_from_slice(&header.1.to_be_bytes());

        let sns = wave::write_sns(&[(128, b"block-one-payload"), (128, b"block-two")]).unwrap();
        let st = wave::read_snr(&snr, Some(&sns)).unwrap();
        assert_eq!(st.codec, CODEC_EA_XAS);
        assert_eq!(st.rate, 24_000);
        assert_eq!(st.samples, 256);
        assert_eq!(st.blocks.len(), 2);
        assert_eq!(st.blocks[0], (128, b"block-one-payload".to_vec()));
        assert_eq!(st.blocks[1], (128, b"block-two".to_vec()));
    }

    #[test]
    fn repacks_multi_stream_sps_bank_and_tracks_offsets() {
        let s0 = make_xas_sps(1, 24_000, 256);
        let s1 = make_xas_sps(2, 48_000, 384);
        let mut bank = Vec::new();
        bank.extend_from_slice(&s0);
        bank.extend_from_slice(&s1);

        let (repacked, report) = repack_sps_bank(&bank).unwrap();
        assert_eq!(repacked, bank);
        assert_eq!(report.entries.len(), 2);
        assert_eq!(report.passthrough, 2);
        assert_eq!(report.converted, 0);
        assert_eq!(report.entries[0].old_offset, 0);
        assert_eq!(report.entries[0].old_size, s0.len());
        assert_eq!(report.entries[1].old_offset, s0.len());
        assert_eq!(report.entries[1].new_offset, s0.len());
        assert_eq!(report.entries[1].channels, 2);
        assert_eq!(report.entries[1].rate, 48_000);

        let (pcm, ch, rate) = decode_sps_to_pcm(&s1).unwrap();
        assert_eq!((ch, rate, pcm.len()), (2, 48_000, 384 * 2));
    }

    #[test]
    fn transcode_snr_sns_xas_to_sps_and_sns() {
        let pcm: Vec<i16> = (0..256).map(|i| (i * 50) as i16).collect();
        let frames = audio::encode(&pcm, 1).unwrap();
        let blocks = audioframe::xas_blocks(&frames, 256, 1, 24_000).unwrap();
        let (h1, h2) = pack_ea_header(0, CODEC_EA_XAS, 1, 24_000, 1, 256).unwrap();
        let mut snr = Vec::new();
        snr.extend_from_slice(&h1.to_be_bytes());
        snr.extend_from_slice(&h2.to_be_bytes());
        let views: Vec<(u32, &[u8])> = blocks.iter().map(|(k, d)| (*k as u32, *d)).collect();
        let sns = wave::write_sns(&views).unwrap();

        let sps = transcode_snr_sns_to_sps(&snr, &sns).unwrap();
        let (decoded, ch, rate) = decode_sps_to_pcm(&sps).unwrap();
        assert_eq!((ch, rate, decoded.len()), (1, 24_000, 256));

        let (repacked_hdr, repacked_blocks) = transcode_snr_sns_to_sns(&snr, &sns).unwrap();
        let (decoded2, ch2, rate2) = decode_snr_sns_to_pcm(&repacked_hdr, &repacked_blocks).unwrap();
        assert_eq!((ch2, rate2, decoded2.len()), (1, 24_000, 256));
    }
}
