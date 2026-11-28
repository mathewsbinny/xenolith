# API Reference

This reference summarizes the public modules, types, functions, and error contracts across all 8 workspace crates (checked against the code of `main`, 2026-11-28). For full inline documentation and rustdoc cross-links, run:

```powershell
cargo doc --workspace --no-deps --open
```

---

## 1. Platform Layer (`platform/`)

### `x360-containers` ([`platform/x360-containers`](../platform/x360-containers/src/lib.rs))
Read-only parsers for Xbox 360 storage containers.

* **`stfs`** — Xbox 360 STFS packages (`CON `, `PIRS`, `LIVE`), read through any `Read + Seek` source.
  * `Package::open(f: R) -> Result<Package<R>, Error>` — Reads the header (`Header`: magic, block counts, table offsets, metadata) and the file table (`Entry`: paths, sizes, starting blocks).
  * `Package::from_header(f: R, header: Header) -> Package<R>` — Builds a reader from an already-parsed header.
  * `Package::read(&mut self, start, count, consecutive, size) -> io::Result<Vec<u8>>` — Extracts a file's payload, following the hash-table block chains.
* **`xiso`** — Xbox 360 XISO disc images (`MICROSOFT*XBOX*MEDIA`), read through any `Read + Seek` source.
  * `find_partition(f) -> Result<u64, Error>` — Finds the XDVDFS partition (`PARTITIONS`: the four known offsets).
  * `root(f, base) -> io::Result<Option<(u32, u32)>>` / `walk(...)` — Walks the binary-tree directory structure (to `MAX_DEPTH`), refusing cyclic directory loops; files come back as `File = (Vec<u8>, u32, u32)`.
* **`xex`** — Xbox 360 `XEX2` executables.
  * `parse(data: &[u8]) -> Result<XexHeader, XexError>` — Parses optional headers (`Title ID`, `Media ID`, `entry_point`, `image_base`), security info, and 24-byte SHA-1 page descriptors.
  * `XexHeader::verify_image_pages(decoded_image: &[u8]) -> Result<PageVerifyReport, XexError>` — Authenticates a decoded PE image against the reverse-linked SHA-1 page-digest chain.

---

### `x360-xma` ([`platform/x360-xma`](../platform/x360-xma/src/lib.rs)) *(LGPL-2.1-or-later)*
Pure-Rust XMA2 audio decoder.

* **Types:** `Decoded { planar: Vec<Vec<f32>>, errors: u32 }` (`errors`: the errors FFmpeg would have logged; the sample rate is the RIFF's, `Riff::rate`), `Riff<'a> { channels, rate, byte_rate, streams, data }`, `Error`.
* **Functions:**
  * `parse_riff(b: &[u8]) -> Result<Riff<'_>, Error>` — Extracts `fmt ` and `data` chunks from a RIFF WAVE XMA2 stream.
  * `decode(data: &[u8], channels: usize, rate: u32, byte_rate: u32) -> Result<Decoded, Error>` — Decodes raw 2048-byte XMA2 packets into planar `f32` PCM.
  * `decode_riff(riff: &[u8]) -> Result<Decoded, Error>` — Convenience wrapper combining `parse_riff` + `decode`.
  * `to_s16(planar: &[Vec<f32>]) -> Vec<i16>` — Interleaves planar `f32` channels into signed 16-bit PCM (ties-to-even rounding with saturation).

---

### `x360-xenos-texture` ([`platform/x360-xenos-texture`](../platform/x360-xenos-texture/src/lib.rs))
Engine-agnostic Xbox 360 Xenos GPU texture untiler and format decoder.

* **Types:**
  * `XenosFormat`: `Dxt1`, `Dxt3`, `Dxt5`, `Dxt5a`, `Bc5`, `Rgb565`, `Argb8`, `L8`, `Rgba16f` (`block_info() -> (bpb, edge)`).
  * `SurfaceDesc { width, height, mips, slices, format }` — Metadata for a raw tiled payload.
  * `UntiledSurface { level_sizes: Vec<usize>, data: Vec<u8> }` — Linear slice-major, mip-major output.
  * `Place { start, pitch, x, y }` — Tiled byte/block location of one mip level.
  * `TextureError`: `Refused(String)`, `OutOfRange`.
* **Functions:**
  * `untile(raw: &[u8], desc: &SurfaceDesc) -> Result<UntiledSurface, TextureError>` — Single-pass untiling with on-the-fly 16-bit endian swap and console-format decode (`Dxt5a -> L8`, `Rgb565 -> BGRA8`, `Argb8 -> BGRA8`).
  * `tiled(x: usize, y: usize, pitch: usize, log_bpp: u32) -> usize` — `XGAddress2DTiledOffset` block addressing.
  * `packed(w: usize, h: usize, mip: usize, edge: usize) -> Option<(usize, usize)>` — Packed mip-tail tile offset.
  * `layout(w, h, mips, fmt, slices) -> (Vec<Vec<Place>>, usize)` / `layout_raw(...)` — Computes 4 KB-aligned Xenos storage layout.
  * `swap16(data: &mut [u8])`, `rows4(level: &[u8], w, h) -> Vec<u8>`, `dxt5a_block([u8; 8]) -> [u8; 16]`.

---

### `x360-xenos-shader` ([`platform/x360-xenos-shader`](../platform/x360-xenos-shader/src/lib.rs))
Xbox 360 XDK shader container parser, Xenos microcode disassembler, HLSL translator, and CPU reference interpreter.

* **`xenos`** — XDK container & microcode parser.
  * `parse(blob: &[u8], is_pixel: Option<bool>) -> Result<Shader, XenosError>` — Parses container header, constant table (`Ctab`), float/int literals, shader header, CF slots, and ALU/fetch instructions.
  * `fetch_const(f: &Fetch) -> u32`.
* **`disasm`** — Human-readable microcode listing.
  * `body(sh: &Shader, show_raw: bool, is_pixel: bool) -> Vec<String>` — Formats CF labels, ALU vector/scalar pairs, predicates, and fetches.
* **`hlsl`** — Xenos → Direct3D 10/11 HLSL translator. A copy of the suite's `bc2-shader` translator that has drifted from it (2026-11-27: main lacks the suite's forward-jump emitter); keep the two in step until round XM4 makes one the other.
  * `translate(sh: &Shader, iface: &Interface) -> Result<String, TranslateError>` — Emits HLSL bound to the caller's `Interface`.
  * `translate_all(...)` / `crate::translate_blobs(...)` — Parallel batch translation via Rayon.
* **`pyrepr`** — Python's `repr(float)` (the shortest round-tripping digits), which the HLSL text writes its numbers with, kept from the port off the suite's Python translator (a porting crutch; the suite plans to retire such Python-isms).
* **`exec`** — Pure-Rust CPU reference interpreter (pixel **and** vertex shaders).
  * `run(sh, inputs, consts, textures, n, pos, front) -> Result<ExecResult, ExecError>` — Runs with default `Options`.
  * `run_with(..., opts: &Options) -> Result<ExecResult, ExecError>` — Full control over `filter_bits`, `zero_outside`, `trace`, `continue_after_kill`, `bool_consts`, `loop_consts`, and `vertex_buffers`.
  * **Key Types:** `Texture`, `AddressMode::{Wrap, Clamp, Zero}`, `VertexBuffer`, `LoopConst`, `Options`, `ExecResult { outputs, pos, interpolators, alive, snaps }`, `Snapshot`.

---

## 2. Middleware Layer (`middleware/`)

### `x360-havok` ([`middleware/havok`](../middleware/havok/src/lib.rs))
Havok Physics middleware tools (4.6 / 5.5).

* **`packfile`** — Havok packfile parser, pointer fixups, collision triangle extraction (`triangles`), and reflection (`packfile::reflect`).
* **`convert`** — Data-driven Xbox 360 → PC Havok 5.5.0 endian converter.
  * `Classes::new(&[Klass])` — Class reflection table describing member offsets and types.
  * `convert_packfile(data: &[u8], start: usize, classes: &Classes) -> Result<Vec<u8>, HavokError>` — Field-by-field endian conversion with byte-ownership conflict detection.
  * `verify_packfile(...)` — Per-byte diff/ownership verifier against a PC packfile.
* **`mopp`** — Havok MOPP BVH bytecode decoder, validator, and raycast checker.
* **`port`** — Havok 4.6.1 → 5.5.0 packfile structure porting.

---

### `x360-granny` ([`middleware/granny`](../middleware/granny/src/lib.rs))
RAD Game Tools Granny 2 (`.gr2`) skeleton and animation tools.

* **`format`** — PC & Xbox 360 `.gr2` parser (`Granny`), object tree reader, bone tables (`bone_table`, `renumber`, `TableKind`).
* **`convert`** — Xbox 360 → PC endian conversion.
  * `to_pc(b: &[u8], bones: Option<&HashMap<u32, u32, S>>) -> Result<Vec<u8>, GrannyError>` — Converts big-endian `.gr2` to little-endian with optional bone renumbering and CRC recomputation.
  * `to_pc_all(...)` — Parallel batch conversion via Rayon; `bone_names(b)` and `bone_map(src, dst)` build the renumbering map.
* **`edit`** — Curve key sampling, position shifting, orientation curve writing, track addition, and CRC32 recomputation.

---

## 3. Engine Adapters (`engines/`)

### `x360-frostbite` ([`engines/frostbite`](../engines/frostbite/src/lib.rs))
Frostbite 1 / 1.5 engine-specific Xbox 360 → PC adapters (also re-exports `containers`, `xenos_texture`, `shader`, `havok`, `granny`, and `xma` when the `xma` feature is on).

* **Cargo features:** `default = ["xma"]`. With `default-features = false` the crate links no LGPL code; the XMA2-backed audio paths (`wavport`, Wave porting in `bundle_port`) need `xma`.
* **Known defects (round XM0, 2026-11-27; checked 2026-11-28).** The `fbrb`, `dbx` and `apt` modules are xenolith's own rewrites, not the suite's verified readers (`bfbc2-tool-suite` `spec/formats/fbrb.md`, `dbx.md`). Until round XM3 replaces them with the suite's code, do not rely on them for byte-exact output:
  * `fbrb`: the header and payload are unzipped when they *start with* `1F 8B`, not by the header's zipped byte (FBRB-HDR-10); `parse` drops each entry's size2 and `to_bytes` writes size2 = size (wrong for RES-wrapped entries, FBRB-ENT-5); DICE's padded layouts of the async / streaming bundles are not kept (FBRB-LAY-*); names and types share one interned string table and lookups are case-sensitive (FBRB-ENT-9: the game's are not). The module's own doc comment omits the header's trailing zipped byte and payload-length word.
  * `dbx`: `to_xml` prints every packed number as an `i32` (`engines/frostbite/src/dbx.rs:264`), so a float field comes out as its bit pattern; `from_xml` infers kinds from the text (`dbx.rs:559-573`: `true` / `false` and `/`-separated integers), so a string field holding `"42"` or `"true"` changes kind; the string table is not in DICE's order.
  * `apt`: one of three BIGF implementations across the repositories, each handling duplicate names differently.

* **`texture`** — Frostbite 92-byte `XenonTexture` → `ITexture` wrapper over `x360-xenos-texture` (auto-unwraps 64-byte `RES\x02` prefixes and supports 2D, cube, and 3D volume textures).
  * `convert(b: &[u8]) -> Result<Vec<u8>, TextureError>` / `convert_all(items: &[&[u8]])` / `unwrap_res_header(b: &[u8])`.
  * `xenos_format(fmt: u32) -> Option<XenosFormat>`, `format(fmt: u32)`, `layout(...)`.
* **`fbrb`** — Frostbite `FbRB` v2 archive reader & writer (`FbrbArchive::parse`, `FbrbArchive::to_bytes`).
* **`bundle_port`** — End-to-end Rayon-parallel `FbRB` level/asset bundle porting orchestrator (`port_archive`, `port_fbrb_bytes`, `PortOptions`, `BundlePortReport`) dispatching textures, terrain (`64`/`128`/`256`/`512`/`1024`/`2048`), water, `Wave` (in-memory XMA2 → EA-XAS, optional external `streaming_sounds` resolution, and automatic PC streaming `Wave` header fallback via `wavport::convert_streaming_stub`), `.sns` banks, `ShaderDatabase`, `*MeshSet` & `MeshData` (lowercase/`_zonly` layout fallback and stream-0 vertex emission), `AnimTreeInfo` v6 BE→LE, Granny 2, `AimAnimation`, APT, Havok (`4.6.1 → 5.5.0` via `HavokPortContext`), unified `ske01` `bone_map` threading, and optional BFBC1 1.0 → 1.5 upgrades.
* **`bfbc1`** (`engines/frostbite/src/bfbc1/{mod,dbx,hpd,terrain,havok,dedup}.rs`, re-exported at `x360_frostbite::bfbc1`) — BFBC1 (Frostbite 1.0 → 1.5) schema and asset upgraders: empirical DBX schema migration (`upgrade_dbx`, `upgrade_dbx_bytes`, `DbxSchemaRules::bfbc1_to_fb15` — lowers `Audio.ValueRef` defaults, drops `LevelReferencedAssetsData` & `RichPresenceContextPair`, clears dangling GUID refs and prunes `<item ref="null"/>` slots inside `<array>` containers, renames `RegroupOrderEntityData`, wraps `Mesh1p`/`Mesh3p`, normalizes `EIA*`→`Eia*`, remaps `BoneId`), Frostbite 1.0 `0x29`/`256×256`/`512×512` heightfield upgrader (`upgrade_heightfield`, `downsample_sub_heightfield_256_to_128`), PC vertex layout & `MeshData` upgrader (`compute_pc_vertex_layout`, `build_pc_layout_body`, `upgrade_mesh_header`), Havok `4.6.1 → 5.5.0` structural porting (`HavokPortContext`, `port_havok_461_to_550_pc`, `inspect_havok_header`, `upgrade_havok_packfile`), Bink `RES\x02` movie unwrapper (`unwrap_bink_movie`), Kynogon HPD v1 → v2 navmesh upgrader (`upgrade_kynogon_hpd`, `HpdUpgradeReport` — upgrades `1.3f` headers and compacts 20-byte edges to 12-byte edges while preserving 36-byte nodes and `"Kynogon InPlace Mesh"` payloads), `PathdataSectorOffsets` binary DBX builder (`build_sector_offsets_dbx`), and shared-common `FbRB` deduplicators (`deduplicate_common_entries`, `deduplicate_common_entries_guid_aware`).
* **`dbx`** — Frostbite 1 / 1.5 `{binary}` `.dbx` partition parser (`parse`), deterministic binary serializer (`DbxFile::to_bytes`), XML exporter (`DbxFile::to_xml`), and pure-Rust XML-to-DBX compiler (`from_xml`).
* **`apt`** — EA APT `BIGF` (`.apt`/`.const`/`.geo`) archive reader, writer, BE→LE structural converter, and `FSCommand`/`extern.*`/console-prompt symbol inspector (`parse_bigf`, `write_bigf`, `convert_bigf_be_to_le`, `inspect_apt_symbols`).
* **`menu_port`** — Battlefield 1943 Xbox 360 → PC `mainmenu-00.fbrb` converter (APT BE → LE, `XenonTexture` → `ITexture`, `Wave` → EA-XAS, `_Xenon` → `_Win32` names in DBX).
* **`menu_export`** — Pure-Rust BF1943 Xbox 360 UI art (PNG), UI SFX (PCM16 WAV), and `EurostileLTStd-BoldCn` bitmap font + kerning exporter (`export_bf1943_menu_assets`, `decode_itexture_rgba`, `encode_rgba_png`, `encode_pcm16_wav`, `parse_dbx_font_glyphs`, `parse_dbx_font_kerning`).
* **`texcheck`** — PC `ITexture` mip-level reader (`levels`) and DXT1/DXT3/DXT5/BC5/L8 pixel decoder (`decode`).
* **`mesh` / `meshconv` / `meshdata`** — Frostbite `MeshData` / `MeshSet` headers, vertex-element conversion (packed normals/colors → half-floats), relayout, and triangle-strip unrolling.
* **`terrain`** — Console terrain heightfield (`64`/`128`/`256`/`1024`/`2048`), water mesh, material map, visual terrain/water, and soot conversion.
* **`audio` / `audioframe` / `wave` / `wavport` / `sns`** — EA-XAS v1 encoder/decoder (`encode`, `encode_all`, `decode_frames`), Frostbite Wave / SPS / SNR / SNS stream framing (`write_sns`, `write_sps`), XMA2 → EA-XAS conversion (`wavport::convert`, `wavport::convert_streaming_stub`, backed by `x360-xma`), header-only `.sns` bank inspection (`inspect_sps_bank`), multi-stream `.sns` audio bank repacking with offset relocation (`repack_sps_bank`, `transcode_sps_stream`, `transcode_snr_sns_to_sps`, `transcode_snr_sns_to_sns`), and whole-archive `streaming_sounds-00.fbrb` porting (`port_streaming_sounds_archive`).
* **`shader_db`** — Direct3D 10/11 `DXBC` container parser/writer/validator (`DxbcContainer`, `dxbc_checksum` verified bit-exact against `fxc.exe`, `strip_debug_chunks`), Frostbite `ShaderDatabase` DXBC repacker (`repack_dxbc_database`), Xbox 360 `ShaderDatabase` Xenos scanner & parallel HLSL translator (`extract_xenos_shaders`, `translate_xenos_database`), DirectX 9 SM 3.0 HLSL lowering (`hlsl_dx10_to_dx9`), and `fxc.exe` compiler bridge (`find_fxc`, `compile_hlsl_with_fxc`, `compile_xenos_database` for `Dx9` / `Dx10` / `Dx11`).
* **`hkclasses` / `pe`** — 32-bit PE image reader and `hkClass` layout extractor from PC Frostbite executables.
* **`aimanim`** — Frostbite `AimAnimation` parser, writer, bone renumberer, and `to_pc` converter.

---

## 4. Applications (`apps/`)

### `xenolith` ([`apps/xenolith`](../apps/xenolith/src/main.rs))
Desktop GUI & headless CLI (`target/release/xenolith.exe`) built with `eframe`, `egui`, `egui_dock`, and [`design/tokens.json`](../design/tokens.json).

* **Cargo features:** `default = ["frostbite", "xma"]` (disable `xma` for a strictly MIT-only build).
* **Modules:**
  * `theme` — Parses `design/tokens.json` into Dark/Light `Palette`s and configures `egui::Visuals` + `egui_dock::Style`.
  * `task` — Panic-guarded background worker (`Task<T>`).
  * `pipeline` — Magic-byte asset inspection (`inspect_file` incl. XEX2, FbRB, DBX, APT, Wave, header-only `.sns`, DXBC/Xenos `ShaderDatabase`, Havok SDK), batch job execution (`run_job`), and PCM16 `.wav` encoding (`encode_wav_pcm16`).
  * `cli` — Headless CLI subcommands (`inspect`, `port-fbrb [--bfbc1] [--streaming-sounds] [--havok-exe] [--havok-template]`, `unpack-fbrb`, `repack-sns`, `port-streaming-sounds --waves`, `repack-shaderdb`, `compile-xenos-shaders [--profile dx9|dx10|dx11]`, `upgrade-dbx`, `dbx-to-xml`, `xml-to-dbx`, `inspect-apt`, `unwrap-bink`, `upgrade-hpd`, `make-sector-offsets`, `register-level`, `dedup-fbrb`, `export-atlas-sprite`, `export-bf1943-menu`).
  * `app` — Docked `StudioApp` with **Asset Browser**, **Inspector & Preview**, **Conversion Queue**, and **Log & Problems** panels.
