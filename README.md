> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

# Zed for AROS (AArch64)

This is the PomAROS development port of [Zed](https://zed.dev) to native
AArch64 AROS. The active repository is [pomaros/zed](https://github.com/pomaros/zed)
and its default development branch is **`aros`**.

## Verified behavior

The consolidated build has passed a functional scenario in the PomAROS VM:
open a file, select and edit its contents, save, read back the actual bytes from
the guest filesystem, capture the editor window, and shut down the VM cleanly.
The verified application code is commit `d68b5b60f298453267d6b680914044a8eeeb1f62`.

The same application binary has also passed a focused scenario on a physical
M1 running native AROS: open a file, edit and save it, read back the exact bytes,
find text, and close the window with exit status 0. The startup software-rendering
warning was dismissed with **Skip**; the test project stayed in restricted mode.

Both scenarios use the AROS GPUI backend with **CPU rendering (tiny-skia)**.
They do not qualify GPU acceleration or the editor's complete feature set.
Treat this as a development port rather than a fully qualified Zed release.

## Experimental native GPU rendering

The optional `aros-native-gpu` build renders through WGPU/GLES, native Intuition
window handles and `egl.library`. A physical M1 capture confirms the editor's
GPU layer; window close returns status 0. Window resizing is visually observed.
This is separate from the CPU functional scenario above.

The native presentation driver must respect Intuition's logical window canvases
and software cursor. With that driver, a focused M1 check verifies GPU-rendered
content in the desktop bitmap, the visible pointer, a completed window move and
normal close. Application rendering uses the GPU; presentation into the native
window bitmap uses a CPU copy. Accelerated desktop composition is separate work.
The checked system profile uses outline dragging: content moves on release.
Opaque live dragging is outside this qualification.
Keep the CPU build for VM work and as a fallback reference.

Build with `--features aros-native-gpu` and set `AROS_EGL_LIB_DIR` to the graphics
SDK directory containing `libEGL.a`. The build isolates this entry archive from
the managed C runtime. See [the native GPU build notes](packaging/aros/README.md#native-gpu-build).

## Building for AROS

Use the managed native/VM Rust SDK and its matching AROS C SDK from
[pomaros/rust-aros](https://github.com/pomaros/rust-aros), plus the prepared
native crate store. The qualified cohort uses the unmodified official
`nightly-2026-08-04` compiler and SDK generation
`b4ec3990690f25e8809c03a6d7063ea36b84699e90e2f036b88f85718f8cfed2`.
The AROS standard library and platform adapters are patched; the compiler is not.
The Hosted/macOS Rust SDK has a different ABI and must not be substituted.

```sh
git clone --branch aros https://github.com/pomaros/zed.git
cd zed
export AROS_RUST_SDK=/absolute/path/to/managed-aros-rust-sdk
export AROS_CRATE_STORE=/absolute/path/to/native-crate-store
export CARGO_TARGET_DIR=/absolute/path/to/zed-build
python3 script/aros-cargo.py build --locked --offline --profile release-fast -p zed --bin zed
```

[script/aros-cargo.py](script/aros-cargo.py) verifies the SDK inputs, selects its
precompiled standard library and applies the AROS crate configuration. It uses
[packaging/aros/Cargo.lock](packaging/aros/Cargo.lock); keep the root lockfile
for the other platforms. Prepare the SDK and crate store before the offline
build. The executable is written under
`$CARGO_TARGET_DIR/aarch64-unknown-aros/release-fast/zed`; retain its unstripped
copy and matching source/SDK identities when preparing a smaller runtime.

## Upstream Zed

[![Zed](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/zed-industries/zed/main/assets/badge/v0.json)](https://zed.dev)
[![CI](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml/badge.svg)](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml)

Welcome to Zed, a high-performance, multiplayer code editor from the creators of [Atom](https://github.com/atom/atom) and [Tree-sitter](https://github.com/tree-sitter/tree-sitter).

---

### Installation

On macOS, Linux, and Windows you can [download Zed directly](https://zed.dev/download) or install Zed via your local package manager ([macOS](https://zed.dev/docs/installation#macos)/[Linux](https://zed.dev/docs/linux#installing-via-a-package-manager)/[Windows](https://zed.dev/docs/windows#package-managers)).

Other platforms are not yet available:

- Web ([tracking discussion](https://github.com/zed-industries/zed/discussions/26195))

### Developing Zed

- [Building Zed for macOS](./docs/src/development/macos.md)
- [Building Zed for Linux](./docs/src/development/linux.md)
- [Building Zed for Windows](./docs/src/development/windows.md)

### Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for ways you can contribute to Zed.

Also... we're hiring! Check out our [jobs](https://zed.dev/jobs) page for open roles.

### Licensing

Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked.

License information for third party dependencies must be correctly provided for CI to pass.

We use [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to automatically comply with open source licenses. If CI is failing, check the following:

- Is it showing a `no license specified` error for a crate you've created? If so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so, first determine what license the project has and whether this system is sufficient to comply with this license's requirements. If you're unsure, ask a lawyer. Once you've verified that this system is acceptable add the license's SPDX identifier to the `accepted` array in `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a clarification field at the end of `script/licenses/zed-licenses.toml`, as specified in the [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration).

## Sponsorship

Zed is developed by **Zed Industries, Inc.**, a for-profit company.

If you’d like to financially support the project, you can do so via GitHub Sponsors.
Sponsorships go directly to Zed Industries and are used as general company revenue.
There are no perks or entitlements associated with sponsorship.
