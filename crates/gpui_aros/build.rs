//! Build script for gpui_aros.
//!
//! The C glue (`c/gpui_aros_glue.c`) wraps AROS Intuition/CyberGraphics. It
//! is compiled only when targeting AROS *and* an AROS SDK is named by
//! `AROS_SDK` (the `Developer` directory of a native AROS build, holding
//! `include/proto/*.h` and `lib/*.a`). Without it the cc invocation is
//! skipped so that a plain `cargo check` still succeeds; the runnable AROS
//! command then links the glue object itself at collect-aros time.
//!
//! The C compiler, archiver and flags come from the cc crate's ordinary
//! target-suffixed environment (`CC_aarch64_unknown_aros`,
//! `AR_aarch64_unknown_aros`, `CFLAGS_aarch64_unknown_aros`), which the
//! consumer's AROS cargo config sets. Native AROS treats x18 as an ordinary
//! register; nothing here reserves it.
use std::env;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=c/gpui_aros_glue.c");
    println!("cargo:rerun-if-env-changed=AROS_SDK");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "aros" {
        return;
    }

    let Ok(sdk) = env::var("AROS_SDK") else {
        println!("cargo:warning=gpui_aros glue deferred to link time (AROS_SDK not set)");
        return;
    };
    let include_root = format!("{sdk}/include");
    let probes = [
        "proto/exec.h",
        "proto/intuition.h",
        "proto/cybergraphics.h",
        "proto/keymap.h",
        "proto/gadtools.h",
        "proto/asl.h",
        "exec/types.h",
        "devices/inputevent.h",
        "intuition/intuition.h",
        "cybergraphx/cybergraphics.h",
    ];
    if let Some(missing) = probes
        .iter()
        .find(|p| !Path::new(&include_root).join(p).exists())
    {
        println!(
            "cargo:warning=gpui_aros glue deferred to link time (SDK header {missing} absent under {include_root})"
        );
        return;
    }

    if env::var_os("CARGO_FEATURE_NATIVE_GPU").is_some() {
        // EGL entry stubs call the native egl.library. A separate directory
        // permits an independently versioned graphics SDK without mutating the
        // managed Rust SDK or changing its CRT/platform adapter provenance.
        println!("cargo:rerun-if-env-changed=AROS_EGL_LIB_DIR");
        let egl_lib = env::var("AROS_EGL_LIB_DIR")
            .unwrap_or_else(|_| format!("{sdk}/lib"));
        assert!(Path::new(&egl_lib).join("libEGL.a").is_file(),
            "native-gpu requires libEGL.a; select its SDK with AROS_EGL_LIB_DIR");
        // Expose only the graphics entry archive to dependents, not another
        // SDK's full library directory (which could alter CRT resolution).
        let selected = Path::new(&egl_lib).join("libEGL.a");
        println!("cargo:rerun-if-changed={}", selected.display());
        let isolated = Path::new(&env::var("OUT_DIR").unwrap()).join("native-egl");
        std::fs::create_dir_all(&isolated).expect("create EGL stub directory");
        std::fs::copy(selected, isolated.join("libEGL.a")).expect("copy EGL entry stubs");
        println!("cargo:rustc-link-search=native={}", isolated.display());
        println!("cargo:rustc-link-lib=static=EGL");
    }

    // -Wno-pointer-sign matches the AROS C convention (STRPTR is unsigned
    // char*; literals are plain char*). AROS commands are relocatable
    // objects: no PIC.
    cc::Build::new()
        .file("c/gpui_aros_glue.c")
        .pic(false)
        .flag("-Wno-pointer-sign")
        .include(&include_root)
        .include(format!("{include_root}/aros/posixc"))
        .include(format!("{include_root}/aros/stdc"))
        .compile("gpui_aros_glue");
}
