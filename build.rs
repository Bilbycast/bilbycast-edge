//! Build script for bilbycast-edge.
//!
//! Currently only does one thing: when the `rga-transfer` feature is
//! enabled, generates Rust FFI bindings for the small slice of the
//! Rockchip RGA 2D-accelerator API (`librga`) used by
//! `src/display/rga_transfer.rs` — the hardware-accelerated
//! DRM_PRIME→sysmem transfer that replaces FFmpeg's generic CPU
//! mmap+memcpy path on RK3588. Bindgen (not hand-written FFI) is used
//! deliberately: `rga_buffer_t` has a union and several nested structs,
//! and getting its layout wrong from a manual `#[repr(C)]` transcription
//! risks silent memory corruption rather than a loud build failure.

fn main() {
    // Declared BEFORE the early-return below, and unconditionally: as
    // soon as a build script emits any `rerun-if-changed`/`-env-changed`
    // directive, Cargo stops using its implicit "rerun every build"
    // default and reruns ONLY on the declared triggers — so toggling
    // this feature off then back on without touching any other input
    // would otherwise replay a stale cached (feature-off, i.e. no
    // `cargo:rustc-link-lib=rga`) run from before this line existed.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_RGA_TRANSFER");
    if std::env::var("CARGO_FEATURE_RGA_TRANSFER").is_err() {
        return;
    }

    println!("cargo:rerun-if-changed=src/display/rga_wrapper.h");

    let lib = pkg_config::Config::new()
        .probe("librga")
        .expect(
            "rga-transfer: pkg-config could not find `librga` — install librga-dev \
             (apt: librga-dev librga2) on this Rockchip host",
        );

    let bindings = bindgen::Builder::default()
        .header("src/display/rga_wrapper.h")
        .clang_args(
            lib.include_paths
                .iter()
                .map(|p| format!("-I{}", p.display())),
        )
        // Types only — deliberately no `allowlist_function` here.
        // `rga_transfer.rs` hand-declares the small number of
        // functions it calls in its own `#[link(name = "rga")]`
        // extern block instead of using bindgen-generated ones: on
        // this toolchain, `cargo:rustc-link-lib=rga` emitted below
        // reliably reached this package's *library* metadata but not
        // its *binary* target's final link (`-lrga` never appeared on
        // the linker command line despite the directive being
        // correctly emitted every build — confirmed via `-vv`), and
        // an explicit `#[link(...)]` on the extern block is a second,
        // source-level path that doesn't depend on that propagation.
        // `rga_buffer_t`'s layout (a union plus several nested
        // structs) is exactly the part worth generating rather than
        // hand-transcribing, so bindgen still owns that.
        .allowlist_type("rga_buffer_t")
        .allowlist_type("IM_STATUS")
        // `im_handle_param_t` — the {width, height, format} struct
        // `importbuffer_fd`/`importbuffer_virtualaddr` take, part of
        // the pre-register-then-wrap pattern this module uses (see
        // `rga_transfer.rs` doc comment for why, over the simpler
        // `wrapbuffer_*` one-shot calls).
        .allowlist_type("im_handle_param")
        // RK_FORMAT_* are members of the `_Rga_SURF_FORMAT` C enum, not
        // preprocessor defines — allowlist the enum's tag name so
        // bindgen emits its variants, not `allowlist_var` (which only
        // matches #define'd/global values).
        .allowlist_type("_Rga_SURF_FORMAT")
        .derive_default(true)
        .generate()
        .expect("rga-transfer: bindgen failed to generate librga bindings");

    let out_path = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("rga_bindings.rs"))
        .expect("rga-transfer: failed to write generated librga bindings");
}
