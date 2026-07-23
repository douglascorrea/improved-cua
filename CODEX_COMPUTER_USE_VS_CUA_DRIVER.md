# Codex Computer Use vs. `cua-driver` on macOS

Short answer: Codex Computer Use is not powered by one magical Apple API. It feels better because OpenAI built the entire stack—native controller, app discovery, perception, focus management, verification, policy, and agent instructions—as one product.

### How background control works

On macOS, several mechanisms are combined:

1. **Window capture:** ScreenCaptureKit can capture a specific window without making it frontmost. [Apple ScreenCaptureKit](https://developer.apple.com/documentation/screencapturekit/sccontentfilter/init%28desktopindependentwindow%3A%29)

2. **Semantic interaction:** Accessibility APIs expose another app’s UI tree by PID. Buttons can be pressed and values changed directly, without moving the physical pointer or focusing the app. [AXUIElementPerformAction](https://developer.apple.com/documentation/applicationservices/1462091-axuielementperformaction)

3. **Targeted input:** `CGEvent.postToPid` can send events to a specific process instead of the global HID stream. This is public, but apps such as Chromium, Electron, Catalyst, games, and canvas-based editors may reject or mishandle it. [Apple CGEvent](https://developer.apple.com/documentation/coregraphics/cgevent)

4. **Background launching:** `NSWorkspace.OpenConfiguration.activates = false` launches an app without intentionally bringing it forward. [Apple NSWorkspace configuration](https://developer.apple.com/documentation/appkit/nsworkspace/openconfiguration/activates)

5. **Focus guards:** If the target app activates itself anyway, the controller can immediately restore the previous frontmost app.

That is why your keyboard and real pointer remain yours. Most actions are performed against accessibility objects or routed to a target process; the visible “agent pointer” can be a separate overlay.

### What makes Codex feel better

I inspected the Computer Use installation on your Mac. Its proprietary native service and `@oai/sky` client provide more than raw clicking:

- A signed, persistent background service with stable macOS permission identity.
- Screenshot and accessibility tree returned together.
- Stable element indexes and accessibility-tree diffs.
- Automatic app discovery and background launch.
- Automatic UI-settle waits after actions.
- App-specific instructions inserted into the agent context.
- Per-app approvals and safety policy.
- A very small, model-friendly action vocabulary.
- Focus-steal prevention and window-specific targeting.

This means Codex spends less reasoning on plumbing and is less likely to target stale elements, act before repainting finishes, confuse windows, or believe a silent failure.

The exact event implementation is closed-source. The installed binary contains components named `WindowServerSPI`, `AccessibilitySPI`, `SyntheticAppFocusEnforcer`, and `SystemFocusStealPreventer`, which strongly suggests deeper WindowServer integration, but it does not prove that every click uses a particular private API.

OpenAI officially documents background operation as a macOS-specific capability. [OpenAI Computer Use documentation](https://learn.chatgpt.com/docs/computer-use)

### What about `cua-driver`?

Interestingly, `cua-driver` already uses the same general architecture—and goes quite deep.

Its macOS source explicitly loads the private SkyLight framework and calls APIs including:

- `SLEventPostToPid`
- `SLPSPostEventRecordTo`
- `SLEventAuthenticationMessage`
- `CGEventSetWindowLocation`
- `SLPSSetFrontProcessWithOptions`

It does this because the public `CGEventPostToPid` path is insufficient for some Chromium and Catalyst applications. [cua-driver’s SkyLight implementation](https://github.com/trycua/cua/blob/cua-driver-rs-v0.10.0/libs/cua-driver/rust/crates/platform-macos/src/input/skylight.rs)

So `cua-driver` does not have a fundamentally lower technical ceiling. Its weaker practical performance is more likely caused by:

- A larger, more generic tool surface.
- Less tightly coupled model instructions.
- Less per-application tuning.
- More visible version/platform edge cases.
- Weaker automatic target binding, waiting, and perception alignment.
- Codex not always loading/exposing its MCP tools correctly—which was exactly part of your earlier configuration problem.

### Can we build something comparable?

Yes—especially on macOS.

A public-API implementation can support excellent background capture, AX-based actions, direct value setting, targeted events, and non-activating app launch.

The difficult boundary is universal background pixel and keyboard input. For applications that reject public event delivery, there are only three realistic options:

- Use private SkyLight APIs, accepting macOS-version fragility and no Mac App Store distribution.
- Briefly foreground the app, act, and restore the previous app.
- Run automation in a separate macOS user session or VM.

The fastest path would be to improve the Codex integration around `cua-driver`, not rewrite its native controller: expose a small Sky-compatible plugin, combine screenshot + AX state, add automatic settle detection and app-specific guidance, and enforce snapshot → action → verification. That should close much of the perceived gap.
