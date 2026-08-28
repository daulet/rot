# rot

`rot` is a fast, Rust-only source metrics CLI. It combines physical line and
SCC-style complexity counts with Cargo-aware production/test ownership and a
cheap approximation of public API surface.

The default report answers three different questions without mixing them:

- How much source is there? (`code`, `comments`, `docs`, and `blank`)
- Where does it run? (`production`, `test`, `bench`, `example`, `build`,
  `conditional`, `inactive`, or `orphan`)
- How much interface does it declare and export? (`Public` and exported
  signature lines)

## Build and run

```console
cargo build --release
./target/release/rot path/to/workspace
./target/release/rot path/to/workspace --files
./target/release/rot path/to/workspace --format json
```

`rot` respects Git and ignore files by default. Pass `--no-ignore` to scan
ignored paths and `--hidden` to include hidden paths other than `.git`.

## Configuration profiles

By default, `rot` evaluates each workspace package with its Cargo default
features and the host's built-in `rustc --print cfg` predicates. The most useful
profile controls are:

```console
rot . --features serde,cli
rot . --no-default-features --features minimal
rot . --all-features
rot . --all-features --exclude-feature unstable
rot . --target aarch64-unknown-linux-gnu
rot . --cfg loom --unset-cfg debug_assertions
```

`--exclude-feature` is deliberately stronger than Cargo: it forces that feature
predicate false even when another enabled feature implies it. Such reports are
marked `synthetic`, and broken feature implications are emitted as diagnostics.
Package-qualified selectors such as `crate_name/feature_name` are accepted.

Unknown custom cfg predicates are not guessed. Their lines go to
`conditional`; `--cfg` and `--unset-cfg` make them explicit.

## Metric contract

### Lines and ownership

Every physical line belongs to exactly one role, so role totals always add up to
the project total. A line containing any Rust token is code; otherwise a comment
token wins over whitespace. Blank-looking lines inside multiline strings are
code, and blank-looking lines inside block comments are comments. `docs` is a
subset of `comments`.

Ownership is derived from Cargo targets, the Rust module graph, and syntax-tree
attributes. In particular, a gate on an out-of-line module propagates into its
file:

```rust
#[cfg(test)]
mod tests; // every reachable line in tests.rs is test-owned
```

Nested `cfg`, compound `all`/`any`/`not`, `cfg_attr`, item/field/statement
attributes, `#[path]`, and literal `include!` edges are evaluated. Test
attributes include the built-in `test` and `bench` attributes plus common async
test macros; add project-specific ones with `--test-attribute path::to::macro`.

The roles mean:

- `Production`: reachable with `test = false` for a library or binary target.
- `Tests`: only reachable with `test = true`, or from an integration-test target.
- `Benches`, `Examples`, `Build`: owned by those Cargo target kinds.
- `Conditional`: depends on a custom cfg whose value was not supplied.
- `Inactive`: discovered source is referenced, but false in this profile.
- `Orphan`: discovered source is not reachable from a selected Cargo target.

When code is shared by production and tests, it is reported once as production.

### Complexity

Complexity is an SCC-style, zero-based lexical score. It adds one for each
Rust token `for`, `if`, `while`, `loop`, `else`, `match`, `&&`, `||`, `!=`,
`==`, and postfix `?`, excluding the `?` in `?Sized`. Tokens inside strings and
comments do not count. This is intentionally not a control-flow-graph or
cognitive-complexity metric.

### API surface

`Public` is the number of unrestricted `pub` declarations in each ownership
bucket. It is useful for spotting test-only public helpers and declarations
hidden inside private modules.

`Exported surface` is narrower: it starts at Cargo library targets and follows
public module paths, counting exported items and the physical code lines covered
by their signatures. `#[macro_export]` is treated as crate-root export.

This fast mode is syntax-backed, not compiler-backed. It deliberately does not
claim Hawk's effective-visibility, dead-export, or unnecessary-public verdicts.
Public `use` declarations (with a separate glob subset) and item-producing
macro calls that cannot be resolved without name resolution are reported
explicitly. Public items in inherent `impl` blocks are counted as unresolved
rather than exported because the receiver type may not be externally nameable.
Use the unresolved counters as the precision boundary; a future compiler-backed
mode can close it.

## JSON

`--format json` emits a deterministic report with `schema_version: 1`. It
includes the exact target/feature profile, project and per-file buckets, API
surface, and recoverable diagnostics. Per-file data is always present in JSON;
`--files` only expands the human-readable table.

Pass `--strict` to return a non-zero exit status when any diagnostic remains.
Broken pipes are treated as successful exits so piping into tools such as
`head` is safe.

## Current boundaries

- Rust source only. Ignore rules control discovery, but an ignored `.rs` file
  reached through the module graph is still included.
- Cargo metadata is workspace-local (`--no-deps`); dependency source is not
  scanned.
- Analyze independent Cargo workspaces in separate invocations.
- Cargo feature options require available Cargo metadata.
- Non-literal `include!`, public `use` declarations, and item-producing macros
  are not expanded.
- Doctest snippets embedded in Markdown are not separate test targets.
- The report describes the selected source/configuration profile, not a proof
  that the crate successfully compiles for that profile.

Run `rot --help` for the complete CLI surface.
