# Sky surface spec (reverse-engineered)

Provenance: Codex CLI plugin `computer-use` v1.0.1000451, at
`~/.codex/plugins/cache/openai-bundled/computer-use/1.0.1000451/`.
Primary source: `.codex-plugin/computer-use-node-repl.md` (ships the full
TypeScript surface); transport: `.mcp.json` runs
`Codex Computer Use.app/Contents/SharedSupport/SkyComputerUseClient.app/Contents/MacOS/SkyComputerUseClient mcp`.
Captured 2026-07-23. Re-capture when the plugin version changes.

## Transport

- MCP server (stdio), binary embedded in the Codex Computer Use app bundle.
- Agent-facing consumption in Codex is via `node_repl` JS + a wrapper
  (`scripts/computer-use-client.mjs`, `setupComputerUseRuntime`), but the wire
  surface is the MCP tools below.

## Methods (11)

| Method | Args | Returns |
|---|---|---|
| `click` | `app`, `element_index?`, `x?`, `y?`, `mouse_button?`, `click_count?` | void |
| `drag` | `app`, `from_x`, `from_y`, `to_x`, `to_y` | void |
| `get_app_state` | `app`, `disableDiff?` | `AppState` |
| `list_apps` | — | `App[]` |
| `perform_secondary_action` | `app`, `element_index`, `action` | void |
| `press_key` | `app`, `key` (xdotool syntax: `"a"`, `"Return"`, `"super+c"`, `"KP_0"`) | void |
| `scroll` | `app`, `element_index`, `direction` (udlr, long or short form), `pages?` | void |
| `select_text` | `app`, `element_index`, `text`, `prefix?`, `suffix?`, `selection_type?` (`text`/`cursor_before`/`cursor_after`) | void |
| `set_value` | `app`, `element_index`, `value` | void |
| `type_text` | `app`, `text` | void |

```ts
type App = { id: string; displayName?: string; lastUsedDate?: string;
             useCount?: number; isRunning?: boolean };
type AppState = { app: string; screenshot: { url: string } | null; text: string };
```

## Behavioral contract (from the shipped instructions)

- **Targeting is per-app**, not per-window: `app` = display name | full path |
  bundle id. `press_key`/`type_text` are app-targeted and cannot fire global
  shortcuts.
- **`get_app_state` fuses screenshot + AX text** in one call; screenshot is a
  `file://` URL. It **transparently launches the app in the background** if not
  running.
- **AX tree diffs by default**: after the first full tree, subsequent
  `get_app_state` calls return only added/removed/changed elements
  (`disableDiff: true` forces full). `element_index` values are re-derived from
  the latest tree; agents are told never to reuse stale indexes.
- **Automatic settle**: the runtime waits ~1s after an action before capturing
  state, extending up to ~5s when loading indicators or ongoing state changes
  are detected.
- **Fallback doctrine** (agent instructions): prefer element_index + AX actions;
  fall back to screenshots/coordinate clicks when AX is incomplete.
- **Confirmations policy** lives in agent instructions (hand-off / confirm /
  pre-approve / not-required tiers), not in the driver. Reusable as the seed
  for our own policy layer (v3 guidance).

## Mapping to cua-driver capabilities

| Sky | cua-driver today | Gap |
|---|---|---|
| click / drag / scroll / press_key / type_text / set_value / list_apps | direct equivalents | none |
| perform_secondary_action | AX action dispatch | expose "invoke named AX action" verb |
| select_text | — | new: AX selected-text ops (prefix/suffix disambiguation, cursor placement) |
| get_app_state fusion | two-snapshot pattern (manual) | single fused call (G6) |
| AX tree diffs | — | new: tree diff engine with stable indexing |
| auto settle ~1s→5s loading-aware | fixed 50ms–1s poll | settle engine (G5) |
| transparent background launch | `launch_app` separate, `activates=false` | fold into state-fetch path |

## Packaging model (reference for our own service, ADR 0003)

- `Codex Computer Use.app` — the native service, executable `SkyComputerUseService`,
  bundle id `com.openai.sky.CUAService`, arm64, hardened runtime.
- `SkyComputerUseClient.app` (in `SharedSupport/`) — embedded CLI that speaks MCP,
  bundle id `com.openai.sky.CUAService.cli`. Both signed in the same session.
- `CUALockScreenGuardian.app` (in `SharedSupport/`) — a separate component for
  lock-screen scenarios; scope unknown, investigate only if v2 needs it.
- No LaunchAgent installed: the client spawns/attaches the service on demand.
- Agent-facing skill file: `skills/computer-use/SKILL.md` in the plugin root
  (same content as the node-repl doc).

## Shipped per-app guidance (reference for our v3 guidance DB)

Inside the service bundle: `Package_ComputerUse.bundle/Contents/Resources/`:

- `AppInstructions/` — 7 tiny per-app files (3–14 lines of prose each):
  AppleMusic, Clock, Notion, Numbers, Slack, Spotify, iPhone Mirroring.
  Content = targeted behavioral warnings, e.g. Slack.md in full: "Slack enters
  typed text into the message composer when no text field is focused. Before
  pressing Return, make sure the intended text field is focused… If the AX
  text … is behaving unexpectedly, use screenshots as the source of truth."
- `SkysightSummarizer.md`, `SkysightMemoryInstructions.md` — prompts for their
  AX-tree summarization / memory layers.

Format takeaway for v3: guidance = a few lines of prose keyed by app, covering
input quirks, focus traps, and when to distrust AX. Harvested from dogfood
logs, not written up front — same shape, our content.

## Adapter implications (v2)

- Our Sky adapter is an MCP server speaking these 11 tools against our core.
- App-scoped targeting maps onto our pid+window target by resolving app →
  main/front window (guidance DB can pin preferred windows per app).
- Diff semantics require the core's snapshot engine to retain per-target tree
  history — build it into the fused snapshot (G6), not the adapter, so Hermes
  gets token-efficient diffs too.
- Version drift: pin fixtures to the plugin version above; re-run this spec
  capture on Codex updates.
