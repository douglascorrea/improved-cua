# improved-cua — Architecture

macOS-only, background-first computer-control driver. Fork of `trycua/cua`
(see `docs/adr/0001`), trimmed to the cua-driver macOS path. Terms per `CONTEXT.md`.

## Product definition

The user keeps the foreground, the pointer, and the keyboard. The agent acts on
other applications through the driver, which delivers actions in the background
whenever physically possible, verifies they landed, and is honest when they did
not (ADR 0004). Consumers: Hermes `computer_use` (v1, drop-in replacement for
upstream cua-driver), Codex CLI via a Sky-compatible MCP adapter (v2, ADR 0002).

## What upstream already provides (verified against source)

- Rust workspace (`libs/cua-driver/rust`, 12 crates). Canonical typed contract in
  `cua-driver-contract` (14 canonical tools: 4 session + 10 desktop); ~29
  platform tools beyond the contract slice.
- Transports: MCP (streamable + stdio proxy) and a daemon Unix socket with
  line-delimited JSON at `~/Library/Caches/cua-driver/cua-driver.sock`.
- platform-macos (~27k lines): SkyLight FFI with lazy dlsym + fallback
  (`input/skylight.rs`: `SLEventPostToPid`, `SLPSPostEventRecordTo`,
  `SLEventAuthenticationMessage`, `SLPSSetFrontProcessWithOptions`,
  `CGEventSetWindowLocation`); AX tree/cache/actions; focus-steal preventer
  (`focus_steal.rs`, 4-layer: closure/RAII lease, 5s deadline, 1s janitor);
  FocusGuard reactive layer (`focus_guard.rs`); window change detector;
  permissions module; cursor overlay; browser CDP integration with honest
  trust levels (`browser_input_trust_unavailable`); non-activating launch
  (`activates = false`).

## Gap list (the fork's reason to exist)

G1. **Still capture goes through the `screencapture` subprocess.** SCK is only
    wired for video (`video_sckit.rs`). Per-window stills need
    `SCScreenshotManager` / SCStream single-frame against an `SCWindow` filter:
    faster, no subprocess, works on occluded windows.
G2. **Keyboard delivery on macOS routes through NSMenu activation.**
    `press_key`/`hotkey` lack `delivery_mode` and use
    `with_menu_shortcut_activation` (upstream issue #2079). The SkyLight
    authenticated keyboard path (auth message, macOS 14+) exists in
    `skylight.rs` but is not the default route. Result: typing can steal focus
    today.
G3. **FocusGuard's synthetic-focus layer was dropped in the Rust port.**
    AX enablement (`AXManualAccessibility`/`AXEnhancedUserInterface`) exists in
    the tree walker (tree.rs:196-214), but the Swift reference's layer 2 —
    synthetic `AXFocused`/`AXMain` write + restore around each action — is
    explicitly unported (focus_guard.rs:29-34). This is the Chromium/Safari
    background-reliability gap: AX writes on unfocused targets trigger reflex
    self-activations that the reactive suppressor then has to catch.
G4. **Verification is agent-driven, not driver-owned.** Clicks are never
    driver-verified: tools return a tri-state `effect:
    confirmed/unverifiable/suspected_noop` plus `escalation.recommended`, and
    the *agent* climbs the ladder (click.rs:482-504). Typing is verified via
    AXValue read-back; fronting is verified only by SPI return code. ADR 0004
    deliberately diverges from upstream here: the driver owns read-back (AX
    state and/or frame diff) on every mutating call, and verified failure —
    not agent judgment — triggers Tier 2. Upstream's tri-state vocabulary is
    the seed, not the final design.
G5. **Settle is a fixed poll, and the input pipeline is paced by sleeps.**
    `window_change_detector` adds ~50ms–1s after every action; ~30 hardcoded
    `thread::sleep`s pace mouse/keyboard recipes with no event-ack mechanism
    (mouse.rs, keyboard.rs), so loaded systems degrade silently. Replace with
    event-driven settle: AXObserver notifications + SCK frame-stream diff with
    a monotonic deadline; bounded poll as fallback. Reference behavior: the
    Sky runtime waits ~1s post-action, extending to ~5s while loading
    indicators/state changes are visible (`docs/sky-surface-spec.md`).
G6. **Snapshot fusion is manual, and there are no AX diffs.** Docs describe a
    two-snapshot pattern; the agent correlates screenshot and AX itself.
    Element indexes are DFS-order over actionable nodes, cached per
    (pid, window_id) and wholesale-replaced every snapshot — unstable across
    snapshots by design (cache.rs:92-100). v1 ships one snapshot call
    returning screenshot + AX tree + element indexes together, plus an **AX
    tree diff mode** (added/removed/changed vs. previous snapshot) with
    diff-chain-stable indexing — confirmed as a core Codex token-efficiency
    feature in the shipped Sky spec (`docs/sky-surface-spec.md`). Per-target
    tree history lives in the core so both adapters get diffs.
G7. **Trusted background Chromium clicks** (upstream issue #2285): CDP trusted
    input activates the target on macOS. The native recipe is already deep —
    `click_at_xy_chromium` primes Chromium's user-activation gate with an
    off-screen (-1,-1) down/up before the target pairs, and mouse posts skip
    the auth envelope because it would bypass `cgAnnotatedSessionEventTap`
    (mouse.rs:288-442, 881-923) — but posting is fire-and-forget
    "belt+suspenders" (SkyLight + public, results ignored). The open problem
    is *trusted* delivery + confirmation. Candidate routes: refine the native
    recipe with delivery confirmation, AXPress via synthetic focus (G3), or
    the unwired `SLPSPostEventRecordTo` focus-without-raise primitive. Must
    keep upstream's honesty oracle: never relabel synthetic as trusted, never
    hide a transient steal.
G8. **Tool surface too large for model reliability** (33 platform tools in
    platform-macos + contract slice). v1 keeps wire compat for Hermes; the
    small-vocabulary redesign lands as a curated surface alongside, per
    ADR 0005.
G9. **No OS-version feature matrix.** Gating is symbol-presence only (zero OS
    version checks in the crate): on macOS 14 the keyboard auth envelope is
    silently skipped and Chromium keys may be dropped (skylight.rs:222-234);
    auth message `version` is hardcoded to 0; `extract_event_record` probes
    raw offsets 24/32/16 into `__CGEvent` and takes the first non-null
    pointer (skylight.rs:238-253); dlsym failures fall back without logging.
    For a driver whose core bet is version-fragile private APIs, a tested
    version×feature matrix is itself a capability.

## Target architecture

```
            ┌────────────────┐   ┌─────────────────┐
 Hermes ───▶│ MCP/UDS adapter │   │ Sky adapter (v2)│◀─── Codex CLI
 (computer_use)│ (upstream-  │   │ (@oai/sky MCP   │
            │  compatible)   │   │  surface)       │
            └───────┬────────┘   └────────┬────────┘
                    │    typed contract (cua-driver-contract, versioned)
            ┌───────▼─────────────────────▼────────┐
            │              CORE (protocol-free)     │
            │  session/target binding               │
            │  action dispatcher + delivery ladder  │
            │  verify engine (AX readback, framediff)│
            │  settle engine (AXObserver + SCK)     │
            │  snapshot fusion (SCK stills + AX)    │
            │  focus protection (preventer, guard,  │
            │    synthetic focus)                   │
            │  policy + per-app guidance (v3)       │
            └───────┬───────────────────────────────┘
                    │
            ┌───────▼────────┐
            │ platform-macos │  AX · SkyLight FFI · CGEvent · SCK · NSWorkspace
            └────────────────┘
```

Rules:
- Adapters own protocol quirks; the core never sees MCP- or Sky-shaped data.
- Delivery ladder (ADR 0004) lives in the dispatcher: AX action → CGEventPostToPid
  → SkyLight post → verified-failure → foreground escalation → typed
  `undeliverable`. Every hop records which rung delivered.
- Verify engine is mandatory on mutating calls, not an agent opt-in.
- The agent cursor overlay stays a separate, non-interactive window (existing
  cursor-overlay crate); per-session z+1 overlays per upstream issue #1800 are
  a v2 nicety.

## Cut list (ADR 0001 execution)

Delete: `libs/{lume,lumier,qemu-docker,kasm,xfce,cua-bench,fleet,cuabot,typescript}`,
`libs/cua-driver/wayland-helper`, Rust crates `platform-windows`, `platform-linux`,
non-macOS CI jobs, samples/blog that reference deleted products.
Keep: Rust crates `cua-driver`, `cua-driver-core`, `cua-driver-contract`,
`cua-driver-sdk`, `cursor-overlay`, `cua-driver-testkit`, `platform-macos`;
`contract/`, `docs/`, `tests/`, `scripts/`.
Defer (keep for now, cut if they rot): `libs/cua-driver/python`,
`libs/cua-driver/typescript`, `pip-preview`.

## Testing strategy

- **SkyLight contract tests per macOS version**: at startup, resolve all SPI
  symbols and report the resolved/unresolved set in `health_report`; CI runs the
  matrix on the macOS versions we support. A symbol disappearing is a loud
  failure, not a silent fallback.
- **Real-app delivery matrix**: Safari (WebKit), Chrome (Chromium), an Electron
  app, a Catalyst app, Finder/Notes (AppKit): background click/type/scroll with
  verification per app, plus a foreground sentinel continuously asserting the
  user's focus/z-order never changed (oracle borrowed from upstream #2285
  acceptance criteria).
- **Settle engine**: synthetic thrash apps in `test-apps/`; assert settle
  detection beats the fixed poll on both latency and correctness.
- **Fixtures**: extend `contract/fixtures` with snapshot/verify shapes so both
  adapters test against checked-in contracts.

## Risks and mitigations

- **SkyLight breaks on a future macOS** → dlsym fallback ladder (exists),
  symbol-contract health reporting (above), per-version feature flags; and
  replace the two most fragile spots first: the magic `__CGEvent` offset probe
  (skylight.rs:238-253) and the unchecked `dlopen` (skylight.rs:90).
- **SCK/TCC consent churn** (monthly re-prompts observed since Sequoia; macOS 26
  adds a separate direct-capture consent via `SCShareableContent.get()`) →
  stable signing identity (ADR 0003), single persistent service owning all
  captures, explicit `direct_capture_status` probing on grant flows.
- **Sky protocol drift** (v2) → fixtures + version tolerance in the adapter;
  re-spec from local installation when Codex updates.
- **Chromium trusted background clicks may be impossible** → then the honest
  contract stands: `dom_event` labeled synthetic, escalation labeled
  foreground. We ship honesty, not magic.
- **Upstream divergence** → surviving paths unchanged (ADR 0001); portable
  native fixes (G1–G5) are candidates to PR back upstream, reducing our own
  merge surface.

## Milestones (ADR 0005)

- **v1** — trim + G1–G6 + drop-in Hermes replacement; dogfood daily.
- **v2** — Sky adapter per the captured spec (`docs/sky-surface-spec.md`, 11
  methods incl. `select_text` + named-AX-action dispatch, both new); G7
  investigation; small-vocabulary curated surface.
- **v3** — per-app guidance DB harvested from v1 dogfooding; per-session
  overlays (#1800) if still wanted.
