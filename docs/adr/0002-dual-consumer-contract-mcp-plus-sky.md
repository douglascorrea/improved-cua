# ADR 0002: Dual consumer contract — own MCP vocabulary + literal Sky/@oai compatibility

- Status: accepted (Douglas, 2026-07-23)
- Date: 2026-07-23

## Context

The driver needs a consumer contract. Hermes' `computer_use` tool already talks to
cua-driver over MCP (contract crate pins MCP protocol 2025-06-18). The motivating
document suggests exposing "a small Sky-compatible plugin" so the integration
around the driver feels like Codex Computer Use.

Options:

- **A.** One MCP server, small Codex-like vocabulary, Hermes first-class; SDKs later.
- **B.** A + literal Sky/@oai protocol compatibility so Codex CLI itself can drive it.
- **C.** Keep the current generic surface; add settle/verify as optional helpers.

## Decision

Option B. The fork ships:

1. A single native driver service.
2. An MCP adapter exposing our own small, model-friendly vocabulary
   (snapshot/click/type/key/scroll/drag/wait/launch/focus-restore class),
   with screenshot+AX fused snapshots and verification built into mutating calls.
3. A Sky-compatible adapter that speaks the `@oai/sky` surface Codex Computer Use
   expects, so Codex CLI can bind to our driver instead of OpenAI's native service.

## Consequences

- We must reverse-engineer and track the `@oai/sky` surface from the locally
  installed Codex build; it is proprietary and can change without notice, so the
  adapter needs contract fixtures + version tolerance, and this is a standing
  maintenance cost we accepted explicitly.
- Our own MCP vocabulary stays clean because Sky quirks live in the adapter,
  not in the core.
- Two protocol surfaces means contract tests for both, run in CI against the
  checked-in fixtures (contract/ dir already exists upstream for this pattern).
- Hermes remains the primary consumer; Codex CLI compatibility is the
  differentiator that makes the fork useful outside Douglas's own toolchain.
