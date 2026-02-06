# clippy

**clippy** is a keyboard-driven agent operator tool for terminal workflows.

It captures the *latest completed assistant response* from an interactive
terminal session and lets you relay it into another session using only global
hotkeys — no mouse, no selection, no in-band commands.

This project is named in homage to Microsoft’s original Clippy: a non-AI helper
from an earlier era.

---

## What clippy Does

- Wraps interactive AI agents in the terminal
- Detects completed assistant turns deterministically
- Lets you copy the latest completed turn with a single hotkey
- Lets you paste that turn into another terminal with a single hotkey

Agents are unaware that clippy exists.

---

## What clippy Is Not

- Not a terminal emulator
- Not a clipboard manager
- Not an editor plugin
- Not a general automation framework
- Not an AI system

clippy operates strictly at the boundary between terminal I/O and human intent.

---

## Where clippy Fits

clippy operates at the terminal I/O boundary — it captures live agent turns
as they complete in a PTY session and relays them via hotkeys. This is a
strictly lower layer than tools like [gastown](https://github.com/steveyegge/gastown) (by Steve Yegge), which manage
structured conversation artifacts and workflows. clippy provides the raw
capture primitive; gastown and similar systems operate on structured output
above it. The two are complementary and non-overlapping: clippy gets the turn
out of the terminal, higher-level tools decide what to do with it.

---

## Repository Structure

```
clippy/
├── ROADMAP.md          # Versioned capability roadmap
├── README.md           # Human-oriented overview (this file)
├── docs/
│   └── ARCH_INDEX.md   # Fast lookup index for agents
└── src/                # Implementation (added incrementally)
```

---

## Current Status

clippy is under active development.
The initial release (v0) targets **Linux + X11 + Konsole**.

Portability is an explicit future goal, not a v0 requirement.

---

## For Agents

Agents interacting with this repository should start with:

- `docs/ARCH_INDEX.md`
- then `ROADMAP.md`

---

## Philosophy

clippy is designed as a **primitive**, not a product.

It exists to reduce friction in building better tools — including tools that
improve clippy itself.
