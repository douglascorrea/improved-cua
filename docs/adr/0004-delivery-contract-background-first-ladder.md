# ADR 0004: Delivery contract — background-first ladder, verified foreground escalation, no VM tier

- Status: accepted (by Linus default; awaiting Douglas ratification)
- Date: 2026-07-23

## Context

Universal background pixel/keyboard delivery is impossible on macOS: some apps
(certain Chromium/Electron builds, Catalyst, games, canvas editors) reject or
mishandle public event delivery. The motivating document lists exactly three
realistic options at that boundary: private SkyLight APIs, brief
foreground-and-restore, or a separate user session/VM. Codex Computer Use ships
the same shape (its binary contains `SyntheticAppFocusEnforcer`).

## Decision

Three-tier ladder; tier 3 out of scope.

- **Tier 1 — background (default):** AX actions / AX set-value, CGEventPostToPid,
  SkyLight event posting for stubborn apps, ScreenCaptureKit per-window capture.
  Never changes focus.
- **Tier 2 — verified foreground escalation (reaction, never prediction):** only
  after Tier 1 *verifiably* fails, briefly foreground the target, act, restore
  the previous frontmost app. Always reported to the agent as
  `delivery: foreground` and logged.
- **Tier 3 — separate user session / VM:** out of scope. Upstream's lume/qemu
  stack (deleted in the trim) already serves that use case.

"Flawless" is defined as flawless *honesty about delivery mode*, not flawless
universal background: the driver never reports an action as landed when it did
not, and never silently steals focus.

## Consequences

- Every mutating call needs a verification path, because Tier 2 triggers on
  verified failure, not on heuristics. This makes the snapshot→action→verify
  loop load-bearing architecture, not a UX nicety.
- The agent always knows when the user saw a flicker, so app-specific guidance
  can warn ("this app usually escalates").
- SkyLight private-API fragility (per-macOS-version symbol/behavior drift) is
  contained in Tier 1 and must be covered by contract tests per macOS version.
- Apps that defeat Tiers 1–2 produce an explicit, typed "undeliverable" result
  rather than a silent no-op.
