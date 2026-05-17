//! MXL domain bring-up and libmxl lifecycle.
//!
//! [`MxlDomainManager`] owns the host's libmxl FFI surface and lazily
//! dlopens `libmxl.so` on first call. The boot probe runs in `main.rs`;
//! on success the per-essence capability bits (`"mxl-video"`,
//! `"mxl-audio"`, `"mxl-anc"`) join `HealthPayload.capabilities` and the
//! manager UI gates MXL options accordingly.
//!
//! M1 scope: probe + handle ownership + per-domain `MxlInstance` attach.
//! M2/M3 add per-flow grain readers/writers, ref-counted domain reuse,
//! and the `mxlIsTmpFs` perf-warning check.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow};
use mxl_rs::{MxlApi, MxlInstance, load_api};
use tracing::{debug, info, warn};

/// Set to `true` by [`MxlDomainManager::probe`] on success. Read by
/// `manager::client::edge_capabilities` to decide whether to advertise the
/// `mxl-video` / `mxl-audio` / `mxl-anc` capability bits on
/// `HealthPayload.capabilities`. Manager UI keys the MXL option dropdowns
/// off these bits.
pub static MXL_PROBE_OK: AtomicBool = AtomicBool::new(false);

/// Has the boot probe succeeded?
pub fn probe_succeeded() -> bool {
    MXL_PROBE_OK.load(Ordering::Relaxed)
}

/// Process-wide handle to the [`MxlDomainManager`] installed at boot. The
/// flow spawn arms (`engine::flow::spawn_single_input` / `start_output`)
/// look it up here when wiring an MXL input/output. Returns `None` when
/// the boot probe failed (libmxl.so missing) — the spawn arms then refuse
/// the flow with a `mxl_domain_unavailable` event.
static GLOBAL_DOMAIN_MANAGER: OnceLock<Arc<MxlDomainManager>> = OnceLock::new();

/// Install the [`MxlDomainManager`] returned by [`MxlDomainManager::probe`]
/// so the engine spawn arms can resolve it. Called once from `main.rs`
/// during boot. Subsequent calls are no-ops.
pub fn install_global(mgr: Arc<MxlDomainManager>) {
    let _ = GLOBAL_DOMAIN_MANAGER.set(mgr);
}

/// Look up the global manager (set by [`install_global`]). Returns `None`
/// when the probe failed or `install_global` hasn't been called yet.
pub fn global() -> Option<Arc<MxlDomainManager>> {
    GLOBAL_DOMAIN_MANAGER.get().cloned()
}

/// Default search paths for `libmxl.so` when `BILBYCAST_LIBMXL_SO` is unset.
/// Mirrors how Debian/Ubuntu hosts surface shared libraries after a CMake +
/// `cmake --install` of the upstream MXL build.
const DEFAULT_SO_PATHS: &[&str] = &[
    "/usr/local/lib/libmxl.so",
    "/usr/lib/x86_64-linux-gnu/libmxl.so",
    "/opt/bilbycast/lib/libmxl.so",
];

/// Owner of the dlopen'd libmxl handle for the lifetime of the process.
///
/// Created at boot via [`MxlDomainManager::probe`]. Each MXL flow asks the
/// manager for an [`MxlInstance`] attached to a named domain (a shared-memory
/// path under tmpfs, typically `/dev/shm/<domain-name>`).
pub struct MxlDomainManager {
    /// libmxl FFI handle, dlopen'd by [`Self::probe`]. Used by
    /// [`Self::attach_instance`] (M2/M3); kept alive for the lifetime of
    /// the process so `MxlInstance`s remain valid.
    #[allow(dead_code)]
    api: Arc<MxlApi>,
    so_path: PathBuf,
}

impl MxlDomainManager {
    /// Try to load `libmxl.so` from one of the well-known paths or the
    /// `BILBYCAST_LIBMXL_SO` env var. Returns `None` on miss so the boot
    /// path stays alive on hosts where MXL isn't installed — the manager
    /// UI hides MXL options when no `"mxl-*"` capability is advertised.
    pub fn probe() -> Option<Self> {
        let so_path = match resolve_so_path() {
            Some(p) => p,
            None => {
                debug!(
                    paths = ?DEFAULT_SO_PATHS,
                    "MXL probe: no libmxl.so found on default search paths or via \
                     BILBYCAST_LIBMXL_SO env var. mxl-* capabilities will not be advertised."
                );
                return None;
            }
        };
        match load_api(&so_path) {
            Ok(api) => {
                MXL_PROBE_OK.store(true, Ordering::Relaxed);
                info!(
                    so_path = %so_path.display(),
                    "MXL probe succeeded — libmxl loaded; advertising mxl-* capabilities"
                );
                Some(Self { api, so_path })
            }
            Err(e) => {
                warn!(
                    so_path = %so_path.display(),
                    error = %e,
                    "MXL probe found a candidate libmxl.so but dlopen failed"
                );
                None
            }
        }
    }

    /// Path libmxl was loaded from (used in logs + the manager-visible
    /// resource-budget block).
    pub fn so_path(&self) -> &PathBuf {
        &self.so_path
    }

    /// Open an [`MxlInstance`] attached to the given domain. The `domain`
    /// string is typically a tmpfs path (`/dev/shm/<name>`) shared between
    /// the bilbycast-edge processes that need to compose on-host. `options`
    /// is a JSON string passed to libmxl per its v1.0 schema.
    ///
    /// M1 scaffold — call sites land in M2 (audio + ANC spawn arms) and M3
    /// (video). The `dead_code` allow is intentional and removed when
    /// `engine::flow::spawn_single_input` / `start_output` gain the
    /// `InputConfig::Mxl*` / `OutputConfig::Mxl*` match arms.
    #[allow(dead_code)]
    pub fn attach_instance(&self, domain: &str, options: &str) -> Result<MxlInstance> {
        MxlInstance::new(Arc::clone(&self.api), domain, options)
            .map_err(|e| anyhow!("MXL attach to domain {domain:?} failed: {e}"))
    }
}

/// Performance preflight: is `path` on a tmpfs or ramfs?
///
/// The upstream C API exposes `mxlIsTmpFs` (see `lib/include/mxl/mxl.h`) but
/// it isn't surfaced through the v1.0.1 Rust safe wrapper. Re-implementing in
/// pure Rust via `statfs(2)` keeps the call site safe and avoids reaching
/// past the `mxl-rs` boundary into raw `mxl-sys` bindings. Matches the
/// upstream semantics: returns `true` for `TMPFS_MAGIC` (0x01021994) and
/// `RAMFS_MAGIC` (0x858458f6); falls back to `false` on errors so a
/// not-tmpfs path is degraded-but-functional rather than fatal.
///
/// M2 wires this into the flow startup path so the operator gets a Warning
/// event when an MXL domain is configured under (e.g.) ext4 — libmxl's
/// shared-memory perf model degrades sharply off tmpfs.
#[allow(dead_code)]
pub fn is_tmpfs(path: &std::path::Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::mem::MaybeUninit;

        const TMPFS_MAGIC: libc::__fsword_t = 0x0102_1994;
        const RAMFS_MAGIC: libc::__fsword_t = 0x8584_58f6_u32 as libc::__fsword_t;

        let c_path = match CString::new(path.as_os_str().as_encoded_bytes()) {
            Ok(p) => p,
            Err(_) => return false,
        };
        let mut buf: MaybeUninit<libc::statfs> = MaybeUninit::uninit();
        // SAFETY: c_path is a valid NUL-terminated C string; buf is a valid
        // mutable pointer to an uninitialised libc::statfs. On success, statfs
        // initialises *buf; on failure, *buf is left uninitialised but we
        // return early before any read.
        let rc = unsafe { libc::statfs(c_path.as_ptr(), buf.as_mut_ptr()) };
        if rc != 0 {
            return false;
        }
        // SAFETY: statfs returned 0, so *buf is initialised.
        let fs = unsafe { buf.assume_init() };
        fs.f_type == TMPFS_MAGIC || fs.f_type == RAMFS_MAGIC
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        false
    }
}

fn resolve_so_path() -> Option<PathBuf> {
    if let Ok(env_path) = std::env::var("BILBYCAST_LIBMXL_SO") {
        let p = PathBuf::from(env_path);
        if p.exists() {
            return Some(p);
        }
    }
    for default in DEFAULT_SO_PATHS {
        let p = PathBuf::from(default);
        if p.exists() {
            return Some(p);
        }
    }
    None
}
