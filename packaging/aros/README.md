# Zed for AROS

This branch builds Zed as a native AROS program for AArch64: PomAROS VM and
native AROS systems on that architecture. It uses the official Rust compiler
with the AROS target and standard library of
[pomaros/rust-aros](https://github.com/pomaros/rust-aros), and the AROS GPUI
backend of [pomaros/gpui-aros](https://github.com/pomaros/gpui-aros).

## Building

You need:

- an AROS Rust SDK installed by `toolchain/integrate.py` of rust-aros, over the
  `Developer` directory of an AROS build whose C library declares the POSIX
  interfaces this tree uses (posixc with `openpty`, `pthread_atfork`,
  `lutimes`, `getauxval`, the `termios` controls, the pthread types in
  `sys/types.h` and the byte-order conversions of `endian.h`);
- the crate store built by `crates/store.py` of the same rust-aros revision.

Then, from the repository root:

```sh
AROS_RUST_SDK=/path/to/sdk-prefix AROS_CRATE_STORE=/path/to/store \
  script/aros-cargo.py build -p zed --bin zed
```

`script/aros-cargo.py` checks the SDK, writes the AROS-only Cargo
configuration to `target/aros/toolchain.toml` (the `[patch]` entries of
[`aros-patches.toml`](aros-patches.toml), resolved in the store), pins the
patched crates in the AROS lockfile, [`Cargo.lock`](Cargo.lock) in this
directory, and runs the SDK's Cargo. The root `Cargo.lock` serves the other
platforms and is not touched.

The backend in `crates/gpui_aros` is a copy of a gpui-aros revision, recorded
in `crates/gpui_aros/UPSTREAM`. Change it in gpui-aros, then update the copy:

```sh
script/sync-gpui-aros /path/to/gpui-aros-checkout
```

The script refuses to overwrite local edits.

## Running

Zed needs a large stack and re-runs itself with a 16 MB one when started with
less. It writes its log to the log file, not to the console it was started
from. Paths are AROS paths (`Work:projects/zed`), and `file://` URLs carry them
as `file:///Work%3Aprojects/zed`.

## What AROS does not have yet

These parts of Zed are left out or report themselves unavailable:

- the terminal, until AROS has a pseudo-terminal device;
- language servers, unless `ZED_AROS_LSP_ADDR` names a TCP endpoint where a
  server runs on another machine;
- network services (sign-in, updates, extensions downloads), until sockets,
  timers and TLS are qualified together on AROS;
- extensions and wasm grammars (no wasm runtime), the agent, collaboration and
  calls;
- native file change notifications: the file watcher polls.

## Native GPU build

The AROS GPU integration uses native Intuition handles and the GLES backend
through egl.library. A hardware build selects `--features gpui_aros/native-gpu`
and provides `AROS_EGL_LIB_DIR` if the managed Rust SDK's C library directory
has no native libEGL.a entry stubs. The AROS-only patch inventory pins both
public PomAROS graphics forks. Other platforms keep their own dependencies.

The feature refuses software adapters instead of claiming hardware rendering.
The software build remains available for VM work. Native GPUI scene verification
and the full editor scenario are distinct; enabling the feature is not evidence
that the editor workflow has passed.
