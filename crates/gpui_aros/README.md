# gpui_aros

A GPUI platform backend for native AROS (AArch64): Intuition windows and
input, a tiny-skia CPU rasterizer blitted through CyberGraphics, cosmic-text
shaping with bundled fonts, `clipboard.device`, GadTools menus, ASL file
requesters and Intuition pointers. Any GPUI application runs on it; Ferail is
the first.

## Identity and layout

The crate builds against the `gpui-pre` crates.io snapshots of Zed
(`gpui-pre`, `gpui-pre-collections`, `gpui-pre-wgpu` for the text system), the
identity its consumers use. Its sibling `../gpui_platform_aros` is the
published `gpui-pre-platform` source plus one `target_os = "aros"` arm
selecting `ArosPlatform`; a consumer swaps it in with

```toml
[patch.crates-io]
gpui-pre-platform = { git = "https://github.com/pomaros/gpui-aros", rev = "..." }
```

and keeps `gpui`, `gpui_platform` and gpui-kit on the same snapshot version.
The Zed application, whose GPUI follows Zed rather than a snapshot, carries
these files through its own platform adaptation layer.

| File | What |
|---|---|
| `src/gpui_aros.rs` | crate root; the AROS shell modules are `cfg(target_os = "aros")` |
| `src/platform.rs` | `ArosPlatform`: run loop, `Platform` methods |
| `src/window.rs` | `ArosWindow`: Intuition window, callbacks, `draw`, event pump |
| `src/renderer.rs` | `CpuRenderer`: tiny-skia rasterizer, one arm per `PrimitiveBatch` |
| `src/damage.rs` | frame fingerprints and the dirty rectangle |
| `src/stats.rs`, `src/text_stats.rs` | frame statistics through `log` every two seconds: GPUI draw, raster and blit time, damaged pixels, time per primitive kind, scene size, text-system calls |
| `gpa_profile_*` in the glue | sampling profiler of the main task: `GPUI_AROS_PROFILE=<file>` samples its program counter every millisecond from its exec register frame and writes the samples at exit; `Ferail/scripts/aros-profile-symbolize.py` names them against the unstripped binary |
| `src/atlas.rs` | `CpuAtlas`: etagere shelf packing over `Vec<u8>` textures |
| `src/dispatcher.rs` | std-thread background pool and the main-thread queue |
| `src/requesters.rs` | owned native requester threads, shutdown joins and byte-preserving DOS paths |
| `src/display.rs` | one display from `gpa_screen_size` |
| `src/input.rs` | RAWKEY to `Keystroke`, qualifiers to `Modifiers`, mouse |
| `src/menus.rs` | GPUI menus to GadTools `NewMenu` strips, picks to actions |
| `src/text.rs` | `CosmicTextSystem` with the bundled IBM Plex Sans and Lilex |
| `src/glue.rs` | `extern "C"` declarations for the C glue |
| `src/conformance.rs` | host-runnable renderer and input conformance tests |
| `c/gpui_aros_glue.c` | flat C over Intuition, CyberGraphics, keymap, GadTools, ASL and `clipboard.device` |
| `build.rs` | compiles the glue when targeting AROS with `AROS_SDK` set |

`atlas`, `renderer`, `damage` and `input` are pure Rust and compile on any
host: `cargo test -p gpui_aros` runs the conformance suite described in
[PORTING.md](PORTING.md).

## Platform contract

- **Rendering.** The drawable is the window interior at one device pixel per
  point (`scale_factor` is 1.0). Each frame is diffed against the previous
  one (common prefix and suffix dropped, then a walk that resynchronises
  after every change, so two changes far apart damage two small rectangles'
  union); only the damaged rectangle is rasterized and blitted with
  `WritePixelArray` in `RECTFMT_RGBA`, tiny-skia's byte order. Clips are
  rectangles cut into the geometry; a pixel mask exists only for a rounded,
  stroked, path or scaled primitive that straddles its clip. Tinted glyphs
  are cached per tile and color. Subpixel text is off. There is no GPU
  path. At 1400x900, a hovered row costs about 1 ms of raster and the whole
  window 4 to 10 ms; GPUI's own layout and paint of a file list costs more
  than that.
- **Run loop.** One frame per 33 ms at most: drain the main-thread queue,
  poll every window's IDCMP port, request frames, then `Wait()` on the union
  of the window signal masks and the dispatcher's wake signal. When the
  loop ends, the quit callbacks run, the dispatcher joins its workers, and
  the wake signal and the loop's timer go back to Exec, so the program
  returns with no signal allocated.
- **Input.** RAWKEY codes go through `keymap.library` for the printable
  character and the unmodified key name; the Amiga keys map to GPUI's
  `platform` modifier, so `secondary-` bindings are Ctrl. Right-click reaches
  the application only while it asks for it (`gpa_set_rmb_trap`), otherwise
  Intuition's menu strip owns the button.
- **Menus, requesters, pointers, clipboard.** `set_menus` builds a GadTools
  strip; `prompt_for_paths` and `prompt_for_new_path` open an ASL requester
  on dedicated, owned requester threads; `set_cursor_style` installs an Intuition
  pointer object; clipboard text is IFF FTXT in ISO-8859-1 on unit 0. Window
  titles are folded to Latin-1 for the Intuition title font.
- **Unavailable on AROS, reported as such.** `prevent_idle_sleep` returns an
  error; the system sleep and wake callbacks are kept and never fire; window
  visibility is `Visible` while open and `Hidden` once closed; `open_url`,
  `restart`, `activate` and the other desktop-integration methods are no-ops.

## Building

The C glue is compiled by `build.rs` only when `CARGO_CFG_TARGET_OS` is
`aros` and `AROS_SDK` names a native SDK `Developer` directory (with
`include/proto/*.h`). The compiler, archiver and flags come from the cc
crate's target-suffixed environment (`CC_aarch64_unknown_aros`,
`AR_aarch64_unknown_aros`, `CFLAGS_aarch64_unknown_aros`), which the consumer's
AROS cargo configuration sets: an ELF triple, `-fno-pic`, `-mcmodel=large`,
no reserved x18 (native AROS uses it as an ordinary register). Without
`AROS_SDK` a plain `cargo check` still passes and the glue is expected at the
final link.

Ferail's `scripts/aros-cargo.sh` is the reference invocation: the official
pinned nightly with the AROS std overlay, the native target JSON and
`-Zbuild-std`, plus the patched crate store. `../gpui_aros_smoke` is the
minimal consumer: one window, every renderer primitive, keyboard echo.

## Known limits

- Pattern fills (slash, checkerboard), per-edge border widths other than the
  tree connectors, sprite rotation and `Surfaces` (video) are not drawn.
- ASL requesters keep painting and input running. Application shutdown waits
  for open requesters to complete or be cancelled, then joins their threads
  before unloading native code. Each request owns its `asl.library` reference.
  DOS paths retain their original bytes, including UTF-8 and legacy names;
  only display titles use the UI charset. Embedded NUL and truncated results
  are reported as errors.
- The dispatcher's worker count and priorities were tuned on a hosted,
  single-CPU AROS; native SMP qualification is open.
