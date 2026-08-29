# rot

`rot` is a fast, Rust-only source metrics CLI. It combines physical line,
SCC-style lexical complexity, and AST-authored cyclomatic/cognitive counts with
Cargo-aware production/test ownership and declared-visibility counts.

The default report answers three different questions without mixing them:

- How much source is there? (`code`, `comments`, `docs`, and `blank`)
- Where does it run? (`production`, `test`, `bench`, `example`, `build`,
  `conditional`, `inactive`, or `orphan`)
- How many unrestricted `pub` declarations does it contain? (`Declared pub`)

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

`Lexical` is the existing SCC-style, zero-based token score. It adds one for
each Rust token `for`, `if`, `while`, `loop`, `else`, `match`, `&&`, `||`,
`!=`, `==`, and postfix `?`, excluding the `?` in `?Sized`. Tokens inside
strings and comments do not count.

`Cyclomatic` is an AST-authored score. Every function, method, closure,
const/static initializer, async block, and const block with a body starts at
one. Decisions are `if`, loops, `let-else`, active match alternatives after the
first, match guards, each `&&`/`||`, and postfix `?`. Cfg-gated match arms are
filtered before alternatives are counted. Macro token trees are authored input
but not parsed as generated Rust control flow. Bare anonymous-const expressions
in signatures are outside this first authored metric; an explicit `const {}`
block is its own body.

`Cognitive` is a separate source-nesting score. Structural `if`, loop, match,
and `let-else` decisions add one plus their branch nesting; guards,
short-circuit operators, and `?` add one. Conditions, iterators, scrutinees,
initializers, and ordinary blocks do not add nesting. `else-if` stays at its
chain depth, and every nested body resets the depth. Bare `else`, comparisons,
`return`, `break`, and `continue` add nothing. These authored metrics remain
available for inactive, conditional, synthetic, and recoverable source.

### Declared visibility

`Declared pub` is the number of explicit unrestricted `pub` visibility
occurrences in each ownership bucket. It includes items, fields, associated
items, extern items, and `use` declarations even when they sit behind a private
module or receiver type. It excludes restricted visibility, implicit-public
trait members and enum variants, `#[macro_export]` without `pub`, and
macro-generated declarations. A grouped `pub use {A, B}` is one declaration.

This is a physical syntax count, not API surface. Fast mode makes no claim about
effective visibility, re-exported names, exported signature lines, dead exports,
or unnecessary visibility. Those require the compiler-backed mode.

## Compiler-backed mode

`--compiler` runs a second, concrete Cargo analysis and adds versioned semantic
products to the source report. It never replaces source LOC, authored
complexity, inactive-code, or synthetic-profile results. Build the deliberately
separate helper with the exact pin it was written for:

```console
rustup toolchain install nightly-2026-08-27 \
  --component rustc-dev --component rust-src --component llvm-tools-preview
cargo +nightly-2026-08-27 build \
  --manifest-path compiler/rot-rustc-driver/Cargo.toml --release
cargo build --release

./target/release/rot path/to/workspace --compiler --locked --offline \
  --compiler-driver compiler/rot-rustc-driver/target/release/rot-rustc-driver
```

The protocol-v3 helper handshake requires rustc `1.100.0-nightly`, commit
`bff8e12ff5e6bcd53dfb1dbccdcec80a60a856ed`. `rot` otherwise reports every
compiler product as unavailable and still returns the complete source report.
The same fallback applies to missing helpers, protocol mismatches, Cargo/rustc
failures, incompatible wrappers, `--unset-cfg`, and contradictory feature
exclusions. Availability is recorded per Cargo invocation and per semantic
product; unavailable never means zero.

Compiler mode runs `cargo check --workspace --all-targets --keep-going` in
isolated target and build directories. It can download dependencies unless
`--offline` is passed, and it executes project-controlled build scripts and
procedural macros. This is why the mode is explicit. `--locked` and `--offline`
have their ordinary Cargo meanings. `--compiler-target-dir` chooses only the
parent for temporary isolated artifacts; the run directory is removed when the
report is complete.

The compiler records Cargo's exact unit graph plus actual features, cfg,
host/target, test, codegen, and build-script `OUT_DIR` identity. Each sidecar is
joined to one expected Cargo unit, and real generated `.rs` files that own HIR
facts are rescanned separately under `compiler.generated_files`; they never
inflate project physical LOC.

Compiler mode currently adds four semantic views:

- `effective_api` is the finite set of effectively public definitions and
  public namespace bindings for selected production libraries and proc macros.
- `required_visibility` lists definitions that must remain public for the
  selected workspace to compile. It is a compiler requirement, not a promise
  that an external consumer needs the item.
- `closed_world` reports dead and unnecessarily public candidates under the
  explicit `selected-workspace compiled-target closed world` scope. Production
  and non-production reachability remain separate, and `test_compiled_only`
  marks findings that exist only in a test compilation.
- `macro_expansion_complexity` reports the compiler-confirmed
  `macro_expansion_cyclomatic_delta`. It stays separate from source-authored
  cyclomatic complexity until an exact body/profile baseline can be proved.
  The human total is labeled as an invocation-local sum; JSON retains every
  invocation separately.

All four products have explicit `complete`, `partial`, or `unavailable` status;
missing compiler evidence is never reported as zero. See
[Compiler-backed Rust analysis](docs/rustc-backed-analysis.md) for the metric
ownership, graph, and representation contracts.

Required visibility is intentionally conservative where expanded HIR erases
the spelling a downstream crate used. Directly exported namespaced `pub macro`
definitions retain their module path, and public inherent associated types are
retained with their interface dependencies. A `#[macro_export]` macro does not
retain the private module where its definition happens to appear.

The compiled-target scope excludes doctests and Cargo targets skipped because
their `required-features` are inactive. Both exclusions are carried in JSON as
`evidence_exclusions`; a finding such as `dead_public` is never a claim about
uses from those uncompiled roles. Doctest collection remains a separate
rustdoc-backed product.

## JSON

`--format json` emits a deterministic report with `schema_version: 2`. It
includes the exact target/feature profile, project and per-file buckets,
declared visibility, optional compiler products, and recoverable diagnostics.
Per-file data is always present in JSON; `--files` only expands the
human-readable source table.

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
- Non-literal `include!` and item-producing macros are not expanded.
- Markdown and other non-Rust files are not counted. Doctest snippets embedded
  in Markdown are not extracted as Rust test targets.
- The report describes the selected source/configuration profile, not a proof
  that the crate successfully compiles for that profile.

Run `rot --help` for the complete CLI surface.
