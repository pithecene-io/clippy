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
- Maintains a per-session history of recent turns with stable IDs and metadata
- Lets you copy/paste turns between sessions with global hotkeys
- Delivers turns to multiple sinks: clipboard, file, or PTY injection
- Exposes all operations via a scriptable CLI client

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

## Usage

clippy runs as three cooperating processes plus a CLI client:

```bash
# 1. Start the broker daemon (manages sessions, turns, and relay)
clippyd broker

# 2. Wrap an agent session (detects turns, reports to broker)
clippyd wrap -- claude

# 3. Run the hotkey client (global capture/paste hotkeys)
clippyd hotkey
```

### CLI Client

The `client` subcommand provides one-shot access to all broker operations:

```bash
# Session queries
clippyd client list-sessions
clippyd client list-turns <session> [--limit N]
clippyd client get-turn <turn_id> [--metadata-only]

# Relay operations
clippyd client capture <session>
clippyd client capture-by-id <turn_id>
clippyd client paste <session>

# Sink delivery (clipboard, file, or inject)
clippyd client deliver clipboard
clippyd client deliver file --path /tmp/turn.txt
clippyd client deliver inject --session <session>
```

`get-turn` sends metadata to stderr and raw content to stdout, so it
composes with pipes: `clippyd client get-turn s1:3 | less`

---

## Current Status

clippy is under active development.
Targets **Linux + X11**. v0 (turn relay primitive) and v1 (local turn
registry) are implemented.

Portability is an explicit future goal, not a current requirement.

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
