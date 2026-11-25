# curam — zero-trust coding agent harness (Rust)

The native-Rust harness, built semantic-for-semantic on the proven Python POC
(`curam/curam-harness-poc`) and linking the pure-Rust security core
(`curam-security/crates/curam-core`). See `docs/rust-harness-plan.md` in the
curam-harness-poc repo for the roadmap (build phases H0–H7).

This is one of four curam repos — see `docs/REPOS.md` in the curam-harness-poc repo
for the whole set and why it is split. Note the path dependency: this repo must
be cloned as a sibling of `curam-security`.

## Status

| Phase | What | State |
| --- | --- | --- |
| H0 | curam-core split (curam-security workspace) | **done** |
| H1 | CLI + config layering/validation + init + doctor | **done** (this repo) |
| H2 | provider (ureq/SSE) + budget + journal | next |
| H3–H7 | agent loop + tools, cage exec, TUI, report/sli, evals | planned |

## Build & test

```bash
cargo build        # produces target/debug/curam
cargo test         # 13 unit tests: config golden/negative, POC back-compat,
                   # layering, set_key, scaffold idempotence, doctor checks
```

## Commands (H1)

```text
curam "<task>"        one-shot task (agent loop lands in H3)
curam init [DIR]      scaffold curam.toml + workdir/skills/ + workdir/evals/
curam config --show|--validate|--path|--set KEY=VALUE
curam doctor          preflight: config, cage binary, lists, terminal
curam sli|report|evals|tasks|lists|tui|repl   registered, land with H5–H7
```

Config is layered TOML: defaults → `~/.config/curam/curam.toml` →
`./curam.toml` → `-C FILE`. Every POC `curam.toml` key parses to the same
meaning; wrong types / invalid enums fail with the exact key path
(`config: [budget] cap_usd: expected number, got "string"`).

Exit codes: `0` ok · `1` runtime · `2` usage/config · `3` provider ·
`4` budget · `5` denied · `130` SIGINT.
