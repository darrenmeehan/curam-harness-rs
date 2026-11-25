---
title: "curam harness front end: gpui-kit + libghostty-vt"
date: 2026-11-25T12:45:00Z
project: "curam-harness-rs"
agent: "pi-bolt"
type: "spec"
status: "draft"
tags: ["frontend", "gpui", "ghostty", "migration"]
---

# Front end: gpui-kit + libghostty-vt

The H5 front end was a ratatui/crossterm TUI. It is now one GPUI window built
on `gpui-kit`, with the transcript painted by a real terminal: `libghostty-vt`
(the VT core extracted from Ghostty), through `gpui-cn`'s `Terminal` element.

## Why these crates, and why these versions

`gpui-kit` (https://gpui-kit.com) is the one-dependency facade over GPUI +
`gpui-base` + `gpui-component`. Current release is 0.11.0, but it has no
terminal component yet (`longbridge/gpui-kit` issue #2978 is still open), and
the only GPUI terminal on libghostty-vt is `gpui-cn` 0.9.0, which pins
`gpui-kit ^0.8.1`. Cargo would otherwise build two incompatible GPUI
backends, so this repo pins `gpui-kit = "0.8.1"` and takes `gpui-cn` with
`default-features = false, features = ["ghostty"]` (its default
`gpui-fast` would switch the whole graph to the alternative backend).

| Crate | Version | Role |
| --- | --- | --- |
| `gpui-kit` | 0.8.1 | window, `Input`, `Button`, layout, actions/keymap |
| `gpui-cn` | 0.9.0 (`ghostty`) | `TerminalState` + `Terminal` element, `StreamSource` byte feed |
| `ghostty-vt` | 0.9.0+ghostty-vt.1.0.1 | safe Rust API over libghostty-vt 1.0.1 (via `ghostty-vt-sys`) |

`ratatui` and `crossterm` are gone; `std::io::IsTerminal` covers the two
`is_tty` calls.

## What changed

- `src/tui.rs` rewritten. `App` stays a pure state machine (`rows`, `status`,
  `mode`) so it tests without a window; it now returns what to draw
  (`Out::Append` for a folded thinking delta, `Out::Row` for a new line)
  instead of rendering into a ratatui frame. `View` owns the GPUI entities:
  one `TerminalState` fed by an application-owned `StreamSource`, the prompt
  `InputState`, and the agent channels.
- The agent worker is unchanged (`spawn_task`): it still journals, streams
  `UiEv`s and honours the shared cancel flag. A 30 ms GPUI task drains the
  channel, so the window never blocks on the worker.
- Keys: Enter submits or answers the open ask, Ctrl-C cancels the in-flight
  run (window close quits). The old double-Ctrl-C-to-quit and the Ctrl-b
  "block view" are gone — the latter only ever changed its own rail hint.
- The Ctrl-b block view removal also removes the 500-row cap from the render
  path; `App::capped_rows` keeps the cap for the model.
- `src/main.rs`: `auto_entry` now picks the window when `DISPLAY` or
  `WAYLAND_DISPLAY` is set and stdin is a terminal, and the REPL otherwise.
- README updated; `Cargo.lock` must be regenerated where the sibling
  `curam-security` checkout exists (`cargo build` does it).

## Transcript safety

Tool output is attacker-influenced, and a terminal emulator obeys escape
sequences (OSC 52 clipboard writes, title changes, alternate screen). The
harness therefore never writes agent text to the emulator verbatim: `clean()`
drops every control character and caps a row at 300 chars, then the harness
adds its own SGR colour per row kind. Covered by a unit test.

## Build requirements

`ghostty-vt-sys` builds libghostty-vt from source on Linux (prebuilt only on
macOS/iOS/Windows-GNU), so a build needs Zig 0.17 plus the usual GPUI Linux
headers. On this Nix box:

```bash
nix-shell -p pkg-config fontconfig freetype libxkbcommon libxcb openssl zig
cargo build
```

## Verification

- The stack itself was built end to end here: `gpui-kit 0.8.1` +
  `gpui-cn 0.9 (ghostty)` + `ghostty-vt-sys` compile and link in that shell
  (Zig builds libghostty-vt; linking needs `libxcb` besides xkbcommon).
- This repo's own build was type-checked and its `tui` tests run against a stub
  `curam-core` in a scratch copy, because `../curam-security` is not checked
  out here. `cargo check --all-targets` is clean and the four `tui` unit tests
  pass (`deltas_fold_into_one_row_and_the_ask_opens`,
  `status_updates_do_not_draw_and_done_clears_running`,
  `escape_sequences_from_tool_output_never_reach_the_emulator`,
  `rail_carries_provider_cost_and_mode`). Re-run `cargo test` in a real
  checkout: the policy/journal tests need the real core.
- Not verified: the window itself. No display was available, so the layout,
  the terminal grid sizing and the ask buttons have been reviewed but never
  seen on screen.

## Follow-ups

1. Move to `gpui-kit` 0.11 once the terminal component lands in the kit;
   then drop `gpui-cn`.
2. Feed the terminal from the cage rather than from formatted `UiEv` text:
   `StreamSource` takes raw bytes, so a pty-backed cage would give real ANSI
   output in the transcript with no formatting guesswork.
3. Add a `gpui_kit::test` UI test once the repo can build in CI (needs a
   display-free test backend and the ghostty build deps).
