# cua-driver platform-macos capability map

Source: read-only analysis of the clone at `improved_cua/cua` (subagent report,
2026-07-23; all file:line citations verified against the clone). Crate root:
`libs/cua-driver/rust/crates/platform-macos/src/` (~27k lines, 60 files).
Layout: `input/` (skylight, mouse, keyboard, interactive, ax_actions),
`ax/` (bindings, tree, cache), `capture.rs` + `video_sckit.rs`, `windows.rs`,
`apps/`, `permissions/`, `focus_steal.rs`, `focus_guard.rs`,
`window_change_detector.rs`, `tools/` (33 MCP tools), `cursor/`, `browser/`, `pip/`.

## 1. Input injection paths and the ladder

**Paths implemented:**
- **AX actions** (`input/ax_actions.rs`): `AXUIElementPerformAction`
  (AXPress/AXShowMenu/AXPick/AXConfirm/AXCancel/AXOpen, map at ax_actions.rs:17-27),
  `AXFocused=true` (ax_actions.rs:30), `AXValue` write (ax_actions.rs:42), numeric
  `AXValue` for sliders (ax/bindings.rs:514). Pure RPC — the true zero-focus-steal path.
- **SkyLight `SLEventPostToPid`** (`input/skylight.rs:264-301`): dlopen'd once
  (skylight.rs:85-96), dlsym via RTLD_DEFAULT. Keyboard posts attach an
  `SLSEventAuthenticationMessage` built via `objc_msgSend` on
  `messageWithEventRecord:pid:version:` (skylight.rs:270-297); mouse posts skip the
  envelope (mouse.rs:917 — the envelope bypasses `cgAnnotatedSessionEventTap` which
  Chromium subscribes to).
- **Public `CGEventPostToPid`**: mouse fires **both** SkyLight AND public
  unconditionally ("belt+suspenders", mouse.rs:916-922); keyboard uses
  SkyLight-then-public fallback (keyboard.rs:269-275).
- **Event field stamping** via `SLEventSetIntegerValueField` +
  `CGEventSetWindowLocation` (skylight.rs:305-325): f0 gesture phase, f1 clickState,
  f3 button, f7 subtype, f40 target pid, f51/f91/f92 CGWindowID routing,
  f58 click-group ID (mouse.rs:881-923, full recipe comment 288-308).
- **Global HID tap** (`CGEventTapLocation::HID`): desktop-scope click/type/key
  (mouse.rs:45-88, keyboard.rs:130-265), foreground drag (mouse.rs:591-699).
  Warps the real cursor.
- **`SLPSPostEventRecordTo`**: 248-byte focus/defocus event records for
  `activate_without_raise` (skylight.rs:350-397) — **dead code, no callers
  anywhere in the crate**.
- **`SLPSSetFrontProcessWithOptions(psn, wid, 0x400=kCPSNoWindows)`**
  (skylight.rs:436-448 `set_front_process_persistently`; 478-509
  `with_foreground_hid_activation`; 519-560 `with_menu_shortcut_activation`).

**How the choice is made (per-call, not config):** `DeliveryMode::parse`
(tools/mod.rs:82-96) — `"foreground"` string → Foreground, everything else →
Background. Addressing (`element_index` vs `x,y`) picks AX vs pixel, orthogonally.

**Ladder for click** (tools/click.rs): element_index+window_id → AX action
(click.rs:319); x,y background single left-click → first an AX hit-test at the
point (`AXUIElementCopyElementAtPosition` + AXPress, click.rs:635-685) → else
CGEvent pixel path: `click_at_xy_chromium` (mouseMoved primer → off-screen (-1,-1)
primer down/up for Chromium's user-activation gate → target pairs,
mouse.rs:311-442) or `click_at_xy_with_window_local` when fg;
`delivery_mode:"foreground"` wraps the click in `with_foreground_assist`
(click.rs:778-787). Desktop scope (no pid) → global HID.

**Ladder for type_text** (tools/type_text.rs:680-828): terminal bundle-id →
CGEvent only (terminal.rs:25-36); AX `AXSelectedText` write + read-back verify →
CGEvent keystrokes + 2s drain poll with partial-count reporting; foreground rung
fronts+types+restores with 20/200ms settle. Detects AXWebArea ancestors and
distrusts AX echo verification (type_text.rs:341-343, 551-598), emitting
`escalation.recommended: "page"|"px"`.

**Verification model:** clicks are never driver-verified — tri-state
`effect: confirmed/unverifiable/suspected_noop` + structured
`escalation.recommended: px|foreground` (click.rs:482-504, 669-682;
scroll.rs:166-171 returns `background_unavailable` for Electron background
scroll). Typing is verified via AXValue read-back. The agent (not the driver)
climbs the ladder.

## 2. Capture — the big gap

- **Per-window stills: NOT ScreenCaptureKit.** `capture.rs:17-51` shells out to
  `screencapture -l <wid> -x -o /tmp/...png` (subprocess + temp file per capture).
  Header comment admits the ImageIO/CGWindowList path "would give lower overhead"
  (capture.rs:8-10). Full display: `screencapture -x` (capture.rs:64-93). No
  `SCScreenshotManager`, no `CGWindowListCreateImage` anywhere in the Rust code.
- **SCK is used only for full-display video**: `video_sckit.rs` (SCStream +
  SCRecordingOutput, H264/MP4, macOS 15+ only, video_sckit.rs:111-117).
- **Focus**: `screencapture -l` does not require focus/activation — background
  capture works, and occluded-window content is captured by WindowServer. Cost: a
  fork/exec + PNG encode per frame (and click.rs:595-610 does an *extra*
  screencapture per pixel-click just to sniff PNG IHDR bytes for Retina-scale
  detection).
- Window enumeration: `CGWindowListCopyWindowInfo` (windows.rs:51-57), layer-0
  filter, front-to-back z-index (windows.rs:143-203).

## 3. Accessibility

- **Full AX tree by PID**: yes — `walk_tree(pid, window_id, query)`
  (ax/tree.rs:151). Key background fix: unions `AXChildren` with `AXWindows` since
  AXChildren omits windows of non-frontmost apps (tree.rs:216-237). Filters to one
  window via private `_AXUIElementGetWindow` (bindings.rs:89-91, tree.rs:240-256).
- **Chromium/Electron enablement**: writes `AXManualAccessibility` (fallback
  `AXEnhancedUserInterface`) + one-time 0.5s settle per pid (tree.rs:196-214,
  bindings.rs:550-562).
- **Indexing/stability**: DFS-order indices assigned only to actionable nodes
  (has ≥1 action, tree.rs:387, 428-435); cached per (pid, window_id),
  wholesale-replaced on every `get_window_state` (cache.rs:92-100) — indices are
  **not stable across snapshots** by design (schema says re-snapshot each turn).
  Use-after-free guarded by retain-under-lock `RetainedElement` (cache.rs:24-53).
  Opaque `element_token` LRU registry lives in cua-driver-core (click.rs:261-285)
  with stale-token errors.
- **Caps**: 2000 elements / depth 25 defaults (tree.rs:25-35), 2s per-element
  messaging timeout (tree.rs:49), 20s tool-level deadline (get_window_state.rs:175).
- **AX actions**: press/show_menu/pick/confirm/cancel/open, AXFocused, AXValue
  string+number writes, AXSelectedText insert, AXIncrement/AXDecrement stepping
  (set_value.rs:249+).
- **AX diffs**: **none.** No tree diffing exists. `window_change_detector.rs`
  diffs only the CGWindowList window-ID set + frontmost pid around an action
  (window_change_detector.rs:192-297).

## 4. Focus management

- **Reactive suppressor (the SyntheticAppFocusEnforcer analog)**: `focus_steal.rs`
  — NSWorkspace `didActivateApplicationNotification` observer on a private serial
  NSOperationQueue (focus_steal.rs:388-430); targeted + wildcard suppression
  entries, 5s monotonic deadline, 1s janitor (focus_steal.rs:72-77, 312-376);
  restore via `activateWithOptions(0)` (focus_steal.rs:465-471).
- **focus_guard.rs** wraps each AX action with a targeted lease + 50ms settle
  (focus_guard.rs:76-121). **Explicitly only layer 3 of Swift's FocusGuard —
  layers 1+2 (AX enablement + synthetic AXFocused/AXMain write/restore before
  actions) are NOT ported** (focus_guard.rs:29-34). Note: AX enablement does exist
  in the tree walker (tree.rs:196-214) — the missing piece at action-dispatch time
  is the synthetic AXFocused/AXMain write + restore (Swift layer 2).
- **window_change_detector** arms a wildcard lease spanning snapshot→detect (~1s
  poll, window_change_detector.rs:152-155).
- **Launch**: NSWorkspace `activates=false` + `oapp` AppleEvent
  (apps/nsworkspace.rs:186-232), wildcard→targeted lease upgrade with
  500ms/2500ms windows, then a 5×200ms "belt-and-braces" demote loop if the target
  self-activates (launch_app.rs:242-320).
- **Foreground delivery**: `with_menu_shortcut_activation` — front target via
  SLPSSetFrontProcessWithOptions → run action → restore previous PSN; claimed
  "<1ms" (skylight.rs:511-560). `with_foreground_hid_activation` adds 40ms settles
  and *fails* rather than posting unrouted when SPIs are missing
  (skylight.rs:478-509). `set_front_process_persistently` backs `bring_to_front`
  (bring_to_front.rs:88-104) and interactive PersistentForeground sessions
  (interactive.rs:389-403).
- **Verification of fronting**: only the SPI's return code —
  `with_foreground_assist` returns `Ok(bool fronted)` and tools honestly downgrade
  the reported `path` (`cgevent_fg`→`cgevent`, click.rs:800-809). No read-back that
  WindowServer actually swapped frontmost before the action fires.
- PSN resolution: CGSMainConnectionID → SLSGetWindowOwner → SLSGetConnectionPSN,
  fallback `GetProcessForPID` (skylight.rs:404-426).

## 5. Window/app discovery & binding

- Apps: `NSWorkspace.runningApplications` filtered to activationPolicy Regular
  (apps/mod.rs:41-75); installed-app scan of /Applications roots (apps/mod.rs:373+).
  Launch by bundle-id (Cryptex-safe, keeps LaunchServices NSURL, apps/mod.rs:90-104,
  249-263) or display name (apps/mod.rs:276-301).
- Windows: CGWindowList (windows.rs), `resolve_main_window_id` = topmost on-screen
  else largest (windows.rs:245-264). AX-window ↔ CGWindowID binding via
  `_AXUIElementGetWindow` (bindings.rs:570-578). Binding target for all tools =
  (pid, window_id) pair; session/cursor registry keyed separately.

## 6. Permissions

- Probes: `AXIsProcessTrusted` + `CGPreflightScreenCaptureAccess` only
  (permissions/status.rs:44-67). Requests: `AXIsProcessTrustedWithOptions` +
  `CGRequestScreenCaptureAccess` (status.rs:71-91).
- **Only 2 grants tracked — no Input Monitoring handling anywhere** (not needed:
  they never install an event tap; all posting is CGEventPostToPid/SkyLight which
  is gated by Accessibility).
- Startup gate: CLI banner or native NSPanel (permissions/panel.rs), opens System
  Settings deep-links, polls 1Hz (gate.rs:335-466).
- **TCC per-process cache workaround**: `execvp` self re-exec every 25 stalled
  polls (~25s), with env-var state carry-over so re-execs poll silently and the
  10min deadline is cumulative (gate.rs:546-742).
- Tahoe wrinkle: `SCShareableContent::get()` can raise a *separate* direct-capture
  consent on macOS 26; probed only on explicit grant flows, reported as
  `direct_capture_status` (check_permissions.rs:201-218, health_report.rs:207).

## 7. Known fragility

- **Hardcoded offsets**: `extract_event_record` probes raw offsets 24/32/16 into
  `__CGEvent` and takes the first non-null pointer (skylight.rs:238-253) — could
  read garbage on a future OS. The 248-byte `SLPSPostEventRecordTo` record layout
  is fully magic (bytes 0x04=0xF8, 0x08=0x0D, wid at 0x3C, focus flag at 0x8A;
  skylight.rs:378-394) — but currently dead code.
- **Version gating is symbol-presence only**: no OS version checks anywhere (grep:
  zero hits for NSProcessInfo/version APIs). The one real gate:
  `class_respondsToSelector` for `messageWithEventRecord:pid:version:` (macOS 15+,
  #1503, skylight.rs:222-234, 284) — on macOS 14 the auth envelope is silently
  skipped and Chromium keys may be dropped. Auth message `version` argument
  hardcoded to 0 (skylight.rs:288). SCK video hard-requires macOS 15
  (video_sckit.rs:111-117). One macOS-26-specific fix noted (u64 vs i64 msg_send
  panic, pip/mod.rs:347-349).
- **Silent no-op paths**: every dlsym failure collapses to `Option::None` →
  `post_to_pid` returns false → fallback to public API *without logging*
  (skylight.rs:264-268, keyboard.rs:269-275). Mouse "belt+suspenders" ignores both
  post results unconditionally (mouse.rs:916-922). AX action errors are mapped to
  bail, but `focus_element` swallows errors by design (ax_actions.rs:32-38).
  `dlopen` failure at skylight.rs:90 is unchecked.
- **Sleeps as synchronization**: the input pipeline is paced by ~30 hardcoded
  `thread::sleep`s (12ms primer, 28ms down→up, 80ms pairs, 100ms gesture settle in
  mouse.rs; 8ms inter-key in keyboard.rs; 40ms front settles in skylight.rs:500-502)
  — no event-ack mechanism; timing regressions on loaded systems degrade silently.
- **Per-click screencapture**: pixel clicks run an extra `screencapture`
  subprocess to detect Retina scale from raw PNG bytes (click.rs:593-610).
- **WindowServer restore races**: `with_menu_shortcut_activation` restores the old
  front app immediately after posting; NSMenu works only because the event is
  already enqueued (skylight.rs:514-516) — inherently racy vs. slow run loops.
- `window_change_detector` adds a fixed ~50ms–1s poll after *every* action tool
  call (window_change_detector.rs:152-155) — latency cost, and detection is
  best-effort (its own comment: the suppressor usually restores before the poll
  observes, window_change_detector.rs:71-74).
