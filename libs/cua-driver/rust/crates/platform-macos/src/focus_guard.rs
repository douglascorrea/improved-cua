//! FocusGuard — Rust port of Swift's `FocusGuard.withFocusSuppressed`
//! (`libs/cua-driver/Sources/CuaDriverCore/Focus/FocusGuard.swift`).
//!
//! ## What this does
//!
//! Wraps an individual AX-action call (the inside of a click / type /
//! set_value / drag dispatch) with a **targeted** focus-steal suppression
//! lease. Catches the case where dispatching an AX attribute write
//! triggers a reflexive self-activation in the target app — Safari /
//! WebKit will sometimes pull itself to the front when an
//! `AXSelectedText` write hits a focused input, even when the AX call
//! itself doesn't go through `NSApp.activate(…)`.
//!
//! ## Layering relative to Swift
//!
//! Swift's `FocusGuard.withFocusSuppressed` does three things:
//!
//! 1. **Enablement** — set `AXManualAccessibility` /
//!    `AXEnhancedUserInterface` on the app root so Chromium/Electron
//!    targets respond to attribute-level dispatch. In this port the
//!    tree walker performs enablement at walk time (tree.rs); at
//!    dispatch time we re-assert it via `tree::ensure_ax_enabled_for_pid`
//!    (a cached no-op after the first call).
//! 2. **Synthetic focus** — write `AXFocused` / `AXMain` on the
//!    enclosing window + element before the action, restore after.
//!    Makes AppKit behave as if the action came from a focused
//!    process, preventing reflex activations. Ported here as
//!    [`SyntheticFocusGuard`] (Swift's `SyntheticAppFocusEnforcer`).
//! 3. **Reactive** — arm `SystemFocusStealPreventer` with a targeted
//!    entry. If the target self-activates anyway, re-activates the
//!    prior frontmost. Ported as `focus_steal.rs`; this module arms it
//!    as the backstop around every wrapped action.
//!
//! All three layers are now ported. Layer 2 is the proactive half — it
//! stops the reflex activation from firing at all on Chromium/Safari —
//! while layer 3 remains as the backstop for anything that fires anyway.
//!
//! ## Skip rules (mirrors Swift)
//!
//! - **Target already frontmost** → no synthetic focus, no lease. The
//!   element is genuinely focusable; writing synthetic state would fight
//!   the real focus.
//! - **Window minimized** → no synthetic focus (bare action only).
//!   Writing `AXFocused`/`AXMain` on a minimized window triggers Chrome
//!   (and likely others) to deminiaturize. The bare AX action still
//!   works on the minimized AX tree.
//! - **Restore covers exactly the priors that were readable.** When a
//!   prior value couldn't be read we leave the synthetic `true` in place
//!   rather than write a bogus `false` (Swift semantics). On Chrome and
//!   Safari all three attributes are readable, so in practice nothing
//!   synthetic is left behind.
//!
//! ## Why this is a separate module
//!
//! `focus_steal::with_suppression` already exists as a thin closure
//! API around the dispatcher. `focus_guard::with_focus_suppressed`
//! wraps it with:
//!
//! 1. A "is target already frontmost?" check so we don't arm a useless
//!    self→self suppressor (matches Swift's `isTargetFrontmost` guard).
//! 2. A 50ms post-action sleep that gives any in-flight focus-grab
//!    reflex time to fire and be observed by the suppressor before the
//!    lease is dropped. Matches Swift's `Task.sleep(nanoseconds: 50ms)`.
//! 3. A static `origin` label for tracing — call sites pass a short
//!    string like `"click.AXPress"` so leaked leases / late-firing
//!    observers can be traced back to the caller.

use std::time::Duration;

use core_foundation::base::{CFRelease, CFTypeRef};

use crate::apps;
use crate::ax::bindings::{
    copy_bool_attr, copy_element_attr, set_bool_attr, AXUIElementRef,
};
use crate::focus_steal;

/// Wrap an async closure `f` with a targeted focus-steal suppressor.
///
/// - `target_pid` — the pid the action is dispatched to. `Some(pid)` is
///   the standard case; `None` skips the targeted entry entirely
///   (caller relies on the surrounding `WindowChangeDetector` wildcard
///   lease).
/// - `prior_frontmost` — the pid to restore focus to if the target
///   activates. Typically captured from `apps::frontmost_pid()` before
///   the snapshot.
/// - `origin` — short static label for tracing, e.g. `"click.AXPress"`.
/// - `f` — the action to run with suppression armed.
///
/// Returns whatever `f` returns. Drops the lease ~50ms after `f`
/// resolves so any in-flight reflex activation is observed before
/// suppression ends.
///
/// If `target_pid` is already the frontmost app (no point fighting
/// ourselves) or `prior_frontmost` is `None` (no app to restore to),
/// the function still runs `f` — just without the lease. This keeps
/// the call sites simple (no per-tool `if frontmost == target` ladders).
///
/// This is the element-less form: only layer 3 (reactive lease) applies.
/// AX-action call sites that hold an element pointer should prefer
/// [`with_focus_suppressed_ax`], which additionally arms layers 1+2.
pub async fn with_focus_suppressed<F, Fut, R>(
    target_pid: Option<i32>,
    prior_frontmost: Option<i32>,
    origin: &'static str,
    f: F,
) -> R
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = R>,
{
    with_focus_suppressed_ax(target_pid, prior_frontmost, None, origin, f).await
}

/// Element-aware form of [`with_focus_suppressed`]: arms the full
/// three-layer FocusGuard stack around an AX action.
///
/// - Layer 1 (enablement): re-asserted at dispatch time via
///   `tree::ensure_ax_enabled_for_pid` — cached no-op after first use.
/// - Layer 2 (synthetic focus): when `element_ptr` is `Some` and the
///   target is a background app, a [`SyntheticFocusGuard`] writes
///   `AXFocused`/`AXMain` on the enclosing window + `AXFocused` on the
///   element before `f` runs and restores the priors afterwards.
/// - Layer 3 (reactive lease): unchanged from `with_focus_suppressed`.
///
/// Ordering mirrors Swift: restore synthetic focus **first** (with the
/// lease still armed — the restore writes can themselves trip a reflex),
/// then the 50ms settle, then the lease drops.
pub async fn with_focus_suppressed_ax<F, Fut, R>(
    target_pid: Option<i32>,
    prior_frontmost: Option<i32>,
    element_ptr: Option<usize>,
    origin: &'static str,
    f: F,
) -> R
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = R>,
{
    // Decide whether to arm. Three conditions must hold:
    //   1. We have a known target pid (Some).
    //   2. We have a prior frontmost to restore to (Some).
    //   3. The target isn't already frontmost — Swift's
    //      `isTargetFrontmost` short-circuit, mirrored here. Without
    //      it we'd arm an entry that re-activates the prior frontmost
    //      against a no-op activation, fighting our own previous
    //      restore.
    let should_arm = match (target_pid, prior_frontmost) {
        (Some(tp), Some(pf)) => tp != pf,
        _ => false,
    };

    // Layers 1+2 — synthetic focus around the AX action. All AX traffic
    // goes through spawn_blocking: attribute writes are IPC with a
    // messaging timeout and must not park the async executor.
    let focus_guard = if should_arm {
        if let (Some(tp), Some(el)) = (target_pid, element_ptr) {
            tokio::task::spawn_blocking(move || {
                SyntheticFocusGuard::arm_if_background(tp, prior_frontmost, el)
            })
            .await
            .ok()
            .flatten()
        } else {
            None
        }
    } else {
        None
    };

    // Layer 3 — reactive backstop lease.
    let _lease = if should_arm {
        // unwrap() is safe — `should_arm` is true only when both are Some.
        Some(focus_steal::begin_suppression(
            target_pid,
            prior_frontmost.unwrap(),
            origin,
        ))
    } else {
        None
    };

    let result = f().await;

    // Restore synthetic focus BEFORE the lease drops (Swift order:
    // `reenableActivation` → 50ms sleep → `lease.release()`).
    if let Some(guard) = focus_guard {
        let _ = tokio::task::spawn_blocking(move || guard.restore()).await;
    }

    // Post-action settle — give the reactive observer time to fire on
    // any side-effect activation before we drop the lease. Matches
    // Swift's 50ms sleep in `FocusGuard.withFocusSuppressed`.
    if _lease.is_some() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // _lease drops here (RAII end_suppression).
    result
}

// ── Layer 2: synthetic focus (Swift SyntheticAppFocusEnforcer port) ─────────

/// A single boolean focus-attribute write. Produced by the pure planners
/// ([`plan_arm_writes`] / [`plan_restore_writes`]) and applied by
/// [`SyntheticFocusGuard`]. Keeping the plan pure makes the write set —
/// including the restore-only-what-was-readable rule — unit-testable
/// without a live AX target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FocusWrite {
    WindowFocused(bool),
    WindowMain(bool),
    ElementFocused(bool),
}

/// Synthetic focus is armed only for a background target with an element:
/// the issue's "skip when target already frontmost" rule plus the two
/// missing-information cases (no target pid, no prior frontmost — we
/// can't prove the target is background, so don't touch its focus state).
pub(crate) fn should_arm_synthetic(
    target_pid: Option<i32>,
    prior_frontmost: Option<i32>,
    element_ptr: Option<usize>,
) -> bool {
    matches!((target_pid, prior_frontmost, element_ptr), (Some(tp), Some(pf), Some(_)) if tp != pf)
}

/// The arming write set: `AXFocused=true` + `AXMain=true` on the window
/// (when resolved) then `AXFocused=true` on the element. Mirrors Swift's
/// `preventActivation` (window first, element second).
pub(crate) fn plan_arm_writes(has_window: bool) -> Vec<FocusWrite> {
    let mut writes = Vec::with_capacity(3);
    if has_window {
        writes.push(FocusWrite::WindowFocused(true));
        writes.push(FocusWrite::WindowMain(true));
    }
    writes.push(FocusWrite::ElementFocused(true));
    writes
}

/// The restore write set: each prior that was successfully read on the
/// way in, restored in the same window→element order. Priors that
/// couldn't be read are skipped — writing a bogus `false` would be worse
/// than leaving the synthetic `true` (Swift `reenableActivation`).
pub(crate) fn plan_restore_writes(
    has_window: bool,
    prior_window_focused: Option<bool>,
    prior_window_main: Option<bool>,
    prior_element_focused: Option<bool>,
) -> Vec<FocusWrite> {
    let mut writes = Vec::with_capacity(3);
    if has_window {
        if let Some(v) = prior_window_focused {
            writes.push(FocusWrite::WindowFocused(v));
        }
        if let Some(v) = prior_window_main {
            writes.push(FocusWrite::WindowMain(v));
        }
    }
    if let Some(v) = prior_element_focused {
        writes.push(FocusWrite::ElementFocused(v));
    }
    writes
}

/// RAII port of Swift's `SyntheticAppFocusEnforcer` + `FocusState`.
///
/// `arm_if_background` snapshots the prior values of `AXFocused`/`AXMain`
/// on the element's enclosing window and `AXFocused` on the element,
/// writes `true` to each, and hands back a guard. [`restore`](Self::restore)
/// writes the priors back; `Drop` covers the panic/cancel paths so the
/// target is never left with synthetic focus installed.
///
/// The element pointer is borrowed — the caller (tool) holds the element
/// alive for the whole dispatch via its cache retain guard. The window
/// reference is separately retained (`copy_element_attr` +1) and released
/// on restore. All fields are plain data so the guard is `Send` and can
/// cross `spawn_blocking` boundaries.
pub struct SyntheticFocusGuard {
    element: usize,
    window: Option<usize>,
    prior_window_focused: Option<bool>,
    prior_window_main: Option<bool>,
    prior_element_focused: Option<bool>,
    restored: bool,
}

impl SyntheticFocusGuard {
    /// Arm synthetic focus for `element_ptr` when `target_pid` is a
    /// background app. Returns `None` when the skip rules say not to
    /// (frontmost target, missing info, or a minimized window). Also
    /// re-asserts AX enablement (layer 1) before touching focus state.
    ///
    /// Must be called from a thread where blocking AX IPC is acceptable
    /// (a `spawn_blocking` worker), not the async executor.
    pub fn arm_if_background(
        target_pid: i32,
        prior_frontmost: Option<i32>,
        element_ptr: usize,
    ) -> Option<Self> {
        if !should_arm_synthetic(Some(target_pid), prior_frontmost, Some(element_ptr)) {
            return None;
        }
        // Layer 1 — cached no-op once the pid has been enabled by any walk.
        crate::ax::tree::ensure_ax_enabled_for_pid(target_pid);
        Self::arm_unchecked(element_ptr)
    }

    /// Read priors + write synthetic `true`s. `None` only when the
    /// window exists and is minimized (the Chrome deminiaturize hazard).
    fn arm_unchecked(element_ptr: usize) -> Option<Self> {
        unsafe {
            let element = element_ptr as AXUIElementRef;
            // Resolve the enclosing window via the element's own AXWindow
            // attribute — Swift's `enclosingWindow(of:)`. Best-effort:
            // some AX trees omit AXWindow on deeply nested elements; in
            // that case we still synthesize focus on the element alone.
            let window = copy_element_attr(element, "AXWindow").map(|w| w as usize);

            if let Some(w) = window {
                // SKIP when minimized — writing AXFocused/AXMain on a
                // minimized window triggers deminiaturization (Chrome).
                // The bare AX action still works on the minimized tree.
                if copy_bool_attr(w as AXUIElementRef, "AXMinimized") == Some(true) {
                    CFRelease(w as CFTypeRef);
                    return None;
                }
            }

            let prior_window_focused =
                window.and_then(|w| copy_bool_attr(w as AXUIElementRef, "AXFocused"));
            let prior_window_main =
                window.and_then(|w| copy_bool_attr(w as AXUIElementRef, "AXMain"));
            let prior_element_focused = copy_bool_attr(element, "AXFocused");

            // Best-effort writes: elements that don't support AXFocused
            // (labels, static text) reject the write and we move on —
            // same trade-off as the Swift enforcer (no logging either;
            // the failures are routine).
            for write in plan_arm_writes(window.is_some()) {
                apply_write(element_ptr, window, &write);
            }

            Some(Self {
                element: element_ptr,
                window,
                prior_window_focused,
                prior_window_main,
                prior_element_focused,
                restored: false,
            })
        }
    }

    /// Restore the priors captured at arm time and release the retained
    /// window reference. Idempotent; `Drop` calls this too, so a guard
    /// dropped without an explicit restore still cleans up.
    pub fn restore(mut self) {
        self.restore_inner();
    }

    fn restore_inner(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        let writes = plan_restore_writes(
            self.window.is_some(),
            self.prior_window_focused,
            self.prior_window_main,
            self.prior_element_focused,
        );
        for write in writes {
            apply_write(self.element, self.window, &write);
        }
        if let Some(w) = self.window.take() {
            unsafe { CFRelease(w as CFTypeRef) };
        }
    }
}

impl Drop for SyntheticFocusGuard {
    fn drop(&mut self) {
        self.restore_inner();
    }
}

/// Apply one planned write, best-effort. Errors are swallowed by design —
/// see the Swift enforcer's rationale (primary action landing matters,
/// focus fidelity is second-order).
fn apply_write(element: usize, window: Option<usize>, write: &FocusWrite) {
    unsafe {
        match write {
            FocusWrite::WindowFocused(v) => {
                if let Some(w) = window {
                    let _ = set_bool_attr(w as AXUIElementRef, "AXFocused", *v);
                }
            }
            FocusWrite::WindowMain(v) => {
                if let Some(w) = window {
                    let _ = set_bool_attr(w as AXUIElementRef, "AXMain", *v);
                }
            }
            FocusWrite::ElementFocused(v) => {
                let _ = set_bool_attr(element as AXUIElementRef, "AXFocused", *v);
            }
        }
    }
}

/// Convenience wrapper that captures the current frontmost via
/// `apps::frontmost_pid()` at call time. Use when the caller doesn't
/// already have a `prior_frontmost` from a surrounding snapshot —
/// e.g. tools that only opt into the layer-3 guard without the
/// snapshot/detect cycle.
///
/// Most action tools should prefer `with_focus_suppressed` with an
/// explicit `prior_frontmost` captured *before* the snapshot, so the
/// wildcard lease + targeted lease both restore to the same pid.
pub async fn with_focus_suppressed_now<F, Fut, R>(
    target_pid: Option<i32>,
    origin: &'static str,
    f: F,
) -> R
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = R>,
{
    let prior = apps::frontmost_pid();
    with_focus_suppressed(target_pid, prior, origin, f).await
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// `with_focus_suppressed` runs the closure and returns its value.
    /// Skip-arming path (no prior frontmost) still executes f.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runs_closure_without_prior_frontmost() {
        let n = with_focus_suppressed(
            Some(42),
            None, // no prior → no lease armed
            "test.no_prior",
            || async { 7 },
        )
        .await;
        assert_eq!(n, 7);
    }

    /// Skip-arming path: target == prior_frontmost (self → self).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runs_closure_when_target_is_prior_frontmost() {
        let n = with_focus_suppressed(
            Some(42),
            Some(42), // self → self → skip
            "test.self_to_self",
            || async { 11 },
        )
        .await;
        assert_eq!(n, 11);
    }

    /// Arming path: target != prior_frontmost. The lease is armed,
    /// closure runs, lease drops on the way out (we can't directly
    /// observe the dispatcher state through the public API here —
    /// the focus_steal module's own tests cover the lease lifecycle —
    /// but we exercise the codepath to make sure it compiles and the
    /// 50ms sleep doesn't deadlock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn arms_lease_when_target_differs_from_prior() {
        let n = with_focus_suppressed(Some(123), Some(456), "test.armed", || async { 99 }).await;
        assert_eq!(n, 99);
    }

    /// `with_focus_suppressed_now` resolves prior_frontmost via the
    /// live NSWorkspace query — exercise the codepath end-to-end.
    /// In a unit-test context (no NSApp main thread) the query may
    /// return None; the closure must still run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn now_variant_runs_even_when_frontmost_is_none() {
        let n = with_focus_suppressed_now(Some(7), "test.now", || async { 5 }).await;
        assert_eq!(n, 5);
    }

    // ── Layer-2: synthetic-focus arming decision ────────────────────────────

    /// Synthetic focus requires all three of: a known target pid, a known
    /// prior frontmost that differs from the target (i.e. the target is a
    /// background app), and an element to focus. Missing any → skip.
    #[test]
    fn synthetic_arm_requires_target_prior_and_element() {
        // No target pid.
        assert!(!should_arm_synthetic(None, Some(1), Some(0x1)));
        // No prior frontmost → can't know the target is background.
        assert!(!should_arm_synthetic(Some(2), None, Some(0x1)));
        // Target already frontmost (self → self) — the issue's explicit skip.
        assert!(!should_arm_synthetic(Some(2), Some(2), Some(0x1)));
        // No element to synthesize focus on.
        assert!(!should_arm_synthetic(Some(2), Some(1), None));
        // Background target + element → arm.
        assert!(should_arm_synthetic(Some(2), Some(1), Some(0x1)));
    }

    // ── Layer-2: write planners ─────────────────────────────────────────────

    /// Arming writes AXFocused+AXMain on the window (when resolved) and
    /// AXFocused on the element, in window→element order (Swift
    /// `preventActivation`).
    #[test]
    fn arm_writes_cover_window_main_and_element() {
        let writes = plan_arm_writes(true);
        assert_eq!(
            writes,
            vec![
                FocusWrite::WindowFocused(true),
                FocusWrite::WindowMain(true),
                FocusWrite::ElementFocused(true),
            ]
        );
        // No window resolved → element only.
        let writes = plan_arm_writes(false);
        assert_eq!(writes, vec![FocusWrite::ElementFocused(true)]);
    }

    /// Restore writes cover exactly the priors that were readable on the
    /// way in, in the same window→element order. An unreadable prior is
    /// skipped — writing a bogus `false` would be worse than leaving the
    /// synthetic `true` (Swift `reenableActivation` semantics). When all
    /// priors were read (the normal case on Chrome/Safari) nothing
    /// synthetic is left behind.
    #[test]
    fn restore_writes_cover_exactly_the_readable_priors() {
        // All priors read → full restore with prior values, window first.
        let writes = plan_restore_writes(true, Some(false), Some(true), Some(false));
        assert_eq!(
            writes,
            vec![
                FocusWrite::WindowFocused(false),
                FocusWrite::WindowMain(true),
                FocusWrite::ElementFocused(false),
            ]
        );
        // Unreadable priors are skipped individually.
        let writes = plan_restore_writes(true, None, Some(true), None);
        assert_eq!(writes, vec![FocusWrite::WindowMain(true)]);
        // No window → element restore only.
        let writes = plan_restore_writes(false, None, None, Some(false));
        assert_eq!(writes, vec![FocusWrite::ElementFocused(false)]);
        // Nothing readable → nothing to restore.
        assert!(plan_restore_writes(true, None, None, None).is_empty());
    }

    // ── with_focus_suppressed_ax (element-aware variant) ────────────────────

    /// No element → behaves like `with_focus_suppressed` (no synthetic arm,
    /// no dereference of any pointer).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ax_variant_runs_closure_without_element() {
        let n = with_focus_suppressed_ax(Some(42), Some(7), None, "test.ax.no_element", || async {
            7
        })
        .await;
        assert_eq!(n, 7);
    }

    /// Element present but target already frontmost → synthetic focus is
    /// skipped. The bogus pointer proves the skip path never dereferences
    /// the element (a real deref of 0xDEADBEEF would crash the test
    /// process).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ax_variant_skips_synthetic_when_target_is_prior_frontmost() {
        let n = with_focus_suppressed_ax(
            Some(42),
            Some(42),
            Some(0xDEAD_BEEFusize),
            "test.ax.self_to_self",
            || async { 11 },
        )
        .await;
        assert_eq!(n, 11);
    }

    /// Same proof for the missing-prior path: element present but no prior
    /// frontmost → no arm, no dereference.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ax_variant_skips_synthetic_without_prior_frontmost() {
        let n = with_focus_suppressed_ax(
            Some(42),
            None,
            Some(0xDEAD_BEEFusize),
            "test.ax.no_prior",
            || async { 13 },
        )
        .await;
        assert_eq!(n, 13);
    }
}
