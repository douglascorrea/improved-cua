//! Continuous-foreground-sentinel acceptance test for FocusGuard layer 2
//! (issue #3, AC1 + AC2).
//!
//! For Chrome and for Safari, with the browser running in the BACKGROUND:
//!
//! 1. a real AX press (reload button) and a real AX set-value (address
//!    field), dispatched through the production seam
//!    (`focus_guard::with_focus_suppressed_ax` wrapping
//!    `input::ax_actions`), must produce **zero**
//!    `NSWorkspace.didActivateApplicationNotification` events for the
//!    target pid while a sentinel observer watches continuously — the
//!    synthetic-focus layer prevents the reflex activation, and the
//!    reactive lease never has anything to catch;
//! 2. every synthetic focus attribute (`AXFocused` on the element,
//!    `AXFocused`/`AXMain` on its window) is back at its prior value
//!    after each action — no residual synthetic focus left behind.
//!
//! ## Real-app test — skip-gated, never silently green
//!
//! Each leg skips (passing the suite) with a loud `SENTINEL SKIP` line
//! when a prerequisite is missing: Accessibility (TCC) trust for the
//! test runner's responsible process, or the browser not installed.
//! Per TESTING.md, an environment that can't prove the AC stays visible
//! instead of collapsing to a reduced green run.
//!
//! ## What this test touches on the machine
//!
//! HARD CONSTRAINT (user at the machine): no part of this harness may
//! activate an app. Every launch goes through
//! `apps::nsworkspace::open_urls_with_application`, which always sets
//! `NSWorkspaceOpenConfiguration.activates = false`, and nothing here
//! ever foregrounds a third app to "push" the target into the
//! background. If the target is frontmost when a leg needs it in the
//! background (e.g. the user is actively using Safari), the leg skips
//! loudly instead of stealing focus back.
//!
//! - Chrome: a SEPARATE throwaway instance (`--user-data-dir` under
//!    /tmp, `creates_new_instance`) on `about:blank`, launched without
//!    activation, terminated when the leg ends. The user's real Chrome
//!    session is never attached to.
//! - Safari: the user's real Safari. If it's already running we open one
//!   `about:blank` window/tab WITHOUT activating Safari, drive only
//!   that, and close what we opened (a stray about:blank tab may be
//!   left if window-vs-tab resolution is ambiguous). If we launched
//!   Safari ourselves we terminate it afterwards.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use block2::RcBlock;
use objc2_app_kit::{
    NSRunningApplication, NSWorkspace, NSWorkspaceApplicationKey,
    NSWorkspaceDidActivateApplicationNotification,
};
use objc2_foundation::{NSNotification, NSOperationQueue};

use crate::apps;
use crate::ax::bindings::*;
use crate::focus_guard;
use crate::input::ax_actions;
use crate::windows;

/// Serialize the two legs: both manipulate the frontmost app, so they
/// must not race each other (or double-background a target).
fn serial() -> &'static Mutex<()> {
    static SERIAL: OnceLock<Mutex<()>> = OnceLock::new();
    SERIAL.get_or_init(|| Mutex::new(()))
}

fn skip(reason: &str) {
    eprintln!("SENTINEL SKIP: {reason}");
}

// ── Sentinel observer ────────────────────────────────────────────────────────

/// Continuously counts `didActivateApplicationNotification` events for
/// one target pid from install until process end. Leaked on purpose (the
/// observer must outlive the test body; the process is a short-lived
/// test binary). Mirrors focus_steal.rs's observer shape: private serial
/// NSOperationQueue so delivery works without a main run loop.
struct Sentinel {
    target_pid: i32,
    hits: Arc<AtomicUsize>,
    all_pids: Arc<Mutex<Vec<i32>>>,
}

impl Sentinel {
    fn install(target_pid: i32) -> Self {
        let hits = Arc::new(AtomicUsize::new(0));
        let all_pids: Arc<Mutex<Vec<i32>>> = Arc::new(Mutex::new(Vec::new()));

        let hits_clone = Arc::clone(&hits);
        let pids_clone = Arc::clone(&all_pids);
        let block = RcBlock::new(move |note_ptr: std::ptr::NonNull<NSNotification>| {
            use objc2::msg_send;
            use objc2::runtime::AnyObject;
            let note = unsafe { note_ptr.as_ref() };
            let Some(info) = (unsafe { note.userInfo() }) else {
                return;
            };
            let app_ptr: *mut AnyObject =
                unsafe { msg_send![&*info, objectForKey: NSWorkspaceApplicationKey] };
            if app_ptr.is_null() {
                return;
            }
            let pid: i32 = unsafe { msg_send![app_ptr, processIdentifier] };
            if pid == target_pid {
                hits_clone.fetch_add(1, Ordering::SeqCst);
            }
            if let Ok(mut v) = pids_clone.lock() {
                v.push(pid);
            }
        });

        unsafe {
            let ws = NSWorkspace::sharedWorkspace();
            let center = ws.notificationCenter();
            let queue = NSOperationQueue::new();
            queue.setMaxConcurrentOperationCount(1);
            let token = center.addObserverForName_object_queue_usingBlock(
                Some(NSWorkspaceDidActivateApplicationNotification),
                None,
                Some(&queue),
                &block,
            );
            std::mem::forget(token);
            std::mem::forget(queue);
        }

        Sentinel {
            target_pid,
            hits,
            all_pids,
        }
    }

    fn count(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    /// Assert AC1 for this leg: zero activations of the target since
    /// install. Prints every observed activation for diagnosis.
    fn assert_zero_target_activations(&self, browser: &str) {
        let n = self.count();
        let observed = self
            .all_pids
            .lock()
            .map(|v| v.clone())
            .unwrap_or_default();
        eprintln!(
            "SENTINEL {browser}: didActivateApplicationNotification for target pid {} = {n} \
             (all observed activation pids during window: {observed:?})",
            self.target_pid
        );
        assert_eq!(
            n, 0,
            "{browser}: expected zero didActivateApplicationNotification for the background \
             target during AX press/set-value, observed {n}"
        );
    }
}

// ── AX helpers (raw bindings, +1 retains released by the caller) ─────────────

/// Depth-first search for an element matching `pred`. Returns a +1
/// retained ref. All non-matching intermediate refs are released.
unsafe fn dfs_find(root: AXUIElementRef, depth: usize, pred: &dyn Fn(AXUIElementRef) -> bool) -> Option<AXUIElementRef> {
    if depth == 0 {
        return None;
    }
    for child in copy_children(root) {
        if pred(child) {
            return Some(child); // caller owns the +1
        }
        if let Some(found) = dfs_find(child, depth - 1, pred) {
            core_foundation::base::CFRelease(child as _);
            return Some(found);
        }
        core_foundation::base::CFRelease(child as _);
    }
    None
}

/// Find the AX window element for `wid` under `app` (+1 retained).
unsafe fn find_ax_window(app: AXUIElementRef, wid: u32) -> Option<AXUIElementRef> {
    for w in copy_ax_windows(app) {
        let matches = ax_get_window_id(w) == Some(wid);
        if matches {
            return Some(w);
        }
        core_foundation::base::CFRelease(w as _);
    }
    None
}

/// The press target: a button whose title or description mentions
/// "reload" — harmless on `about:blank` (reloads a blank page), and the
/// app definitely processes the action (a real navigation), which is
/// what historically tripped the reflex activation.
unsafe fn find_reload_button(window: AXUIElementRef) -> Option<AXUIElementRef> {
    dfs_find(window, 20, &|el| {
        if copy_string_attr(el, "AXRole").as_deref() != Some("AXButton") {
            return false;
        }
        let title = copy_string_attr(el, "AXTitle").unwrap_or_default();
        let desc = copy_string_attr(el, "AXDescription").unwrap_or_default();
        title.to_lowercase().contains("reload") || desc.to_lowercase().contains("reload")
    })
}

/// The set-value target: the first text field in the window — on browser
/// chrome that's the address field.
unsafe fn find_text_field(window: AXUIElementRef) -> Option<AXUIElementRef> {
    dfs_find(window, 20, &|el| {
        copy_string_attr(el, "AXRole").as_deref() == Some("AXTextField")
    })
}

// ── App/frontmost helpers ────────────────────────────────────────────────────

fn running_pid_for_bundle(bundle_id: &str) -> Option<i32> {
    unsafe {
        let ws = NSWorkspace::sharedWorkspace();
        let running = ws.runningApplications();
        for index in 0..running.count() {
            let app = running.objectAtIndex(index);
            if let Some(bid) = app.bundleIdentifier() {
                // Case-insensitive: Safari reports `com.apple.Safari`.
                if bid.to_string().eq_ignore_ascii_case(bundle_id) {
                    return Some(app.processIdentifier());
                }
            }
        }
    }
    None
}

/// Confirm `target_pid` is in the background WITHOUT touching the
/// frontmost app. The old harness ran `open -b com.apple.finder` here —
/// an explicit focus steal, banned while the user is at the machine.
/// All launches in this harness are non-activating, so the target
/// should already be background; we only confirm. A short grace poll
/// absorbs post-launch frontmost transitions, then a frontmost target
/// is a loud skip (e.g. the user is actively using Safari) — never a
/// steal-back.
async fn confirm_background(target_pid: i32) -> bool {
    for _ in 0..10 {
        match apps::frontmost_pid() {
            Some(fp) if fp == target_pid => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Some(_) => return true,
            None => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
    apps::frontmost_pid() != Some(target_pid)
}

fn window_ids_for_pid(pid: i32) -> HashSet<u32> {
    windows::all_windows()
        .into_iter()
        .filter(|w| w.pid == pid)
        .map(|w| w.window_id)
        .collect()
}

async fn wait_for_window(pid: i32, tries: usize) -> Option<u32> {
    for _ in 0..tries {
        if let Ok(wid) = windows::resolve_main_window_id(pid) {
            return Some(wid);
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    None
}

/// Walk the tree until it materializes (non-empty) or the tries run out.
/// A fresh browser process can show a CGWindow before its AX server is
/// answering; the first walk then returns zero nodes. Chromium AX
/// enablement happens inside the walk, so retrying the walk is also the
/// enablement path.
async fn walk_until_ready(pid: i32, wid: u32, tries: usize) -> crate::ax::tree::TreeWalkResult {
    let mut result = crate::ax::tree::walk_tree(pid, Some(wid), None);
    for _ in 0..tries {
        if !result.nodes.is_empty() {
            return result;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        result = crate::ax::tree::walk_tree(pid, Some(wid), None);
    }
    result
}

// ── The shared leg ───────────────────────────────────────────────────────────

struct LegTargets {
    /// +1 retained: the window element of the press target.
    press_window: AXUIElementRef,
    /// +1 retained: the reload button.
    reload_button: AXUIElementRef,
    /// +1 retained: the address text field.
    text_field: AXUIElementRef,
}

impl Drop for LegTargets {
    fn drop(&mut self) {
        unsafe {
            core_foundation::base::CFRelease(self.press_window as _);
            core_foundation::base::CFRelease(self.reload_button as _);
            core_foundation::base::CFRelease(self.text_field as _);
        }
    }
}

/// Discover the leg's AX targets in `wid`. Returns None (caller skips)
/// when the window or either element can't be found. Partial finds are
/// released on the failure path.
unsafe fn discover_targets(pid: i32, wid: u32) -> Option<LegTargets> {
    let app = AXUIElementCreateApplication(pid);
    if app.is_null() {
        return None;
    }
    let window = find_ax_window(app, wid);
    let reload_button = window.and_then(|w| find_reload_button(w));
    let text_field = window.and_then(|w| find_text_field(w));
    core_foundation::base::CFRelease(app as _);
    match (window, reload_button, text_field) {
        (Some(w), Some(rb), Some(tf)) => Some(LegTargets {
            press_window: w,
            reload_button: rb,
            text_field: tf,
        }),
        (w, rb, tf) => {
            for el in [w, rb, tf].into_iter().flatten() {
                core_foundation::base::CFRelease(el as _);
            }
            None
        }
    }
}

/// Run one press + set-value round against `t` under the continuous
/// sentinel, asserting AC1 (zero activations) and AC2 (attributes
/// restored) for the leg.
async fn run_actions_and_assert(browser: &str, pid: i32, t: &LegTargets) {
    // Priors captured BEFORE the guarded actions (AC2 baseline).
    let prior_el_focused = unsafe { copy_bool_attr(t.reload_button, "AXFocused") };
    let prior_win_focused = unsafe { copy_bool_attr(t.press_window, "AXFocused") };
    let prior_win_main = unsafe { copy_bool_attr(t.press_window, "AXMain") };
    let prior_field_value =
        unsafe { copy_string_attr(t.text_field, "AXValue") }.unwrap_or_default();

    let sentinel = Sentinel::install(pid);
    // Let launch/backgrounding activations fully drain before watching.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // ── AX press through the production seam ──────────────────────────
    let press_ptr = t.reload_button as usize;
    let prior_front = apps::frontmost_pid();
    let press_result = focus_guard::with_focus_suppressed_ax(
        Some(pid),
        prior_front,
        Some(press_ptr),
        "sentinel.press",
        || async move { tokio::task::spawn_blocking(move || ax_actions::perform_ax_action(press_ptr, "press")).await },
    )
    .await;
    match press_result {
        Ok(Ok(())) => eprintln!("SENTINEL {browser}: AXPress dispatched"),
        other => panic!("{browser}: AXPress dispatch failed: {other:?}"),
    }
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    assert_focus_restored(browser, "after AXPress", t, prior_el_focused, prior_win_focused, prior_win_main);

    // ── AX set-value through the production seam ──────────────────────
    let field_ptr = t.text_field as usize;
    let prior_front = apps::frontmost_pid();
    let value_result = focus_guard::with_focus_suppressed_ax(
        Some(pid),
        prior_front,
        Some(field_ptr),
        "sentinel.set_value",
        || async move {
            tokio::task::spawn_blocking(move || ax_actions::set_ax_value(field_ptr, "cua-sentinel-test")).await
        },
    )
    .await;
    match value_result {
        Ok(Ok(())) => eprintln!("SENTINEL {browser}: AXValue write dispatched"),
        other => panic!("{browser}: AXValue write failed: {other:?}"),
    }
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    assert_focus_restored(browser, "after AXValue write", t, prior_el_focused, prior_win_focused, prior_win_main);

    // Restore the field's prior value (also through the guarded seam) so
    // we leave the browser as we found it.
    let restore_value = prior_field_value.clone();
    let _ = focus_guard::with_focus_suppressed_ax(
        Some(pid),
        apps::frontmost_pid(),
        Some(field_ptr),
        "sentinel.set_value.restore",
        || async move {
            tokio::task::spawn_blocking(move || ax_actions::set_ax_value(field_ptr, &restore_value)).await
        },
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let field_after = unsafe { copy_string_attr(t.text_field, "AXValue") }.unwrap_or_default();
    eprintln!("SENTINEL {browser}: address field restored to {field_after:?}");

    // AC1 — the whole point of the leg.
    sentinel.assert_zero_target_activations(browser);
}

fn assert_focus_restored(
    browser: &str,
    phase: &str,
    t: &LegTargets,
    prior_el: Option<bool>,
    prior_win_f: Option<bool>,
    prior_win_m: Option<bool>,
) {
    // Only priors that were readable are restorable (Swift semantics) —
    // assert exactly those.
    let checks = [
        ("element AXFocused", prior_el, unsafe { copy_bool_attr(t.reload_button, "AXFocused") }),
        ("window AXFocused", prior_win_f, unsafe { copy_bool_attr(t.press_window, "AXFocused") }),
        ("window AXMain", prior_win_m, unsafe { copy_bool_attr(t.press_window, "AXMain") }),
    ];
    for (name, prior, after) in checks {
        if let Some(p) = prior {
            eprintln!("SENTINEL {browser}: {name} {phase}: prior={p} after={after:?}");
            assert_eq!(
                after,
                Some(p),
                "{browser}: synthetic {name} not restored {phase} — residual synthetic focus left behind"
            );
        } else {
            eprintln!("SENTINEL {browser}: {name} prior unreadable {phase} — nothing to assert (Swift restore semantics)");
        }
    }
}

// ── Chrome leg (separate throwaway instance) ─────────────────────────────────

const CHROME_BUNDLE_ID: &str = "com.google.Chrome";
const CHROME_PROFILE: &str = "/tmp/cua-sentinel-chrome-profile";

/// Terminate-on-drop guard for the throwaway Chrome instance. We don't
/// hold a `Child` (LaunchServices owns the process), so termination goes
/// through `NSRunningApplication` — graceful first, forceful after a
/// short grace period.
struct OwnedApp {
    pid: i32,
}
impl Drop for OwnedApp {
    fn drop(&mut self) {
        unsafe {
            if let Some(app) =
                NSRunningApplication::runningApplicationWithProcessIdentifier(self.pid)
            {
                app.terminate();
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        unsafe {
            if let Some(app) =
                NSRunningApplication::runningApplicationWithProcessIdentifier(self.pid)
            {
                if !app.isTerminated() {
                    app.forceTerminate();
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sentinel_chrome_background_ax_actions_produce_zero_activations() {
    let _g = serial().lock().unwrap_or_else(|e| e.into_inner());

    if !unsafe { AXIsProcessTrusted() } {
        skip("test runner lacks Accessibility (TCC) trust — grant it to the \
              terminal/IDE running cargo and re-run");
        return;
    }
    if running_pid_for_bundle(CHROME_BUNDLE_ID).is_none()
        && !std::path::Path::new("/Applications/Google Chrome.app").exists()
    {
        skip("Google Chrome not installed at /Applications/Google Chrome.app");
        return;
    }

    // Fresh throwaway profile each run (deterministic chrome UI). Launch
    // WITHOUT activation (activates=false inside the driver's NSWorkspace
    // path) — the instance comes up behind the user's frontmost app and
    // never steals focus, so there is nothing to "push back".
    let _ = std::fs::remove_dir_all(CHROME_PROFILE);
    let launched = apps::nsworkspace::open_urls_with_application(
        &["about:blank".to_string()],
        CHROME_BUNDLE_ID,
        &apps::nsworkspace::OpenConfig {
            arguments: vec![
                format!("--user-data-dir={CHROME_PROFILE}"),
                "--no-first-run".to_string(),
                "--no-default-browser-check".to_string(),
                "--disable-session-crashed-bubble".to_string(),
                "--hide-crash-restore-bubble".to_string(),
            ],
            creates_new_instance: true,
            apple_event_bundle_id: Some(CHROME_BUNDLE_ID.to_string()),
            ..Default::default()
        },
    );
    let app = match launched {
        Ok(app) => app,
        Err(e) => {
            skip(&format!("non-activating Chrome launch failed: {e}"));
            return;
        }
    };
    let pid = unsafe { app.processIdentifier() };
    let _owned = OwnedApp { pid };
    eprintln!("SENTINEL chrome: launched throwaway instance pid={pid} (no activation)");

    // Wait for a window, then materialize the AX tree (enables Chromium AX).
    let Some(wid) = wait_for_window(pid, 60).await else {
        skip("throwaway Chrome showed no window within 15s");
        return;
    };
    let tree = walk_until_ready(pid, wid, 60).await;
    if tree.nodes.is_empty() {
        skip("throwaway Chrome's AX tree never materialized (60 tries over 15s)");
        return;
    }
    if !confirm_background(pid).await {
        skip("throwaway Chrome is frontmost — refusing to steal focus back; \
              re-run when it has not just been interacted with");
        return;
    }
    // Re-walk after backgrounding so the tree reflects the background state.
    let _ = walk_until_ready(pid, wid, 20).await;

    let targets = unsafe { discover_targets(pid, wid) };
    let Some(targets) = targets else {
        let tree = crate::ax::tree::walk_tree(pid, Some(wid), None);
        let preview: Vec<&str> = tree.tree_markdown.lines().take(80).collect();
        skip(&format!(
            "could not find reload button / address field in the throwaway Chrome window \
             (pid={pid} wid={wid}). AX tree dump ({} nodes, first 80 lines):\n{}",
            tree.nodes.len(),
            preview.join("\n")
        ));
        return;
    };

    run_actions_and_assert("chrome", pid, &targets).await;
    // OwnedApp::drop terminates the throwaway instance here.
}

// ── Safari leg (real Safari; owned window/tab) ───────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sentinel_safari_background_ax_actions_produce_zero_activations() {
    let _g = serial().lock().unwrap_or_else(|e| e.into_inner());

    if !unsafe { AXIsProcessTrusted() } {
        skip("test runner lacks Accessibility (TCC) trust — grant it to the \
              terminal/IDE running cargo and re-run");
        return;
    }
    if !std::path::Path::new("/Applications/Safari.app").exists() {
        skip("Safari not installed");
        return;
    }

    let was_running = running_pid_for_bundle("com.apple.safari").is_some();
    let before_windows = if was_running {
        running_pid_for_bundle("com.apple.safari")
            .map(window_ids_for_pid)
            .unwrap_or_default()
    } else {
        HashSet::new()
    };

    // Open about:blank WITHOUT activation (activates=false in the
    // driver's NSWorkspace path) — launches Safari behind the user's
    // frontmost app if needed, else opens a new window/tab in the
    // running instance without bringing it forward.
    let launched = apps::nsworkspace::open_urls_with_application(
        &["about:blank".to_string()],
        "com.apple.Safari",
        &apps::nsworkspace::OpenConfig {
            apple_event_bundle_id: Some("com.apple.Safari".to_string()),
            ..Default::default()
        },
    );
    if let Err(e) = launched {
        skip(&format!("non-activating Safari open of about:blank failed: {e}"));
        return;
    }

    // Resolve the Safari pid (launch case may take a moment).
    let mut pid = running_pid_for_bundle("com.apple.safari");
    for _ in 0..40 {
        if pid.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        pid = running_pid_for_bundle("com.apple.safari");
    }
    let Some(pid) = pid else {
        skip("Safari did not appear after `open`");
        return;
    };
    eprintln!("SENTINEL safari: pid={pid} was_running={was_running}");

    // Identify OUR window: a new CGWindowID, else the main window (tab case).
    let mut our_window: Option<u32> = None;
    for _ in 0..20 {
        let now = window_ids_for_pid(pid);
        if let Some(new_id) = now.difference(&before_windows).next() {
            our_window = Some(*new_id);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let we_own_window = our_window.is_some();
    let wid = match our_window.or_else(|| windows::resolve_main_window_id(pid).ok()) {
        Some(w) => w,
        None => {
            skip("Safari has no window to target");
            return;
        }
    };
    eprintln!("SENTINEL safari: targeting window_id={wid} (own window: {we_own_window})");

    let tree = walk_until_ready(pid, wid, 40).await;
    if tree.nodes.is_empty() {
        skip("Safari's AX tree never materialized");
        return;
    }
    if !confirm_background(pid).await {
        skip("Safari is frontmost (the user is likely using it) — refusing to \
              steal focus back; re-run when Safari is in the background");
        return;
    }
    let _ = walk_until_ready(pid, wid, 20).await;

    let targets = unsafe { discover_targets(pid, wid) };
    let Some(targets) = targets else {
        skip("could not find reload button / address field in the Safari window");
        return;
    };

    run_actions_and_assert("safari", pid, &targets).await;

    // Cleanup: close the window we opened (only when it's clearly ours),
    // and terminate Safari only when this test launched it.
    if we_own_window {
        unsafe {
            if let Some(close_btn) = copy_element_attr(targets.press_window, "AXCloseButton") {
                let _ = perform_action(close_btn, "AXPress");
                core_foundation::base::CFRelease(close_btn as _);
            }
        }
    } else {
        eprintln!("SENTINEL safari: about:blank opened as a tab in an existing window — left in place");
    }
    if !was_running {
        unsafe {
            if let Some(app) =
                NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            {
                app.terminate();
            }
        }
    }
}
