# CONTEXT.md — improved_cua ubiquitous language

Glossary only. No implementation decisions here (those live in docs/adr/).

## Actors

- **User** — the human at the Mac. Owns the foreground, the real pointer, and the
  keyboard. Their activity is never intentionally interrupted.
- **Agent** — the LLM-driven process (Hermes `computer_use`, Codex CLI via the Sky
  adapter, or any MCP client) issuing actions against the driver.
- **Driver** — the persistent native macOS service that executes actions and
  returns observations. The artifact this project builds.

## Core concepts

- **Target** — a specific application window the agent acts on, identified by
  pid + window id. Actions and observations are always scoped to a target.
- **Snapshot** — one observation of a target: fused screenshot (ScreenCaptureKit,
  captured without activation) + accessibility tree, with stable element
  indexes. The unit the agent reasons over.
- **Action** — a single mutation request against a target (click, type, key,
  scroll, drag, launch, …).
- **Verification** — the post-action readback that determines whether the action
  actually landed (AX state change, frame diff, or both). Drives escalation.
- **Settle** — the target UI reaching quiescence after an action (no layout
  thrash / repaints) such that the next snapshot is trustworthy.

## Delivery modes (see ADR 0004)

- **Background delivery (Tier 1)** — the action reaches the target without any
  change to the user's frontmost app, pointer, or keyboard focus.
- **Foreground escalation (Tier 2)** — a brief, always-reported foreground +
  restore cycle, used only after background delivery verifiably fails.
- **Undeliverable** — the typed result when a target defeats Tiers 1–2. Never a
  silent no-op.

## Surfaces

- **MCP adapter** — the driver's own small, model-friendly tool vocabulary
  served over MCP. Primary consumer: Hermes.
- **Sky adapter** — a compatibility surface speaking the `@oai/sky` protocol so
  Codex CLI can drive the driver. (See ADR 0002.)

## Platform terms

- **AX / AXUIElement** — Apple's accessibility API; semantic tree + direct
  actions (press, set value) deliverable to a background app's pid.
- **SCK** — ScreenCaptureKit; per-window capture without activation.
- **SkyLight** — private WindowServer-adjacent framework used for per-pid event
  posting when public paths are rejected. Fragile across macOS versions.
- **TCC identity** — the code-signing identity macOS attributes permission
  grants (Screen Recording, Accessibility, Input Monitoring) to. Stability of
  this identity across builds is a product requirement. (See ADR 0003.)
