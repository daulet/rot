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

## Quick start

Rot is currently built from a repository checkout:

```console
cargo build --release
./target/release/rot path/to/workspace
```

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

Use `--strict` with fast `rot` automation. Rot still emits the requested report,
but exits unsuccessfully if analysis diagnostics remain. Operational errors fail
with or without `--strict`; metric changes never determine exit status, so the
caller must evaluate the reported deltas.

Run `rot --help` for the complete option list and profile controls.

## Deeper compiler analysis

Fast `rot` deliberately stays at the source level. It can count explicit `pub`
declarations, but it cannot resolve cross-crate uses, public reexports, or the
concrete consumers of a declaration.

`rot-audit` is the slower, compiler-backed companion for aggressive
refactoring. It runs a real Cargo/rustc build for one target and feature profile,
correlates the selected Cargo units, and provides three deeper views:

- Visibility safety: declarations that must remain public, can be narrowed, or
  are unreachable in the selected compiled targets.
- API topology diffs: public definitions and namespace bindings added, removed,
  or retargeted since a Git revision.
- Impact explanations: direct and transitive consumers of one exact definition,
  with production, test, build-time, and public-interface provenance.

```console
rot-audit . --locked --offline --driver PATH
rot-audit . --baseline origin/main --locked --offline --driver PATH
rot-audit . --explain 'my-package:module::item' --driver PATH
```

Use `rot` for routine measurement and source-metric diffs. Use `rot-audit` when
you need compiler evidence before moving, deleting, narrowing, or reexporting
code across a workspace. `--baseline` performs two isolated real builds.

The evidence is closed-world: it covers only the selected compiled targets, not
inactive feature profiles, doctests, or unknown external consumers. The audit
also requires a driver built for the exact selected rustc. Its API comparison
tracks public names and binding topology; it is not a semver checker and does
not detect a same-path signature or ABI change.

See [Compiler-backed refactoring audit](docs/rustc-backed-analysis.md) for
setup, supported compilers, safety boundaries, and the full evidence contract.
