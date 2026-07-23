# ADR 0003: Signing strategy — persistent self-signed certificate now, Developer ID later

- Status: accepted (Douglas, 2026-07-23)
- Date: 2026-07-23

## Context

Codex Computer Use's first listed advantage is "a signed, persistent background
service with stable macOS permission identity." TCC attributes Screen Recording,
Accessibility, and Input Monitoring grants to a binary's code-signing identity;
ad-hoc-signed binaries get a new cdhash every build and re-prompt on each rebuild,
which makes iteration miserable and silently breaks a background service.

Options:

- **A.** Persistent self-signed codesigning certificate (stable identity, free,
  works fully offline); structure signing config so a Developer ID identity can
  replace it without touching the bundle id or designated requirements.
- **B.** Apple Developer ID from the start ($99/yr, enables notarization, needed
  for Gatekeeper-clean distribution to other Macs).
- **C.** Ad-hoc signing; accept TCC re-prompts.

## Decision

Option A, with B as a pre-planned upgrade path.

## Consequences

- TCC grants survive rebuilds on Douglas's machines; the service stays persistent.
- No notarization: first run on a *different* Mac requires right-click-open or
  `xattr -d com.apple.quarantine`; acceptable because the primary deployment
  target is local.
- Build system must keep bundle identifier and designated requirement string
  constant from day one — changing either silently orphans existing TCC grants.
- When external distribution becomes real, swapping in Developer ID + notarization
  is a signing-config change, not a code change; TCC continuity for existing
  users requires the DR to be authored with that migration in mind.
