//! SkyLight SPI bridge — Rust port of Swift's `SkyLightEventPost`.
//!
//! Two-layer story (matches Swift reference exactly):
//!
//! 1. **Post path** — `SLEventPostToPid` goes through `SLEventPostToPSN` →
//!    `CGSTickleActivityMonitor` → `SLSUpdateSystemActivityWithLocation` →
//!    `IOHIDPostEvent`. The public `CGEventPostToPid` skips the activity-monitor
//!    tickle so Chromium/Catalyst targets don't accept those events as live input.
//!
//! 2. **Authentication** (keyboard only) — on macOS 14+, WindowServer gates
//!    synthetic keyboard events on Chromium-like targets on an attached
//!    `SLSEventAuthenticationMessage`. We build one via the ObjC factory and
//!    attach it with `SLEventSetAuthenticationMessage` before posting.
//!
//! All symbols are resolved once at first use via `dlopen` + `dlsym`.
//! If anything fails to resolve the functions return `false` and callers
//! fall back to the public `CGEvent::post_to_pid`.

use libc::pid_t;
use std::ffi::{c_void, CStr};
use std::os::raw::{c_char, c_int, c_uint};
use std::sync::OnceLock;

// ── Function-pointer typedefs ──────────────────────────────────────────────

/// `void SLEventPostToPid(pid_t, CGEventRef)`
type PostToPidFn = unsafe extern "C" fn(pid_t, *mut c_void);

/// `void SLEventSetAuthenticationMessage(CGEventRef, id)`
type SetAuthMsgFn = unsafe extern "C" fn(*mut c_void, *mut c_void);

/// `void CGEventSetWindowLocation(CGEventRef, double x, double y)`
///
/// NOTE: CGPoint on 64-bit ARM/x86 is two f64 values packed consecutively.
/// We pass them as two separate f64 arguments which has identical ABI.
type SetWindowLocFn = unsafe extern "C" fn(*mut c_void, f64, f64);

/// `void SLEventSetIntegerValueField(CGEventRef, uint32_t field, int64_t value)`
type SetIntFieldFn = unsafe extern "C" fn(*mut c_void, u32, i64);

/// `uint32_t CGSMainConnectionID(void)`
type ConnectionIDFn = unsafe extern "C" fn() -> u32;

// ── NSMenu shortcut activation SPIs ──────────────────────────────────────────

/// `OSStatus SLPSSetFrontProcessWithOptions(const void *psn, uint32_t windowID, uint32_t options)`
type SetFrontProcessFn = unsafe extern "C" fn(*const c_void, u32, u32) -> i32;

/// `OSStatus SLSGetWindowOwner(uint32_t cid, uint32_t wid, uint32_t *out_cid)`
type GetWindowOwnerFn = unsafe extern "C" fn(u32, u32, *mut u32) -> i32;

/// `OSStatus SLSGetConnectionPSN(uint32_t cid, void *psn)`
type GetConnectionPSNFn = unsafe extern "C" fn(u32, *mut c_void) -> i32;

// ── Focus-without-raise SPIs ──────────────────────────────────────────────────

/// `OSStatus SLPSPostEventRecordTo(const void *psn, const uint8_t *bytes)`
/// Posts a 248-byte synthetic event record into the target process's Carbon
/// event queue. Build the buffer with bytes[0x04]=0xf8, bytes[0x08]=0x0d,
/// target window id at bytes 0x3c–0x3f (little-endian), focus/defocus marker
/// at bytes[0x8a] (0x01 = focus, 0x02 = defocus), all other bytes zero.
type PostEventRecordToFn = unsafe extern "C" fn(*const c_void, *const u8) -> i32;

/// `OSStatus _SLPSGetFrontProcess(void *psn)`
/// Writes the current frontmost process's 8-byte PSN into `psn`.
type GetFrontProcessFn = unsafe extern "C" fn(*mut c_void) -> i32;

/// `OSStatus GetProcessForPID(pid_t, void *psn)`
/// Deprecated but still resolves. Writes the target pid's 8-byte PSN.
type GetProcessForPIDFn = unsafe extern "C" fn(pid_t, *mut c_void) -> i32;

/// Factory: `+[SLSEventAuthenticationMessage messageWithEventRecord:pid:version:]`
/// ObjC send: `(id self, SEL _cmd, void* record, int32 pid, uint32 version) -> id`
type FactoryMsgSendFn = unsafe extern "C" fn(
    *mut c_void, // Class (receiver)
    *mut c_void, // SEL
    *mut c_void, // SLSEventRecord*
    c_int,       // pid
    c_uint,      // version
) -> *mut c_void;

// ── Symbol resolution ──────────────────────────────────────────────────────
//
// Every resolution failure is checked and traced against the version
// expectation in `crate::version_matrix::SKYLIGHT_SYMBOLS` — nothing in
// this module falls back silently (gap G9, issue #8).

/// Load SkyLight once so all dlsym lookups via RTLD_DEFAULT find it.
///
/// Returns `true` when the framework handle is usable. A `dlopen`
/// failure is logged at error level with the `dlerror` text instead of
/// being silently ignored.
fn ensure_skylight_loaded() -> bool {
    static LOADED: OnceLock<bool> = OnceLock::new();
    *LOADED.get_or_init(|| {
        let path = b"/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight\0";
        let handle = unsafe {
            libc::dlopen(
                path.as_ptr() as *const c_char,
                libc::RTLD_LAZY | libc::RTLD_GLOBAL,
            )
        };
        if handle.is_null() {
            let detail = unsafe {
                let err = libc::dlerror();
                if err.is_null() {
                    "unknown dlopen error".to_owned()
                } else {
                    CStr::from_ptr(err).to_string_lossy().into_owned()
                }
            };
            tracing::error!(
                "dlopen(SkyLight.framework) failed: {detail} — \
                 every SkyLight SPI will report unresolved"
            );
            return false;
        }
        true
    })
}

/// Strip the trailing NUL from a `b"name\0"` dlsym literal for logging
/// and registry lookups.
fn symbol_label(name: &[u8]) -> &str {
    let s = std::str::from_utf8(name).unwrap_or("<invalid>");
    s.trim_end_matches('\0')
}

/// Look up a symbol by name via RTLD_DEFAULT (after loading SkyLight).
/// Returns `None` when the symbol doesn't resolve — and says so: misses
/// are logged against the registry's version expectation, so an OS
/// update breaking a private API is a loud signal, not silent drift.
fn find_sym(name: &[u8]) -> Option<*mut c_void> {
    let label = symbol_label(name);
    if !ensure_skylight_loaded() {
        tracing::warn!("private symbol {label} unresolved: SkyLight.framework failed to load");
        return None;
    }
    let ptr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr() as *const c_char) };
    if !ptr.is_null() {
        return Some(ptr);
    }
    let version = crate::version_matrix::macos_version();
    match crate::version_matrix::spec_for(label) {
        Some(spec) if spec.expected_on(version) => {
            tracing::warn!(
                "private symbol {label} ({feature}) unresolved on macOS {version} — \
                 expected on this OS; caller falls back to the public API path",
                feature = spec.feature,
            );
        }
        Some(spec) => {
            let (maj, min) = spec.since.unwrap_or((0, 0));
            tracing::info!(
                "private symbol {label} ({feature}) absent on macOS {version} — \
                 only expected since macOS {maj}.{min}; fallback is by design",
                feature = spec.feature,
            );
        }
        None => {
            tracing::warn!(
                "private symbol {label} unresolved and missing from the \
                 version_matrix registry — add a SKYLIGHT_SYMBOLS row"
            );
        }
    }
    None
}

/// Reinterpret a raw symbol pointer as a function pointer of type `T`.
/// Safety: caller guarantees T matches the symbol's actual signature.
unsafe fn as_fn<T: Copy>(ptr: *mut c_void) -> T {
    std::mem::transmute_copy::<*mut c_void, T>(&ptr)
}

/// Resolve `name` through `lock`'s once-per-process cache.
///
/// The test hook is consulted *before* the cache so a forced absence
/// takes effect even after a successful resolution earlier in the
/// process; production callers therefore log a miss exactly once.
fn resolve_cached<T: Copy>(lock: &OnceLock<Option<T>>, name: &'static [u8]) -> Option<T> {
    #[cfg(test)]
    if test_hook::is_forced_absent(symbol_label(name)) {
        tracing::warn!(
            "symbol {} forced absent by test hook — exercising fallback path",
            symbol_label(name)
        );
        return None;
    }
    *lock.get_or_init(|| find_sym(name).map(|p| unsafe { as_fn(p) }))
}

/// Resolve a symbol's presence by bare name (no trailing NUL), with a
/// once-per-process cache per name. This is the probe behind
/// `version_matrix::collect_matrix`.
pub(crate) fn symbol_resolved(name: &str) -> bool {
    #[cfg(test)]
    if test_hook::is_forced_absent(name) {
        tracing::warn!("symbol {name} forced absent by test hook — exercising fallback path");
        return false;
    }
    static CACHE: OnceLock<std::sync::Mutex<std::collections::HashMap<String, bool>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    if let Some(&resolved) = lock(cache).get(name) {
        return resolved;
    }
    let mut cname = String::with_capacity(name.len() + 1);
    cname.push_str(name);
    cname.push('\0');
    let resolved = find_sym(cname.as_bytes()).is_some();
    lock(cache).insert(name.to_owned(), resolved);
    resolved
}

/// Whether `+[SLSEventAuthenticationMessage messageWithEventRecord:pid:version:]`
/// is callable on this OS (macOS 15+, #1503). Cached per process.
///
/// This probes the **metaclass** — the factory is a class method, and
/// `class_respondsToSelector` on the class object alone only sees
/// instance methods (verified on macOS 26.5.2). This reports the true OS
/// capability for the version matrix; the keyboard post path has its own
/// historical guard and is intentionally left untouched (see the NOTE in
/// `post_to_pid`).
pub(crate) fn auth_message_factory_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let cls = objc_class(c"SLSEventAuthenticationMessage");
        if cls.is_null() {
            return false;
        }
        let meta = object_get_class(cls);
        if meta.is_null() {
            return false;
        }
        let sel = sel_register(c"messageWithEventRecord:pid:version:");
        class_responds_to_selector(meta, sel)
    })
}

/// `Class object_getClass(id obj)` — on a Class, returns its metaclass.
fn object_get_class(obj: *mut c_void) -> *mut c_void {
    type GetClassFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
    static SYM: OnceLock<Option<GetClassFn>> = OnceLock::new();
    match resolve_cached(&SYM, b"object_getClass\0") {
        Some(f) => unsafe { f(obj) },
        None => std::ptr::null_mut(),
    }
}

/// Lock a mutex, recovering from poisoning (a panicking test must not
/// cascade-fail unrelated tests).
fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ── Test hook: forced symbol absence ─────────────────────────────────────────

/// Test-only machinery to simulate a macOS version where a symbol is
/// absent, proving the fallback is taken *and* logged. Tests that force
/// absences (or assert host-wide resolution truth) hold the shared gate
/// so parallel tests never observe each other's forced state.
#[cfg(test)]
pub(crate) mod test_hook {
    use super::lock;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

    static FORCED: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    static GATE: OnceLock<Mutex<()>> = OnceLock::new();

    fn forced() -> &'static Mutex<HashSet<&'static str>> {
        FORCED.get_or_init(|| Mutex::new(HashSet::new()))
    }

    pub(crate) fn is_forced_absent(name: &str) -> bool {
        lock(forced()).contains(name)
    }

    /// Take the serialization gate without forcing anything. Tests that
    /// assert "symbols resolve on this host" must hold this so a
    /// concurrent forced-absence test can't flips their reality.
    pub(crate) fn hold_gate() -> MutexGuard<'static, ()> {
        lock(GATE.get_or_init(|| Mutex::new(())))
    }

    /// RAII guard for a forced absence; removes the entry on drop.
    pub(crate) struct ForcedAbsence {
        name: &'static str,
        _gate: MutexGuard<'static, ()>,
    }

    impl Drop for ForcedAbsence {
        fn drop(&mut self) {
            lock(forced()).remove(self.name);
        }
    }

    /// Force `name` to resolve as absent until the returned guard drops.
    pub(crate) fn force_absent(name: &'static str) -> ForcedAbsence {
        let gate = hold_gate();
        lock(forced()).insert(name);
        ForcedAbsence { name, _gate: gate }
    }

    // ── Log capture ──────────────────────────────────────────────────

    /// In-memory tracing writer so tests can assert a fallback was logged.
    #[derive(Clone, Default)]
    struct SharedLog(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedLog {
        type Writer = SharedLog;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Run `body` under a thread-scoped subscriber (DEBUG and above)
    /// that captures into memory; return everything logged while it ran.
    pub(crate) fn captured_logs(body: impl FnOnce()) -> String {
        let log = SharedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(log.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, body);
        let bytes = log.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }
}

// ── Lazily-resolved handles ────────────────────────────────────────────────

fn post_to_pid_fn() -> Option<PostToPidFn> {
    static SYM: OnceLock<Option<PostToPidFn>> = OnceLock::new();
    resolve_cached(&SYM, b"SLEventPostToPid\0")
}

fn set_auth_msg_fn() -> Option<SetAuthMsgFn> {
    static SYM: OnceLock<Option<SetAuthMsgFn>> = OnceLock::new();
    resolve_cached(&SYM, b"SLEventSetAuthenticationMessage\0")
}

fn set_window_loc_fn() -> Option<SetWindowLocFn> {
    static SYM: OnceLock<Option<SetWindowLocFn>> = OnceLock::new();
    resolve_cached(&SYM, b"CGEventSetWindowLocation\0")
}

fn set_int_field_fn() -> Option<SetIntFieldFn> {
    static SYM: OnceLock<Option<SetIntFieldFn>> = OnceLock::new();
    resolve_cached(&SYM, b"SLEventSetIntegerValueField\0")
}

fn connection_id_fn() -> Option<ConnectionIDFn> {
    static SYM: OnceLock<Option<ConnectionIDFn>> = OnceLock::new();
    resolve_cached(&SYM, b"CGSMainConnectionID\0")
}

fn factory_msg_send_fn() -> Option<FactoryMsgSendFn> {
    static SYM: OnceLock<Option<FactoryMsgSendFn>> = OnceLock::new();
    resolve_cached(&SYM, b"objc_msgSend\0")
}

fn set_front_process_fn() -> Option<SetFrontProcessFn> {
    static SYM: OnceLock<Option<SetFrontProcessFn>> = OnceLock::new();
    resolve_cached(&SYM, b"SLPSSetFrontProcessWithOptions\0")
}

fn get_window_owner_fn() -> Option<GetWindowOwnerFn> {
    static SYM: OnceLock<Option<GetWindowOwnerFn>> = OnceLock::new();
    resolve_cached(&SYM, b"SLSGetWindowOwner\0")
}

fn get_connection_psn_fn() -> Option<GetConnectionPSNFn> {
    static SYM: OnceLock<Option<GetConnectionPSNFn>> = OnceLock::new();
    resolve_cached(&SYM, b"SLSGetConnectionPSN\0")
}

fn post_event_record_to_fn() -> Option<PostEventRecordToFn> {
    static SYM: OnceLock<Option<PostEventRecordToFn>> = OnceLock::new();
    resolve_cached(&SYM, b"SLPSPostEventRecordTo\0")
}

fn get_front_process_fn() -> Option<GetFrontProcessFn> {
    static SYM: OnceLock<Option<GetFrontProcessFn>> = OnceLock::new();
    resolve_cached(&SYM, b"_SLPSGetFrontProcess\0")
}

fn get_process_for_pid_fn() -> Option<GetProcessForPIDFn> {
    static SYM: OnceLock<Option<GetProcessForPIDFn>> = OnceLock::new();
    resolve_cached(&SYM, b"GetProcessForPID\0")
}

/// `true` when `SLEventPostToPid` resolved.
pub fn is_available() -> bool {
    post_to_pid_fn().is_some()
}

/// `true` when all three focus-without-raise SPIs resolved.
pub fn is_focus_without_raise_available() -> bool {
    get_front_process_fn().is_some()
        && get_process_for_pid_fn().is_some()
        && post_event_record_to_fn().is_some()
}

// ── ObjC runtime helpers ───────────────────────────────────────────────────

/// Look up an ObjC class by C-string name via `objc_getClass`.
fn objc_class(name: &CStr) -> *mut c_void {
    type GetClassFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
    static SYM: OnceLock<Option<GetClassFn>> = OnceLock::new();
    match resolve_cached(&SYM, b"objc_getClass\0") {
        Some(f) => unsafe { f(name.as_ptr()) },
        None => std::ptr::null_mut(),
    }
}

/// Register / look up an ObjC selector by C-string name via `sel_registerName`.
fn sel_register(name: &CStr) -> *mut c_void {
    type SelRegFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
    static SYM: OnceLock<Option<SelRegFn>> = OnceLock::new();
    match resolve_cached(&SYM, b"sel_registerName\0") {
        Some(f) => unsafe { f(name.as_ptr()) },
        None => std::ptr::null_mut(),
    }
}

/// Whether `cls` actually implements `sel`, via `class_respondsToSelector`.
///
/// macOS 14 (Sonoma) compatibility guard: `SLSEventAuthenticationMessage`
/// exists on macOS 14, but `messageWithEventRecord:pid:version:` was only
/// added in macOS 15 (Sequoia). `sel_registerName` always succeeds (it just
/// interns the string), so a `!sel.is_null()` check is not enough — we must
/// confirm the class responds before calling `objc_msgSend`, or the runtime
/// raises `NSInvalidArgumentException: unrecognized selector`. See #1503.
fn class_responds_to_selector(cls: *mut c_void, sel: *mut c_void) -> bool {
    if cls.is_null() || sel.is_null() {
        return false;
    }
    type RespondsToFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool;
    static SYM: OnceLock<Option<RespondsToFn>> = OnceLock::new();
    match resolve_cached(&SYM, b"class_respondsToSelector\0") {
        Some(f) => unsafe { f(cls, sel) },
        None => false,
    }
}

// ── SLSEventRecord extraction ──────────────────────────────────────────────

/// Offset of the `SLSEventRecord *` field inside `__CGEvent` for a macOS
/// version, or `None` when the layout is unverified on that version.
///
/// Layout (SkyLight ObjC type encodings): `{CFRuntimeBase, uint32_t,
/// SLSEventRecord *}`. On 64-bit: CFRuntimeBase = 16 bytes, uint32 = 4,
/// 4 bytes padding → record pointer at offset 24. Verified stable on
/// every 64-bit macOS from 13 (Ventura) through 26 (Tahoe). Outside that
/// range is a checked failure — this function never probes raw offsets
/// hoping one reads non-null (the pre-#8 behavior).
fn event_record_offset(version: crate::version_matrix::MacOsVersion) -> Option<usize> {
    match version.major {
        13..=26 => Some(24),
        _ => None,
    }
}

/// Extract the embedded `SLSEventRecord *` from a `CGEvent`.
///
/// Checked, version-gated resolution: the offset comes from
/// [`event_record_offset`], and the pointer read is validated (non-null,
/// pointer-aligned) before use. Every failure mode logs once per process
/// and returns null, which makes the caller skip the auth envelope — the
/// same graceful degradation as a missing SPI, but never silent.
unsafe fn extract_event_record(event_ptr: *mut c_void) -> *mut c_void {
    let version = crate::version_matrix::macos_version();
    let Some(offset) = event_record_offset(version) else {
        static GATE_WARN: std::sync::Once = std::sync::Once::new();
        GATE_WARN.call_once(|| {
            tracing::warn!(
                "__CGEvent layout unverified on macOS {version} — skipping keyboard \
                 auth envelope (checked failure; extend event_record_offset for this OS)"
            );
        });
        return std::ptr::null_mut();
    };
    let slot = (event_ptr as *const u8).add(offset).cast::<*mut c_void>();
    let record = std::ptr::read_unaligned(slot);
    if record.is_null() {
        static NULL_WARN: std::sync::Once = std::sync::Once::new();
        NULL_WARN.call_once(|| {
            tracing::warn!(
                "SLSEventRecord pointer null at verified offset {offset} — \
                 skipping keyboard auth envelope"
            );
        });
        return std::ptr::null_mut();
    }
    if (record as usize) % std::mem::align_of::<*mut c_void>() != 0 {
        static ALIGN_WARN: std::sync::Once = std::sync::Once::new();
        ALIGN_WARN.call_once(|| {
            tracing::warn!(
                "SLSEventRecord pointer {record:p} misaligned at offset {offset} — \
                 __CGEvent layout drift on macOS {version}? skipping auth envelope"
            );
        });
        return std::ptr::null_mut();
    }
    record
}

// ── Public entry points ────────────────────────────────────────────────────

/// Post `event_ptr` (raw `CGEventRef`) to `pid` via `SLEventPostToPid`.
///
/// `attach_auth_message`: pass `true` for keyboard events (Chromium path),
/// `false` for mouse events (see Swift doc comment on `postToPid`).
///
/// Returns `true` when `SLEventPostToPid` resolved and the post was attempted.
/// Returns `false` when the SPI is absent — caller falls back to `CGEvent::post_to_pid`.
pub(super) fn post_to_pid(pid: pid_t, event_ptr: *mut c_void, attach_auth_message: bool) -> bool {
    let post_fn = match post_to_pid_fn() {
        Some(f) => f,
        None => return false,
    };

    if attach_auth_message {
        // Build and attach SLSEventAuthenticationMessage.
        //
        // macOS 14 (Sonoma) compatibility: the class exists on macOS 14 but
        // `messageWithEventRecord:pid:version:` was added in macOS 15. Guard
        // with `class_respondsToSelector` (a `!sel.is_null()` check is not
        // enough — `sel_registerName` interns any name); when the selector is
        // absent we skip the auth envelope and fall through to the plain
        // `SLEventPostToPid` below. Chromium-class targets may not receive the
        // event on macOS 14, but the daemon no longer crashes. See #1503.
        let cls = objc_class(c"SLSEventAuthenticationMessage");
        let sel = sel_register(c"messageWithEventRecord:pid:version:");
        let factory = factory_msg_send_fn();

        // NOTE: `class_respondsToSelector` probes *instance* methods, but the
        // factory is a *class* method on the metaclass — this guard therefore
        // never fires and the envelope is never attached (verified on macOS
        // 26.5.2; the Swift reference probed the metaclass via `responds(to:)`).
        // Kept byte-identical for behavior stability; fix tracked as #10.
        // The version matrix probes the true capability via the metaclass.
        if class_responds_to_selector(cls, sel) {
            if let Some(factory_fn) = factory {
                let record = unsafe { extract_event_record(event_ptr) };
                if !record.is_null() {
                    let msg = unsafe { factory_fn(cls, sel, record, pid as c_int, 0u32) };
                    if !msg.is_null() {
                        if let Some(set_auth) = set_auth_msg_fn() {
                            unsafe { set_auth(event_ptr, msg) };
                        }
                    }
                }
            }
        }
    }

    unsafe { post_fn(pid, event_ptr) };
    true
}

/// Stamp a window-local `(x, y)` point onto `event_ptr` via the private
/// `CGEventSetWindowLocation` SPI. Returns `true` when the SPI resolved.
pub(super) fn set_window_location(event_ptr: *mut c_void, x: f64, y: f64) -> bool {
    match set_window_loc_fn() {
        Some(f) => {
            unsafe { f(event_ptr, x, y) };
            true
        }
        None => false,
    }
}

/// Stamp `value` onto `event_ptr` at raw SkyLight field index `field` via
/// `SLEventSetIntegerValueField`. Returns `false` when SPI absent.
pub(super) fn set_integer_field(event_ptr: *mut c_void, field: u32, value: i64) -> bool {
    match set_int_field_fn() {
        Some(f) => {
            unsafe { f(event_ptr, field, value) };
            true
        }
        None => false,
    }
}

/// Return the Skylight main connection ID for the current process.
pub fn main_connection_id() -> Option<u32> {
    connection_id_fn().map(|f| unsafe { f() })
}

// ── Focus-without-raise ───────────────────────────────────────────────────────

/// Activate `target_pid`'s window `target_wid` without raising any windows
/// or triggering Space-follow. Ported from yabai's
/// `window_manager_focus_window_without_raise`.
///
/// Recipe:
/// 1. `_SLPSGetFrontProcess` → capture current front PSN.
/// 2. `GetProcessForPID(target_pid)` → target PSN.
/// 3. Post 248-byte defocus record to front PSN (`bytes[0x8a] = 0x02`).
/// 4. Post 248-byte focus record to target PSN (`bytes[0x8a] = 0x01`,
///    `bytes[0x3c..0x3f]` = `target_wid` little-endian).
///
/// Deliberately skips `SLPSSetFrontProcessWithOptions` — see the Swift
/// reference `FocusWithoutRaise.swift` for why omitting it keeps
/// Chromium's user-activation gate open.
///
/// Returns `true` when all SPIs resolved and both posts succeeded.
pub fn activate_without_raise(target_pid: pid_t, target_wid: u32) -> bool {
    let post_fn = match post_event_record_to_fn() {
        Some(f) => f,
        None => return false,
    };
    let get_front = match get_front_process_fn() {
        Some(f) => f,
        None => return false,
    };
    let get_pid_psn = match get_process_for_pid_fn() {
        Some(f) => f,
        None => return false,
    };

    // 8-byte PSN buffers (two UInt32s).
    let mut prev_psn = [0u8; 8];
    let mut target_psn = [0u8; 8];

    let ok_prev = unsafe { get_front(prev_psn.as_mut_ptr() as *mut c_void) } == 0;
    if !ok_prev {
        return false;
    }

    let ok_target = unsafe { get_pid_psn(target_pid, target_psn.as_mut_ptr() as *mut c_void) } == 0;
    if !ok_target {
        return false;
    }

    // Build the 248-byte event buffer.
    let mut buf = [0u8; 0xF8];
    buf[0x04] = 0xF8;
    buf[0x08] = 0x0D;
    // Stamp target window id in little-endian at bytes 0x3c–0x3f.
    buf[0x3C] = (target_wid & 0xFF) as u8;
    buf[0x3D] = ((target_wid >> 8) & 0xFF) as u8;
    buf[0x3E] = ((target_wid >> 16) & 0xFF) as u8;
    buf[0x3F] = ((target_wid >> 24) & 0xFF) as u8;

    // Step 3: defocus previous front.
    buf[0x8A] = 0x02;
    let defocus_ok = unsafe { post_fn(prev_psn.as_ptr() as *const c_void, buf.as_ptr()) == 0 };

    // Step 4: focus target.
    buf[0x8A] = 0x01;
    let focus_ok = unsafe { post_fn(target_psn.as_ptr() as *const c_void, buf.as_ptr()) == 0 };

    defocus_ok && focus_ok
}

// ── NSMenu shortcut activation ────────────────────────────────────────────────

/// Gets the PSN for the process that owns `window_id`.
/// Uses `CGSMainConnectionID` + `SLSGetWindowOwner` + `SLSGetConnectionPSN`.
/// Falls back to `GetProcessForPID(pid)` when the SkyLight path fails.
pub fn get_process_psn_for_window(window_id: u32, pid: libc::pid_t, out_psn: &mut [u8; 8]) -> bool {
    // Try modern path: CGSMainConnectionID → SLSGetWindowOwner → SLSGetConnectionPSN
    if let (Some(get_owner), Some(get_psn), Some(conn_id_fn)) = (
        get_window_owner_fn(),
        get_connection_psn_fn(),
        connection_id_fn(),
    ) {
        let main_cid = unsafe { conn_id_fn() };
        let mut owner_cid: u32 = 0;
        let ok = unsafe { get_owner(main_cid, window_id, &mut owner_cid) } == 0;
        if ok && owner_cid != 0 {
            let psn_ok = unsafe { get_psn(owner_cid, out_psn.as_mut_ptr() as *mut c_void) } == 0;
            if psn_ok {
                return true;
            }
        }
    }
    // Fallback: GetProcessForPID
    if let Some(get_pid_psn) = get_process_for_pid_fn() {
        return unsafe { get_pid_psn(pid, out_psn.as_mut_ptr() as *mut c_void) } == 0;
    }
    false
}

/// Make `target_pid` and `target_wid` WindowServer-frontmost and leave them
/// there. Unlike [`with_foreground_assist`], this deliberately does not save or
/// restore the previous process. It is the persistent counterpart required by
/// focus-proxy surfaces whose input channel is armed only while genuinely
/// frontmost.
///
/// Returns `true` only when the target PSN resolved and WindowServer accepted
/// `SLPSSetFrontProcessWithOptions`.
pub fn set_front_process_persistently(target_pid: libc::pid_t, target_wid: u32) -> bool {
    let Some(set_front) = set_front_process_fn() else {
        return false;
    };
    let mut target_psn = [0u8; 8];
    if !get_process_psn_for_window(target_wid, target_pid, &mut target_psn) {
        return false;
    }

    // kCPSNoWindows = 0x400. Supplying the exact target window still makes
    // that window's process frontmost while avoiding a broad all-window raise.
    unsafe { set_front(target_psn.as_ptr() as *const c_void, target_wid, 0x400) == 0 }
}

/// Tool-agnostic foreground-assist: briefly front `window_id`, run `body` (which
/// posts the synthetic input), then restore the prior frontmost process.
///
/// This is the `delivery_mode:"foreground"` rung of the best-effort-background
/// ladder, shared by `type_text` and `click`. It is the same brief front →
/// act → restore primitive `press_key`/`hotkey` use for NSMenu key dispatch —
/// see [`with_menu_shortcut_activation`], which this delegates to. Reached only
/// when the agent has seen the background rungs fail (clicks) or the field is
/// unverifiable + focus-sensitive (Catalyst typing).
///
/// Returns `Ok(true)` when the brief activation happened, `Ok(false)` when the
/// fronting SPIs are unavailable (the body still ran, just without a front).
pub fn with_foreground_assist(
    target_pid: libc::pid_t,
    target_wid: u32,
    body: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    with_menu_shortcut_activation(target_pid, target_wid, body)
}

/// Activate an exact target window for a global HID keyboard action.
///
/// Unlike [`with_menu_shortcut_activation`], this helper must not run `action`
/// when the private foreground SPI is unavailable: a global HID event has no
/// pid addressing and would otherwise land in whichever application is
/// currently frontmost. The short settles keep the target frontmost until
/// WindowServer has routed both sides of the key chord, then restore the prior
/// process even when the action fails.
pub fn with_foreground_hid_activation(
    target_pid: libc::pid_t,
    target_wid: u32,
    action: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let set_front = set_front_process_fn()
        .ok_or_else(|| anyhow::anyhow!("foreground HID delivery is unavailable"))?;

    let mut prev_psn = [0u8; 8];
    let prev_ok = get_front_process_fn()
        .map(|f| unsafe { f(prev_psn.as_mut_ptr() as *mut c_void) } == 0)
        .unwrap_or(false);

    let mut target_psn = [0u8; 8];
    if !get_process_psn_for_window(target_wid, target_pid, &mut target_psn) {
        anyhow::bail!("could not resolve target window for foreground HID delivery");
    }
    let activated = unsafe { set_front(target_psn.as_ptr() as *const c_void, target_wid, 0x400) };
    if activated != 0 {
        anyhow::bail!("WindowServer rejected foreground HID activation");
    }

    std::thread::sleep(std::time::Duration::from_millis(40));
    let result = action();
    std::thread::sleep(std::time::Duration::from_millis(40));

    if prev_ok {
        unsafe { set_front(prev_psn.as_ptr() as *const c_void, 0, 0x400) };
    }

    result
}

/// Activate `target_pid`'s window `target_wid` for NSMenu key dispatch, run `action`,
/// then immediately restore the prior frontmost process.
///
/// The entire activate → action → restore sequence is < 1 ms — a 5 ms UX monitor
/// never observes the intermediate frontmost state. NSMenu still fires because the
/// key event is already enqueued in the target's run-loop queue before we restore.
///
/// Returns `Ok(true)` when activation succeeded, `Ok(false)` when SPIs unavailable.
pub fn with_menu_shortcut_activation(
    target_pid: libc::pid_t,
    target_wid: u32,
    action: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    let set_front = match set_front_process_fn() {
        Some(f) => f,
        None => {
            // SPIs unavailable — run action anyway without activation.
            action()?;
            return Ok(false);
        }
    };

    // Capture prior frontmost PSN.
    let mut prev_psn = [0u8; 8];
    let prev_ok = get_front_process_fn()
        .map(|f| unsafe { f(prev_psn.as_mut_ptr() as *mut c_void) } == 0)
        .unwrap_or(false);

    // Resolve target PSN.
    let mut target_psn = [0u8; 8];
    let target_ok = get_process_psn_for_window(target_wid, target_pid, &mut target_psn);
    if !target_ok {
        action()?;
        return Ok(false);
    }

    // Make target WindowServer-frontmost (kCPSNoWindows = 0x400).
    unsafe { set_front(target_psn.as_ptr() as *const c_void, target_wid, 0x400) };

    // Run action then restore — even if action fails.
    let result = action();

    // Restore prior frontmost (windowID=0, options=0x400).
    if prev_ok {
        unsafe { set_front(prev_psn.as_ptr() as *const c_void, 0, 0x400) };
    }

    result?;
    Ok(true)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use test_hook::captured_logs;

    #[test]
    fn skylight_framework_load_is_checked_and_succeeds() {
        assert!(
            ensure_skylight_loaded(),
            "dlopen(SkyLight.framework) must succeed on a macOS host"
        );
    }

    #[test]
    fn required_symbols_resolve_on_this_host() {
        let _gate = test_hook::hold_gate();
        let version = crate::version_matrix::macos_version();
        for name in [
            "SLEventPostToPid",
            "CGSMainConnectionID",
            "SLPSSetFrontProcessWithOptions",
            "objc_msgSend",
        ] {
            assert!(
                symbol_resolved(name),
                "{name} must resolve on macOS {version} — private API drift on a supported OS"
            );
        }
    }

    #[test]
    fn forced_absent_symbol_produces_logged_fallback() {
        let _forced = test_hook::force_absent("SLEventPostToPid");
        let logs = captured_logs(|| {
            // A null event pointer is safe here: with the symbol forced
            // absent, post_to_pid returns false before the event is
            // ever dereferenced — exactly the production fallback shape.
            let posted = post_to_pid(424_242, std::ptr::null_mut(), false);
            assert!(
                !posted,
                "forced absence must return false so the caller falls back"
            );
        });
        assert!(
            logs.contains("SLEventPostToPid"),
            "the fallback log must name the missing symbol; got:\n{logs}"
        );
        assert!(
            logs.contains("forced absent"),
            "the fallback log must be explicit, not silent; got:\n{logs}"
        );
    }

    #[test]
    fn forced_absence_is_visible_to_the_matrix_probe() {
        let _forced = test_hook::force_absent("CGSMainConnectionID");
        assert!(
            !symbol_resolved("CGSMainConnectionID"),
            "the matrix probe must observe forced absences"
        );
    }

    #[test]
    fn auth_message_factory_capability_matches_os_version() {
        let _gate = test_hook::hold_gate();
        let version = crate::version_matrix::macos_version();
        // Metaclass probe of the true OS capability: the factory class
        // method exists on macOS 15+ (#1503) and on this host (26.5.2).
        assert_eq!(
            auth_message_factory_supported(),
            version.major >= 15,
            "auth factory capability must match the OS version gate"
        );
    }

    #[test]
    fn forced_absence_lifts_when_guard_drops() {
        {
            let _forced = test_hook::force_absent("SLSGetWindowOwner");
            assert!(!symbol_resolved("SLSGetWindowOwner"));
        }
        let _gate = test_hook::hold_gate();
        assert!(
            symbol_resolved("SLSGetWindowOwner"),
            "resolution must recover once the forced absence is lifted"
        );
    }

    // ── Version-gated SLSEventRecord extraction (G9) ─────────────────

    #[test]
    fn event_record_offset_is_version_gated() {
        use crate::version_matrix::MacOsVersion;
        assert_eq!(event_record_offset(MacOsVersion::new(13, 0, 0)), Some(24));
        assert_eq!(event_record_offset(MacOsVersion::new(15, 7, 1)), Some(24));
        assert_eq!(event_record_offset(MacOsVersion::new(26, 5, 2)), Some(24));
        // Unknown/unsupported versions are a checked failure, never a probe.
        assert_eq!(event_record_offset(MacOsVersion::new(12, 7, 6)), None);
        assert_eq!(event_record_offset(MacOsVersion::new(27, 0, 0)), None);
        assert_eq!(event_record_offset(MacOsVersion::new(0, 0, 0)), None);
    }

    /// Fabricated `__CGEvent` buffer: 64 bytes, pointer-aligned.
    fn fake_event(writes: &[(usize, *mut c_void)]) -> [u64; 8] {
        let mut buf = [0u64; 8];
        for &(offset, ptr) in writes {
            assert_eq!(offset % 8, 0, "test writes must be pointer-aligned");
            buf[offset / 8] = ptr as usize as u64;
        }
        buf
    }

    static FAKE_RECORD: u64 = 0xDEAD;

    #[test]
    fn extract_reads_the_verified_offset() {
        let record_ptr = &FAKE_RECORD as *const u64 as *mut c_void;
        let mut buf = fake_event(&[(24, record_ptr)]);
        let got = unsafe { extract_event_record(buf.as_mut_ptr() as *mut c_void) };
        assert_eq!(got, record_ptr);
    }

    #[test]
    fn extract_does_not_probe_other_offsets() {
        // The old resolver probed offsets 24/32/16 and took the first
        // non-null pointer. With null at the verified offset and a valid
        // pointer at 32, checked resolution must return null — never the
        // probed garbage.
        let record_ptr = &FAKE_RECORD as *const u64 as *mut c_void;
        let mut buf = fake_event(&[(32, record_ptr)]);
        let got = unsafe { extract_event_record(buf.as_mut_ptr() as *mut c_void) };
        assert!(
            got.is_null(),
            "no raw offset probing: a non-null slot at 32 must not be picked up"
        );
    }

    #[test]
    fn extract_rejects_misaligned_record_pointer() {
        let mut buf = fake_event(&[(24, 0x1001usize as *mut c_void)]);
        let got = unsafe { extract_event_record(buf.as_mut_ptr() as *mut c_void) };
        assert!(
            got.is_null(),
            "a misaligned record pointer is layout drift — checked failure"
        );
    }

    #[test]
    fn extract_rejects_null_record_pointer() {
        let mut buf = fake_event(&[]);
        let got = unsafe { extract_event_record(buf.as_mut_ptr() as *mut c_void) };
        assert!(got.is_null());
    }
}
