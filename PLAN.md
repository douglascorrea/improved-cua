# improved-cua — Plan

Context: `CODEX_COMPUTER_USE_VS_CUA_DRIVER.md` (motivation), `CONTEXT.md`
(glossary), `ARCHITECTURE.md` (design + gap list), `docs/adr/0001–0005`
(decisions), `docs/sky-surface-spec.md` (v2 spec).

## Phase 0 — fork mechanics (ADR 0001)

1. `gh repo fork trycua/cua` (or push this clone to a new `improved-cua` repo
   preserving history); add `upstream` remote.
2. One deletion commit per the ARCHITECTURE.md cut list; prune CI to macOS-only;
   prune pnpm/uv/nix workspace references to surviving paths.
3. Verify: `cargo build` + `cargo test` green in `libs/cua-driver/rust` on this
   Mac; install the built binary over `~/.local/bin/cua-driver` and confirm
   Hermes `computer_use` still works (drop-in contract).

## Phase 1 — core loop (v1, gaps G1–G6)

Ordered by dependency:

1. **G1 SCK stills** — `SCScreenshotManager`/single-frame SCStream with
   `SCWindow` filter replaces the `screencapture` subprocess in `capture.rs`.
   Verify: capture an occluded Safari window while another app is frontmost;
   assert pixels belong to the target and frontmost never changed.
2. **G3 FocusGuard layers 1+2** — port AX enablement (`AXManualAccessibility`,
   `AXEnhancedUserInterface`) and synthetic focus (`AXFocused`/`AXMain` write +
   restore) per the Swift reference comments in `focus_guard.rs`. Verify:
   background AX actions on Chrome + Safari stop producing reflex activations
   (sentinel test).
3. **G2 keyboard delivery** — make the SkyLight authenticated keyboard route
   the default; NSMenu activation becomes the Tier-2 path; expose
   `delivery_mode` on `press_key`/`hotkey` (closes upstream #2079-class gap).
   Verify: type into a background Chrome tab and a background native app with
   a foreground sentinel asserting no focus change.
4. **G4 verify engine** — mandatory post-action read-back (AX state and/or
   frame diff) on all mutating calls; typed outcomes
   `landed | escalated | undeliverable`; verified-failure triggers Tier 2
   (ADR 0004). Verify: synthetic no-op targets produce `undeliverable`, never
   false success.
5. **G5 settle engine** — AXObserver + SCK stream diff with monotonic deadline;
   bounded poll as fallback. Verify against `test-apps/` thrash apps: equal-or-
   better latency than the fixed poll, no premature snapshots.
6. **G6 fused snapshot + diffs** — one call: SCK still + AX tree +
   diff-chain-stable element indexes; per-target tree history; diff mode
   default-on. Verify: token size of a repeated snapshot on a static app drops
   to ~diff size.
7. **G9 version matrix hardening** — replace the `__CGEvent` offset probe and
   unchecked `dlopen` with checked, version-gated resolution; log every
   fallback; `health_report` emits the version×feature matrix. Verify: matrix
   output matches reality on macOS 26.5.2 (this Mac) and CI runners.

Then: dogfood as the daily Hermes driver for 1–2 weeks; log every Tier-2
escalation and every `undeliverable` (this log is the v3 guidance DB seed).

## Phase 2 — Codex compatibility (v2, ADR 0002)

1. Sky adapter: MCP server speaking the 11-method surface in
   `docs/sky-surface-spec.md` against the core. New core work: `select_text`,
   named-AX-action dispatch, app-scoped target resolution.
2. Point a Codex CLI test profile at our adapter instead of the bundled
   `SkyComputerUseClient`; run a fixed task battery (Safari, Chrome, Notes,
   Slack) and compare completion vs. the stock plugin.
3. G7 investigation (trusted background Chromium clicks) with the upstream
   #2285 oracle: fully occluded target + continuous foreground sentinel.
4. Curated small-vocabulary surface for our own MCP (informed by what the Sky
   surface proves is sufficient).

## Phase 3 — guidance + polish (v3)

1. Per-app guidance DB from v1 dogfood logs (escalation-prone apps, AX quirks,
   preferred windows).
2. Policy layer seeded from Codex's shipped confirmations tiers
   (`docs/sky-surface-spec.md` §Behavioral contract).
3. Per-session cursor overlays (upstream #1800) if still wanted.

## Standing work

- Track `upstream` for cua-driver fixes; cherry-pick into surviving paths.
- Consider PRing G1–G5-class portable fixes back upstream to shrink our merge
  surface.
- Re-capture the Sky spec when the Codex plugin version changes
  (current: 1.0.1000451).
- Keep `health_report` reporting the resolved/unresolved SkyLight symbol set on
  every macOS update.
