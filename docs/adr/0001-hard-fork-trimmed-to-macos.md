# ADR 0001: Hard fork of trycua/cua, aggressively trimmed to the macOS cua-driver path

- Status: accepted (by Linus default; awaiting Douglas ratification)
- Date: 2026-07-23

## Context

We are building a macOS-only, background-first computer-control driver ("improved_cua")
on top of the trycua/cua codebase. Upstream is a multi-OS, multi-product monorepo
(lume, qemu-docker, kasm, xfce, cua-bench, fleet, typescript + python workspaces)
under active development; cua-driver's contract is versioned (CONTRACT_VERSION 0.2.0)
and its MCP protocol is pinned (2025-06-18), so upstream fixes remain valuable.

Three fork shapes were considered:

- **A. Hard fork of the full monorepo, then delete everything not on the macOS
  cua-driver path**, keeping git history and an `upstream` remote.
- **B. Extract libs/cua-driver into a brand-new clean repo**, abandoning upstream
  tracking.
- **C. Soft fork**: minimal divergence, track upstream continuously.

## Decision

Option A. One loud deletion commit removes: libs/lume, qemu-docker, kasm, xfce,
cua-bench, fleet, typescript/* (except the driver binding if it stays useful),
platform-windows, platform-linux, wayland-helper, and all non-macOS CI.
Surviving paths keep their upstream names and locations.

## Consequences

- Cherry-picking upstream cua-driver fixes keeps working, because git cherry-pick
  only touches paths that still exist on both sides.
- We inherit working build/test tooling (release-please, cua-driver-testkit,
  contract fixtures) instead of rebuilding it.
- The deleted code is recoverable from history; the fork can re-adopt a component
  later if the product changes.
- We now own merge conflicts only inside libs/cua-driver, not across the monorepo.
- Repo still carries monorepo-shaped tooling (pnpm/uv workspaces, nix) that must
  be pruned in the same pass or it rots.
