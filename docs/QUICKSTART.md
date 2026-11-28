# Quick Start Guide

## 1. Prerequisites

* **Rust toolchain:** Rust `1.100.0` (automatically selected by [`rust-toolchain.toml`](../rust-toolchain.toml) via `rustup`).
* **Platform:** Pure Rust, with no C/C++ or Python build dependencies. It is built and tested on Windows; other platforms are untested (checked 2026-11-28). `compile-xenos-shaders` also needs Microsoft's `fxc.exe` (from the Windows SDK, or named by the `FXC_PATH` environment variable).

## 2. Build, Test, Lint, and Docs

Run all commands from the repository root (`xenolith/`):

```powershell
# Run all unit, property (proptest), integration, and doc tests
cargo test

# Enforce strict pedantic Clippy lints and zero warnings
cargo clippy --all-targets -- -D warnings

# Generate and open full HTML API documentation
$env:RUSTDOCFLAGS="-D warnings"
cargo doc --workspace --no-deps --open

# Check formatting
cargo fmt --check

# MIT-only build: no x360-xma (LGPL-2.1-or-later) in the dependency graph
cargo build -p xenolith --no-default-features --features frostbite
cargo tree -p xenolith --no-default-features --features frostbite   # must not list x360-xma
```

### Launching Xenolith (Desktop GUI & Headless CLI)

```powershell
# Run desktop GUI in development mode
cargo run -p xenolith

# Build standalone release executable (GUI when run with no args, CLI when passed a subcommand)
cargo build --release -p xenolith
.\target\release\xenolith.exe

# Headless CLI examples
.\target\release\xenolith.exe inspect level-00.fbrb
.\target\release\xenolith.exe port-fbrb --bfbc1 --havok-exe BFBC2Game.exe --havok-template havok_550_master_template.res level-00.fbrb level-00.pc.fbrb
.\target\release\xenolith.exe dedup-fbrb level-00.pc.fbrb mp_common_level-00.fbrb level-00.dedup.fbrb
.\target\release\xenolith.exe port-streaming-sounds --waves level-00.fbrb streaming_sounds-00.fbrb streaming_sounds.pc.fbrb
.\target\release\xenolith.exe repack-sns music.sns music.pc.sns
.\target\release\xenolith.exe compile-xenos-shaders --profile dx9 Single.res out_dx9/
.\target\release\xenolith.exe compile-xenos-shaders --profile dx10 Single.res out_dx10/
.\target\release\xenolith.exe upgrade-dbx old.dbx upgraded.dbx
.\target\release\xenolith.exe dbx-to-xml level.dbx level.xml
.\target\release\xenolith.exe xml-to-dbx level.xml level.dbx
.\target\release\xenolith.exe upgrade-hpd bfbc1_pathdata.hpd pc_pathdata.hpd
.\target\release\xenolith.exe make-sector-offsets sp_01_greenacres Infantry 0 0 sector_offsets.dbx
.\target\release\xenolith.exe register-level startup-00.fbrb mp_beachhead_day1 MP_012GR
.\target\release\xenolith.exe unwrap-bink greenacres.res greenacres.bik
```

---

## 3. Usage Examples

### A. Untile a Raw Xbox 360 Xenos Texture (Engine-Agnostic)

```rust
use x360_xenos_texture::{SurfaceDesc, XenosFormat, untile};

fn untile_dxt5(raw_tiled_payload: &[u8]) -> Result<Vec<u8>, x360_xenos_texture::TextureError> {
    let desc = SurfaceDesc {
        width: 512,
        height: 512,
        mips: 10,
        slices: 1, // 6 for cube maps
        format: XenosFormat::Dxt5,
    };
    let surface = untile(raw_tiled_payload, &desc)?;
    println!("Mip 0 size: {} bytes", surface.level_sizes[0]);
    Ok(surface.data)
}
```

### B. Parse, Disassemble, Execute, and Translate an Xbox 360 Shader

```rust
use std::collections::BTreeMap;
use x360_xenos_shader::{disasm, exec, hlsl, xenos};

fn inspect_and_run_shader(xdk_blob: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Parse XDK container + Xenos microcode
    let shader = xenos::parse(xdk_blob, Some(true))?;

    // 2. Disassemble microcode
    let listing = disasm::body(&shader, false, shader.is_pixel).join("\n");
    println!("{listing}");

    // 3. Execute pixel shader on 4 pixels on CPU
    let inputs = BTreeMap::new();
    let consts = BTreeMap::from([(0u32, [1.0, 0.5, 0.25, 1.0])]);
    let textures = BTreeMap::new();
    let result = exec::run(&shader, &inputs, &consts, &textures, 4, None, None)?;
    println!("Surviving pixels: {:?}", result.alive);

    // 4. Translate to Direct3D 10/11 HLSL
    let iface = hlsl::Interface::new(Vec::new());
    let hlsl_source = hlsl::translate(&shader, &iface)?;
    println!("{hlsl_source}");
    Ok(())
}
```

### C. Decode an Xbox 360 XMA2 RIFF Audio Stream

```rust
fn decode_xma2_to_pcm16(riff_bytes: &[u8]) -> Result<(u32, Vec<i16>), x360_xma::Error> {
    let rate = x360_xma::parse_riff(riff_bytes)?.rate; // the sample rate is the RIFF's
    let decoded = x360_xma::decode_riff(riff_bytes)?;
    let pcm16 = x360_xma::to_s16(&decoded.planar);
    Ok((rate, pcm16))
}
```

### D. Convert a Big-Endian Havok 5.5 Packfile to PC

```rust
use x360_havok::convert::{Classes, convert_packfile};

fn convert_havok_to_pc(console_packfile: &[u8], classes: &Classes) -> Result<Vec<u8>, x360_havok::convert::HavokError> {
    convert_packfile(console_packfile, 0, classes)
}
```

### E. Convert a Frostbite `XenonTexture` to PC `ITexture`

```rust
fn convert_frostbite_texture(xenon_res: &[u8]) -> Result<Vec<u8>, x360_frostbite::texture::TextureError> {
    x360_frostbite::texture::convert(xenon_res)
}
```

---

## 4. Using Individual Crates in Your Own Project

Because the workspace is a layered modular monolith, you can depend on only the crates you need (for example, an MIT-only project that wants Xenos texture untiling and shaders without `x360-xma`; `x360-frostbite` is MIT-only with `default-features = false`):

```toml
[dependencies]
x360-xenos-texture = { path = "../xenolith/platform/x360-xenos-texture" }
x360-xenos-shader  = { path = "../xenolith/platform/x360-xenos-shader" }
x360-frostbite     = { path = "../xenolith/engines/frostbite", default-features = false }
```
