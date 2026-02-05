# ROADMAP.md — clippy

This roadmap describes the planned evolution of **clippy** as an
*agent operator primitive*.

Versions are defined by **capability and leverage**, not time.

---

## Core Invariant

A completed assistant **turn** is a first-class object:
- detectable
- complete
- addressable
- relayable independently of UI

All future versions must preserve this invariant.

---

## v0 — Turn Relay Primitive (Keystone)

**Goal**  
Remove friction when relaying the most recent completed assistant response
between interactive terminal sessions.

**Scope**
- Linux + X11
- Keyboard-only workflow
- Terminal-first
- Single “latest completed turn” per session

**Capabilities**
- PTY wrapper per agent session
- Deterministic detection of completed assistant turns
- Per-session latest-turn buffer
- Global relay buffer
- Global hotkey to capture from focused session
- Global hotkey to paste into focused session

**Non-Goals**
- Turn history
- Search or filtering
- Editor integrations
- macOS / tmux support
- GUI surfaces

**Why this version exists**
v0 establishes the boundary where agent output becomes *addressable state*.
Everything else builds on this.

---

## v1 — Local Turn Registry

**Goal**  
Promote “copy/paste” into structured, inspectable state.

**New Capabilities**
- Ring buffer of recent completed turns per session
- Stable turn identifiers
- Turn metadata (timestamps, truncation, interruption)
- Multiple sinks (clipboard, file, injection)

Clipboard becomes a *consumer*, not the model.

---

## v2 — Resolver Abstraction

**Goal**  
Support additional environments without destabilizing the core.

**Changes**
- Introduce explicit session resolver interface
- Terminal / environment specifics become adapters
- Core turn detection and registry remain unchanged

**Expected Additions**
- tmux resolver
- macOS terminal resolver

Portability is earned after correctness.

---

## v3 — Agent Routing Layer

**Goal**  
Make agent interaction composable rather than linear.

**New Capabilities**
- Explicit agent-to-agent relay paths
- Turn templating and wrapping
- Structured injection for review, implementation, synthesis

At this stage, clipboard usage becomes optional.

---

## v4 — Optional Persistence & Replay

**Goal**  
Enable selective memory without implicit logging.

**Capabilities**
- Explicit session snapshots
- Replay last N turns into new sessions
- User-controlled persistence only

No background recording. No surprise history.

---

## Design Principles

- Determinism over cleverness
- Explicit contracts over heuristics
- Adapters over conditionals
- Bootstrap leverage over polish

