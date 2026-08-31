# rot

`rot` is a Rust-only LOC, complexity measuring tool, primarily to guide agents in refactoring goals.

## Prompt

Give an agent a fixed baseline and acceptance contract:

```text
Refactor this repository in iterative, reviewable rounds. Before editing, start
from a clean worktree and record `git rev-parse HEAD` as BASE_COMMIT. Keep that
exact commit, the Rot binary, selected paths, and Cargo profile fixed for the
entire campaign.

After each round, run
`rot . --baseline "$BASE_COMMIT" --format json --summary-only` with the same
profile flags. Reduce production LOC by at least 30%, and require production
authored cognitive complexity to fall at least proportionally. Do not claim
gains by moving code into test, inactive, or orphan roles, changing
discovery/configuration, weakening behavior, or deleting tests. Keep relevant
tests and performance gates passing. Use per-file output to investigate
regressions, and use native Codex subagents where useful.
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
ROT=./target/release/rot
"$ROT" path/to/workspace
```

Common workflows:

```console
ROT=./target/release/rot

# Project summary
"$ROT" .

# Include every file row
"$ROT" . --files

# Compact JSON for automation
"$ROT" . --format json --summary-only

# Capture once in a clean worktree, before an iterative refactor
BASE_COMMIT="$(git rev-parse HEAD)"

# Compare that fixed commit with staged, unstaged, and untracked Rust
"$ROT" . --baseline "$BASE_COMMIT" --format json --summary-only

# Select a Cargo/configuration profile
"$ROT" . --features serde,cli
"$ROT" . --all-features --exclude-feature unstable
"$ROT" . --target aarch64-unknown-linux-gnu --release
```

Keep `BASE_COMMIT` fixed across the campaign; names such as `HEAD~1` and
`origin/main` are resolved again on every invocation. Reuse the same path,
profile, discovery flags, and Rot binary when comparing rounds.

Positional `PATH` arguments select input; `--files` only changes table detail.
Each selected directory is its own ignore boundary. Use `--hidden` or
`--no-ignore` to broaden discovery. JSON goes to stdout and diagnostics go to
stderr.

Fast `rot` emits the requested report and then exits unsuccessfully if analysis
diagnostics remain. Operational errors also fail. Metric changes never determine
exit status, so the caller must evaluate the reported deltas.

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
