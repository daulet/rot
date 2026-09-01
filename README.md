# rot

`rot` is a Rust-only LOC, complexity measuring tool, primarily to guide agents in refactoring goals.

## Prompt

Give a prompt like below to get rid of slop, DTOs, ceremony in your project:

```
refactor the repo. identify increasingly larger pieces for simplification and reuse (but dont reuse for the sake of it) and act on it. after each round of simplification, repeat the same exercise. use native codex subagents whenever needed. the goal is reducing code base prod LOC by 30% and complexity by 20%, use rot CLI to measure it.
```

## Features

- Counts physical, code, comment, documentation, and blank Rust lines.
- Classifies source by Cargo role: production, tests, benches, examples, build,
  conditional, inactive, or orphan.
- Evaluates Cargo features, common `cfg` forms, module edges, and literal
  `include!` paths.
- Reports lexical, authored cyclomatic, and authored cognitive complexity.
- Counts explicit unrestricted `pub` declarations.
- Emits deterministic human tables or versioned JSON, with project and per-file
  detail.
- Compares a committed Git baseline with the live working tree.
- Includes optional compiler-backed visibility analysis for aggressive
  refactoring.

## Install

```console
# macOS
brew install daulet/tap/rot

# Cargo (installs the `rot` executable)
cargo install rot-metrics
```

Prebuilt macOS and Linux archives and Debian packages are available from
[GitHub Releases](https://github.com/daulet/rot/releases/latest). Install
`rot-audit` with `cargo install rot-metrics --features audit`; it also requires
the matching driver described below.

## Quick start

Run `rot` against any Rust package or workspace.

Common workflows:

```console
# Project summary
rot .

# Include every file row
rot . --files

# Compact JSON for automation
rot . --format json --summary-only

# Compare a commit with staged, unstaged, and untracked Rust
rot . --baseline HEAD~1
rot . --baseline origin/main --format json --summary-only

# Select a Cargo/configuration profile
rot . --features serde,cli
rot . --all-features --exclude-feature unstable
rot . --target aarch64-unknown-linux-gnu --release
```

Positional `PATH` arguments select input; `--files` only changes table detail.
Each selected directory is its own ignore boundary. Use `--hidden` or
`--no-ignore` to broaden discovery. JSON goes to stdout and diagnostics go to
stderr.

Run `rot --help` for the complete option list and profile controls.

## Deeper compiler analysis

`rot-audit` runs a real Cargo/rustc build to find public declarations that must
remain public, can be narrowed, or are dead. It can also compare public API
topology with a Git revision and explain a definition's compiled consumers.

Use `rot` for routine source metrics and revision diffs. Use `rot-audit` when
you need compiler evidence before narrowing, deleting, or reexporting code
across a workspace.

```console
rot-audit . --locked --offline --driver PATH
rot-audit . --baseline origin/main --locked --offline --driver PATH
rot-audit . --explain 'my-package:module::item' --driver PATH
```

Building the required driver needs a Git checkout. It must match the selected
rustc release, commit, and host, and must be rebuilt for each rustc patch.

Evidence is closed-world: selected targets and feature profile only, excluding
doctests and unknown external consumers. See
[Compiler-backed audit](docs/rustc-backed-analysis.md) for full setup.
