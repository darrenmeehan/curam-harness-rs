# curam — zero-trust coding agent harness (Rust)

The native-Rust harness, built semantic-for-semantic on the proven Python POC
and linking the pure-Rust security core,
[published `curam-core`](https://crates.io/crates/curam-core), from crates.io —
no sibling checkout needed. See
[`rust-harness-plan.md`](https://github.com/darrenmeehan/curam-harness-poc/blob/main/docs/rust-harness-plan.md)
for the roadmap (build phases H0–H7).

This is one of four curam repos — see
[`docs/REPOS.md`](https://github.com/darrenmeehan/curam-harness-poc/blob/main/docs/REPOS.md)
for the whole set and why it is split.

## Install

This repo is a Nix flake, so no checkout is needed:

```bash
nix run github:darrenmeehan/curam-harness-rs -- --help
nix profile install github:darrenmeehan/curam-harness-rs
```

Or as an input in another flake:

```nix
inputs.curam-harness.url = "github:darrenmeehan/curam-harness-rs";
# use inputs.curam-harness.packages.${system}.default
```

From a checkout: `nix build` then `./result/bin/curam`, or
`nix develop --command cargo build --release`.

Two runtime requirements, both reported by `curam doctor`:

- **`curam-run`** is the bwrap cage and ships with `curam-security`, not this
  repo. Put it on `PATH` or set `CURAM_RUN_BIN`; the shell and run tools fail
  closed without it.
- The **window front end** needs Vulkan plus an X11 or Wayland session.
  `curam repl` is the no-display fallback.

Only `x86_64-linux` is built and tested. Another system needs its own GUI
`buildInputs` in `flake.nix` — macOS links ghostty's prebuilt xcframework
instead of compiling it with Zig.

## Status

| Phase | What | State |
| --- | --- | --- |
| H0 | curam-core split (curam-security workspace) | **done** |
| H1 | CLI + config layering/validation + init + doctor | **done** |
| H2–H4 | provider/budget/journal, agent loop + tools, caged exec | **done** |
| H5 | front end: GPUI window + libghostty-vt transcript | **done** (replaces ratatui) |
| H6 | skills + stage-2 classifier + report/sli | **done** |
| H7 | evals | planned |

## Build & test

```bash
cargo build        # produces target/debug/curam
cargo test         # unit tests: config golden/negative, POC back-compat,
                   # layering, set_key, scaffold idempotence, doctor checks,
                   # tui state, ANSI sanitising
```

The window front end links GPUI (via `gpui-kit`) and libghostty-vt (via
`gpui-cn`'s `ghostty` feature). Building on Linux therefore needs
`pkg-config`, fontconfig and freetype headers, Vulkan/X11 or Wayland, and
Zig 0.17 — `ghostty-vt-sys` builds libghostty-vt from source there. On a Nix
box: `nix-shell -p pkg-config fontconfig freetype libxkbcommon libxcb openssl zig`.

## Commands (H1)

```text
curam "<task>"        one-shot task (agent loop lands in H3)
curam init [DIR]      scaffold curam.toml + workdir/skills/ + workdir/evals/
curam config --show|--validate|--path|--set KEY=VALUE
curam doctor          preflight: config, cage binary, lists, terminal
curam tui             desktop window (gpui-kit) with a libghostty transcript
curam repl            plain-ANSI fallback for no display / no tty
curam sli|report|evals|tasks|lists   registered, land with H6–H7
```

Config is layered TOML: defaults → `~/.config/curam/curam.toml` →
`./curam.toml` → `-C FILE`. Every POC `curam.toml` key parses to the same
meaning; wrong types / invalid enums fail with the exact key path
(`config: [budget] cap_usd: expected number, got "string"`).

Exit codes: `0` ok · `1` runtime · `2` usage/config · `3` provider ·
`4` budget · `5` denied · `130` SIGINT.
