# Xbox 360 to PC Porting Tools

A modular, pure-Rust toolkit for reading, decoding, and converting Xbox 360 containers, GPU/audio hardware formats, third-party middleware, and game-engine assets to PC formats.

Extracted from the console-to-PC conversion stack of `bfbc2-tool-suite` and organized so additional middlewares (PhysX, Morpheme, Bink, ...) and engines (Unreal Engine 3, MT Framework, ...) can be added side by side while sharing the common Xbox 360 hardware and middleware crates.

## Workspace Layout

```text
platform/                   Engine-agnostic Xbox 360 hardware & OS layer
├── x360-containers/        STFS (LIVE / PIRS / CON) packages & XISO disc images
├── x360-xma/               Pure-Rust XMA2 audio decoder (LGPL-2.1-or-later)
├── x360-xenos-shader/      XDK shader container, Xenos microcode decoder, disassembler & HLSL translator
└── x360-xenos-texture/     Xenos 32x32 block tiling, mip packing, endian swap & texture format decoding

middleware/                 Engine-agnostic third-party middleware converters
├── havok/                  Havok packfile reader, reflection, endian swapper, MOPP & 4.6 -> 5.5 porting
└── granny/                 RAD Granny 2 (.gr2) reader, BE -> LE conversion, curve & CRC editing

engines/                    Engine-specific adapters built on platform/ and middleware/
└── frostbite/              Frostbite 1 / 1.5 adapters (XenonTexture/ITexture, MeshData/MeshSet,
                            terrain/water/soot, EA-XAS/Wave audio, Havok wrapper/PE classes, AimAnimation)
```

## Licensing

The workspace is licensed under the **MIT License** (`LICENSE`), with one exception:
- `platform/x360-xma` is a Rust port of FFmpeg's WMA Pro / XMA2 decoder and is licensed under **LGPL-2.1-or-later** (see `platform/x360-xma/LICENSE`).

## Building & Testing

```powershell
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## Expanding the Toolkit

- **New Xbox 360 hardware/OS formats** -> add a crate under `platform/`
- **New third-party middleware** (e.g., PhysX, Bullet, Morpheme, FaceFX, Scaleform, Bink) -> add a crate under `middleware/`
- **New game engines** (e.g., `engines/unreal3/`, `engines/mtframework/`) -> add a crate under `engines/` depending on whichever `platform/*` and `middleware/*` crates it needs.
