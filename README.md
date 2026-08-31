# rot

`rot` is a Rust-only LOC counter, complexity measuring tool, primarily to guide agents in refactor goals.

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

Run `rot --help` for the complete option list and profile controls.

## Deeper visibility analysis

Fast `rot` deliberately stays at the source level. It can count explicit `pub`
declarations, but it cannot prove whether another crate or public interface
actually requires that visibility.

`rot-audit` is the slower, compiler-backed companion for aggressive
refactoring. It runs a real Cargo/rustc build for one target and feature profile,
correlates the selected Cargo units, and turns the raw `pub` count into an
actionable list:

- **Required public:** must remain `pub` for a selected cross-crate use or
  public interface.
- **Can narrow:** reachable, but unrestricted `pub` is unnecessary.
- **Dead public:** unreachable from every selected compiled target.

Use `rot` for routine measurement and revision diffs. Use `rot-audit` when you
want to reduce visibility across a workspace and need source locations and
compiler-backed reasons for each candidate.

The evidence is closed-world: it covers only the selected compiled targets, not
inactive feature profiles, doctests, or unknown external consumers. The audit
also requires a driver built for the exact selected rustc.

See [Compiler-backed visibility audit](docs/rustc-backed-analysis.md) for setup,
supported compilers, safety boundaries, and the full evidence contract.
