//! RGA (Rockchip 2D accelerator)-hardware-accelerated DRM_PRIME→sysmem
//! transfer.
//!
//! FFmpeg's generic `av_hwframe_transfer_data` (`hwcontext_drm.c`) does
//! an unaccelerated CPU `mmap` + `DMA_BUF_IOCTL_SYNC` + `memcpy` on
//! every frame — measured on bilby-pir6s (RK3588) as the dominant
//! per-frame cost behind HDMI display-output stutter, with spikes up
//! to ~106 ms at 1080p. This module does the identical DRM_PRIME
//! dma-buf → sysmem NV12 copy on the Rockchip RGA 2D engine instead —
//! benchmarked on the same hardware at ~3 ms/frame (1080p), a ~30x
//! improvement, entirely on the CPU-blit code path: presentation stays
//! on the proven primary-plane path, only the transfer step changes.
//!
//! Every entry point returns `Result` so the caller falls back to the
//! existing CPU path on any failure — this is a pure performance
//! optimization, never a hard dependency. Only compiled with the
//! `rga-transfer` feature (ARM Rockchip only; needs `librga-dev` +
//! `/dev/rga` at build/run time).

use anyhow::{bail, Result};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

#[allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code,
    clippy::all
)]
mod ffi {
    // Types only (`rga_buffer_t`, `IM_STATUS`, `_Rga_SURF_FORMAT`) —
    // see build.rs for why the functions below are hand-declared
    // rather than bindgen-generated.
    include!(concat!(env!("OUT_DIR"), "/rga_bindings.rs"));

    // Exact signatures confirmed against the generated bindings this
    // build.rs itself produced from the real `librga-dev` 2.2.0
    // headers on bilby-pir6s (`nm -D librga.so.2.1.0` also confirms
    // all seven are exported, non-mangled, `T` symbols — in
    // particular `importbuffer_fd`/`importbuffer_virtualaddr` have
    // *other*, C++-mangled overloads in the same library taking plain
    // `int` args instead of `im_handle_param*`; the plain,
    // unmangled symbol is specifically the param-struct one).
    // `#[link(name = "rga")]` is what actually gets `-lrga` onto this
    // package's *binary* link command — `cargo:rustc-link-lib=rga`
    // from build.rs alone did not, on this toolchain.
    #[link(name = "rga")]
    unsafe extern "C" {
        pub fn importbuffer_fd(
            fd: std::os::raw::c_int,
            param: *mut im_handle_param,
        ) -> rga_buffer_handle_t;
        pub fn importbuffer_virtualaddr(
            va: *mut std::os::raw::c_void,
            param: *mut im_handle_param,
        ) -> rga_buffer_handle_t;
        pub fn releasebuffer_handle(handle: rga_buffer_handle_t) -> IM_STATUS;
        pub fn wrapbuffer_handle_t(
            handle: rga_buffer_handle_t,
            width: std::os::raw::c_int,
            height: std::os::raw::c_int,
            wstride: std::os::raw::c_int,
            hstride: std::os::raw::c_int,
            format: std::os::raw::c_int,
        ) -> rga_buffer_t;
        pub fn imcopy_t(
            src: rga_buffer_t,
            dst: rga_buffer_t,
            sync: std::os::raw::c_int,
        ) -> IM_STATUS;
        pub fn imStrError_t(status: IM_STATUS) -> *const std::os::raw::c_char;
    }
}

const FORMAT_NV12: i32 = ffi::_Rga_SURF_FORMAT_RK_FORMAT_YCbCr_420_SP as i32;

/// DMA-BUF kernel identity via `fstat` → `(st_dev, st_ino)` — the same
/// pattern `KmsDisplay`'s PRIME framebuffer cache uses. Robust to the
/// fd *number* changing across calls (RKMPP/FFmpeg may hand back a
/// `dup()` of the same underlying buffer under a different fd each
/// time) because it identifies the underlying kernel object, not the
/// fd value.
fn dmabuf_identity(fd: i32) -> Option<(u64, u64)> {
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) == 0 {
            Some((st.st_dev as u64, st.st_ino as u64))
        } else {
            None
        }
    }
}

const SRC_HANDLE_CACHE_CAPACITY: usize = 32;

/// Imported-source-handle cache, keyed by [`dmabuf_identity`].
///
/// Rockchip's own FAQ (`Rockchip_FAQ_RGA_EN.md`, on registering memory
/// via `importbuffer_xx` before use) warns that repeatedly
/// importing/releasing the *same* underlying buffer every frame "has
/// poor performance... recommended to optimize the buffer process
/// overall" — exactly the RKMPP situation: the decoder recycles a
/// small fixed pool of DMA-BUFs (≤16), so the overwhelming majority of
/// `importbuffer_fd` calls are needlessly re-registering a buffer
/// already known to the driver. Confirmed on hardware: import+release
/// on every frame measurably regressed the display's dropped-frame
/// rate versus the (slower per-byte, but far less syscall-heavy) CPU
/// path it was meant to replace.
static SRC_HANDLE_CACHE: Mutex<Option<SrcHandleCache>> = Mutex::new(None);

struct SrcHandleCache {
    map: HashMap<(u64, u64), ffi::rga_buffer_handle_t>,
    order: VecDeque<(u64, u64)>,
}

fn status_to_result(status: ffi::IM_STATUS, what: &str) -> Result<()> {
    if status == ffi::IM_STATUS_IM_STATUS_SUCCESS {
        return Ok(());
    }
    // SAFETY: `imStrError_t` returns a pointer to a static/constant
    // string table entry for a known status code — never null for any
    // value this crate passes, but guarded anyway.
    let msg = unsafe {
        let cstr = ffi::imStrError_t(status);
        if cstr.is_null() {
            "unknown".to_string()
        } else {
            std::ffi::CStr::from_ptr(cstr).to_string_lossy().into_owned()
        }
    };
    bail!("rga_transfer: {what} failed: {msg} (status={status})");
}

/// Hardware-copy a single-object, semi-planar NV12 DRM_PRIME dma-buf
/// into a freshly-allocated sysmem buffer, returning `(y, y_stride,
/// uv, uv_stride)` ready to drop straight into a sysmem video frame —
/// bypassing FFmpeg's generic CPU transfer entirely.
///
/// `fd` is the plane-0 dma-buf fd (borrowed — not closed or dup'd
/// here, and not retained past this call); `pitch` is its row stride
/// in bytes; `uv_offset_rows` is the vertical row offset at which the
/// UV plane starts (`uv_byte_offset / pitch`) — RKMPP pads the coded
/// height to a macroblock boundary (e.g. a 1080-row frame is often
/// laid out with `uv_offset_rows == 1088`), so this is *not* always
/// equal to `height`. Both figures come straight off the
/// `AVDRMFrameDescriptor` FFmpeg already handed us (plane pitch /
/// offset), so no guessing about buffer layout is involved — RGA is
/// simply told the same layout the decoder actually used.
pub fn nv12_dmabuf_to_sysmem(
    fd: i32,
    width: u32,
    height: u32,
    pitch: u32,
    uv_offset_rows: u32,
) -> Result<(Vec<u8>, usize, Vec<u8>, usize)> {
    if width == 0 || height == 0 || pitch == 0 || uv_offset_rows == 0 {
        bail!(
            "rga_transfer: invalid frame geometry ({width}x{height}, pitch={pitch}, \
             uv_offset_rows={uv_offset_rows})"
        );
    }
    let pitch_us = pitch as usize;
    let y_rows_out = height as usize;
    let uv_rows_out = height.div_ceil(2) as usize;
    let uv_byte_offset = pitch_us * uv_offset_rows as usize;
    // RGA's fd-wrapped buffer model is "one contiguous NV12 image": Y
    // for `hstride` rows, then UV immediately after — allocate that
    // full footprint (using `hstride`, not the display `height`, as
    // the height component) plus one page of headroom and page-align.
    // Sized to match what `import_param` below tells the driver to
    // expect, not guessed independently of it.
    const PAGE_SIZE: usize = 4096;
    let logical_size = pitch_us * uv_offset_rows as usize + pitch_us * uv_rows_out;
    let total_size = (logical_size + PAGE_SIZE).div_ceil(PAGE_SIZE) * PAGE_SIZE;
    let mut dst = vec![0u8; total_size];

    // Pre-register both buffers with the RGA driver via `importbuffer_*`
    // before building the `rga_buffer_t`s, rather than the simpler
    // one-shot `wrapbuffer_fd_t`/`wrapbuffer_virtualaddr_t` calls this
    // function used originally. Per Rockchip's own FAQ
    // (Rockchip_FAQ_RGA_EN.md, Q4.1/Q4.5): `wrapbuffer_*` maps the
    // buffer on-demand inside the driver at job-commit time with no
    // pre-flight validation, which on this hardware produced
    // "Only get buffer N byte from vma, but current image required M
    // byte" / "failed to get pte" job-commit failures despite the
    // destination being sized (and even page-rounded) to match the
    // logical NV12 byte count — the driver's actual internal padding
    // requirement didn't match that math. `importbuffer_*` validates
    // up front against the same `{width, height, format}` the
    // `rga_buffer_t` will later claim, so a too-small buffer fails
    // here with a clear return code instead of deep inside job commit.
    // `height` in `import_param` is deliberately `uv_offset_rows`
    // (hstride), not the display `height` — matching the padded
    // layout the buffer is actually allocated at.
    let mut import_param = ffi::im_handle_param {
        width: pitch,
        height: uv_offset_rows,
        format: FORMAT_NV12 as u32,
    };

    // Source side: look up the cache first (see `SRC_HANDLE_CACHE`'s
    // doc comment) — RKMPP recycles a small fixed pool of DMA-BUFs, so
    // this is a cache hit on the overwhelming majority of frames once
    // warmed up. Only import (and cache) on a miss.
    //
    // SAFETY: `importbuffer_fd` reads `fd` and `*import_param` to
    // register the buffer with the driver; it does not take ownership
    // of the fd (RKMPP's own `keepalive` on the caller side still owns
    // it) or retain the `import_param` pointer past the call.
    let cache_key = dmabuf_identity(fd);
    let src_handle = {
        let mut guard = SRC_HANDLE_CACHE.lock().unwrap();
        let cache = guard.get_or_insert_with(|| SrcHandleCache {
            map: HashMap::new(),
            order: VecDeque::new(),
        });
        if let Some(key) = cache_key
            && let Some(&handle) = cache.map.get(&key)
        {
            handle
        } else {
            let handle = unsafe { ffi::importbuffer_fd(fd, &mut import_param) };
            if handle == 0 {
                bail!("rga_transfer: importbuffer_fd returned a null handle");
            }
            if let Some(key) = cache_key {
                if cache.map.len() >= SRC_HANDLE_CACHE_CAPACITY
                    && let Some(oldest) = cache.order.pop_front()
                    && let Some(evicted) = cache.map.remove(&oldest)
                {
                    unsafe { ffi::releasebuffer_handle(evicted) };
                }
                cache.map.insert(key, handle);
                cache.order.push_back(key);
            }
            handle
        }
    };

    // SAFETY: `importbuffer_virtualaddr` just registers `dst`'s
    // pointer/geometry — `dst` is alive, owned by this stack frame,
    // and not moved for the remainder of the function.
    let dst_handle =
        unsafe { ffi::importbuffer_virtualaddr(dst.as_mut_ptr() as *mut std::ffi::c_void, &mut import_param) };
    if dst_handle == 0 {
        bail!("rga_transfer: importbuffer_virtualaddr returned a null handle");
    }

    let status = unsafe {
        let src = ffi::wrapbuffer_handle_t(
            src_handle,
            width as i32,
            height as i32,
            pitch as i32,
            uv_offset_rows as i32,
            FORMAT_NV12,
        );
        let dst_buf = ffi::wrapbuffer_handle_t(
            dst_handle,
            width as i32,
            height as i32,
            pitch as i32,
            uv_offset_rows as i32,
            FORMAT_NV12,
        );
        ffi::imcopy_t(src, dst_buf, 1)
    };

    // `src_handle` is owned by `SRC_HANDLE_CACHE` now (cache hit or
    // freshly inserted above) — only release `dst_handle`, which is
    // never cached (a fresh `dst` allocation every call).
    //
    // SAFETY: `dst_handle` was successfully imported above; releasing
    // it after the (synchronous) `imcopy_t` call has returned is
    // always valid regardless of its outcome.
    unsafe {
        ffi::releasebuffer_handle(dst_handle);
    }
    status_to_result(status, "imcopy_t")?;

    let uv = dst[uv_byte_offset..uv_byte_offset + pitch_us * uv_rows_out].to_vec();
    dst.truncate(pitch_us * y_rows_out);
    Ok((dst, pitch_us, uv, pitch_us))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_geometry() {
        assert!(nv12_dmabuf_to_sysmem(-1, 0, 1080, 1920, 1088).is_err());
        assert!(nv12_dmabuf_to_sysmem(-1, 1920, 0, 1920, 1088).is_err());
        assert!(nv12_dmabuf_to_sysmem(-1, 1920, 1080, 0, 1088).is_err());
        assert!(nv12_dmabuf_to_sysmem(-1, 1920, 1080, 1920, 0).is_err());
    }
}
