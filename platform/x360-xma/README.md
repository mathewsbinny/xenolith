# x360-xma: an XMA2 decoder

FFmpeg's `xma2` decoder (`libavcodec/wmaprodec.c`, FFmpeg 9.0) ported to Rust, so the Xbox 360's EA-XMA sounds are
decoded in process instead of by running ffmpeg once per sound. It follows FFmpeg's C code operation for operation,
from the WAV demuxer's packets to libswresample's 16-bit samples, and gives `ffmpeg -cpuflags 0`'s samples to the bit
on every XMA stream of BFBC2's Xbox copy and Battlefield 1943. The crate is a copy of bfbc2-tool-suite's `bc2-xma`
(its round PR1b-1c), where the proof lives: spec rule XMA-DEC-1 in the suite's `spec/formats/audio.md` and the
measurements in its `rust/BENCHMARKS.md`, "PR1b-1c".

    let d = x360_xma::decode(packets, channels, rate, byte_rate)?;   // one stream of 1 or 2 channels
    let pcm = x360_xma::to_s16(&d.planar);                            // interleaved, as ffmpeg's s16le

## Licence

This crate is a translation of FFmpeg's code and tables (`wmaprodec.c`, `wmaprodata.h`, `wma.c`, `vlc.c`,
`libavutil/tx_template.c`, `float_dsp.c`, `sinewin_tablegen.h`, `wavdec.c`, `pcm.c`, `decode.c`), whose authors hold
their copyright (Baptiste Coudurier, Benjamin Larsson, Ulion, Sascha Sommer, Lynne and the other FFmpeg
developers). It is therefore licensed under FFmpeg's licence, the **GNU Lesser General Public License, version 2.1
or (at your option) any later version** (`license = "LGPL-2.1-or-later"` in its `Cargo.toml`; the user's choice,
2026-11-22), not under the workspace's MIT licence. The licence's text is `LICENSE` beside this file. The rest of the
workspace (MIT, the repository's `LICENSE`) uses it only behind the `xma` Cargo feature (`x360-frostbite`'s and the
`xenolith` application's, on by default): built with `default-features = false`, a crate links no LGPL code. Its
source is this folder.

FFmpeg is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty
of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU Lesser General Public License for more details:
<https://www.gnu.org/licenses/old-licenses/lgpl-2.1.html>.
