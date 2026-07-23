# ADR 0005: Milestone sequencing — Hermes drop-in first, Sky adapter second, guidance DB third

- Status: accepted (Douglas, 2026-07-23)
- Date: 2026-07-23

## Context

ADR 0002 committed to two consumer surfaces (own MCP vocabulary + Sky adapter)
and the motivating document adds two more workstreams (fused snapshot with
verification/settle, per-app guidance DB). Building all surfaces before first
dogfood delays the only feedback loop that matters: daily real use.

## Decision

- **v1 — Hermes drop-in replacement.** Same install path and wire surface as
  upstream cua-driver so Hermes' `computer_use` tool picks it up unchanged.
  Adds: fused screenshot+AX snapshots, snapshot→action→verify loop, settle
  detection, and ports of the FocusGuard layers 1+2 (AX enablement, synthetic
  focus) that upstream's Rust port dropped (documented gap in
  platform-macos/src/focus_guard.rs).
- **v2 — Sky adapter.** Codex CLI compatibility per ADR 0002, built against the
  surface spec reverse-engineered from the local Codex installation.
- **v3 — Per-app guidance database.** App-specific instructions + known
  escalation behavior injected into agent context.

## Consequences

- The core loop (snapshot/verify/settle/focus layers) must be designed
  surface-agnostic from day one, because v2 bolts a second protocol onto it;
  protocol quirks live in adapters, never in the core.
- v1's wire compatibility with upstream cua-driver constrains how aggressively
  the MCP vocabulary can shrink in v1 — the small-vocabulary redesign lands
  behind a new tool namespace or a v2 flag, not by breaking Hermes.
- Sky adapter work starts only after the reverse-engineered spec exists; if
  Codex's surface proves unstable, v2 scope is revisited with evidence.
- Guidance DB is deliberately last: entries should be harvested from v1
  dogfooding (which apps escalate, which need synthetic focus), not written
  from guesses.
