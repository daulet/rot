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

Rot is currently built from a repository checkout rather than published as a
crate. The visibility audit co-ships a private protocol crate and exact-rustc
driver workspace, so publishing only the root package would be incomplete.

```console
cargo build --release
./target/release/rot path/to/workspace
./target/release/rot path/to/workspace --files
./target/release/rot path/to/workspace --format json --summary-only
./target/release/rot path/to/workspace --baseline HEAD~1
```

Every positional directory is an explicit discovery and ignore boundary.
`.ignore` and `.gitignore` files in that directory and its descendants apply;
ignore rules from files above it are not inherited. An explicitly selected Rust
file is always analyzed. Pass `--no-ignore` to include paths filtered by
applicable ignore files and `--hidden` to include hidden paths other than
`.git`.

For coding agents, the compact snapshot and revision-comparison forms are the
main interfaces:

```console
# Deterministic aggregate JSON without per-file payloads.
rot . --format json --summary-only

# The committed baseline versus Rust discovered in the live filesystem.
rot . --baseline origin/main --format json --summary-only

# Human totals, percentages, role changes, and the ten largest metric changes.
rot . --baseline HEAD~1

# Expand the comparison to every metric-changing file.
rot . --baseline HEAD~1 --files
```

`--files` changes table detail; it does not select input files. Positional
`PATH` arguments select inputs. JSON is written to stdout and diagnostics to
stderr, so agents can parse stdout without stripping warnings.

## Configuration profiles

By default, `rot` starts each package selected by `PATH` with its Cargo default
features and uses the host's built-in `rustc --print cfg` predicates. The most
useful profile controls are:

```console
rot . --features serde,cli
rot . --no-default-features --features minimal
rot . --all-features
rot . --all-features --exclude-feature unstable
rot . --target aarch64-unknown-linux-gnu
rot . --cfg loom --unset-cfg debug_assertions
rot . --release
```

Ordinary feature closure starts from the packages selected by `PATH` and follows
workspace-member dependency edges, including dependency-declared features,
default features, and optional-dependency activation. A containing workspace
path selects every member; selecting one member does not let an unselected
reverse dependent activate it. Development dependencies are followed only from
PATH-selected roots, not transitively from ordinary dependencies. Fast mode
records one per-package feature approximation rather than Cargo's distinct
host/target units.
Normal and development dependencies inherit their active parent's requested-
target or host context; build dependencies enter host context, as do proc-macro
dependencies and their transitive normal dependencies. If one package is
reached in both contexts, fast mode unions both context-specific edge and
feature sets into its one package profile. Platform expressions use plain
`rustc --print cfg`, as Cargo does, so `--release`, `--cfg`, and `--unset-cfg`
affect authored source predicates but do not rewrite dependency selection.

JSON names this resolver `workspace_package_union`. In particular, an ordinary
library instantiated in both host and target contexts is not represented as two
separate Cargo units: its feature and edge sets are unioned. The compiler audit
names its resolver `cargo_unit_graph` and uses Cargo's actual units instead.

`--exclude-feature` is deliberately stronger than Cargo: it forces that feature
predicate false after the ordinary feature closure, even when another enabled
feature implies it. Features activated while resolving that closure remain
enabled. Such reports are marked `synthetic` as provenance, not diagnosed as an
error or warning, so an otherwise clean report works with `--strict`.
Unqualified selectors apply only to packages rooted by `PATH`. A qualified
selector such as `crate_name/feature_name` may address a selected root or a
structurally forward-reachable workspace member without activating the optional
edges leading to it. The qualifier can also be a direct dependency alias of a
selected root; that form activates the dependency exactly like Cargo, including
renamed dependencies. When an alias and package name are identical, both
interpretations apply and their feature effects are unified. Exclusions accept
the same names but never activate an inactive dependency. Reverse, unrelated,
ambiguous, and unknown selectors are errors.

Fast mode explicitly asks rustc for a development cfg preset by default.
`--release` switches only the built-in `debug_assertions` default off and records
`cfg_preset: "release"`. It does not claim to resolve project-specific
`[profile.release]`, Cargo configuration, environment, package overrides, or
`RUSTFLAGS`; those can vary by Cargo unit. Explicit `--cfg` and `--unset-cfg`
overrides are applied after the preset.

Fast JSON records `cfg_resolution: "requested_target_global"`: the requested
target's predicates are applied to every authored source file. On a cross-target
run this can classify cfg-gated build-script or proc-macro lines differently
from Cargo, which compiles those units for the host. The compiler audit records
`cargo_unit_graph` and carries each actual invocation's cfg instead.

Unknown custom cfg predicates are not guessed. Their lines go to
`conditional`; `--cfg` and `--unset-cfg` make them explicit.

## Metric contract

### Lines and ownership

Every physical line belongs to exactly one role, so role totals always add up to
the project total. A line containing any Rust token is code; otherwise a comment
token wins over whitespace. Blank-looking lines inside multiline strings are
code, and blank-looking lines inside block comments are comments. `docs` is a
subset of `comments`.

The `Files` value on each role is the number of files contributing to that role.
One Rust file can contain both production and test lines, so role-level file
counts overlap. The `Total` row is the distinct Rust-file count; line counts do
not overlap.

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
strings and comments do not count. This is Rot's defined SCC-style metric, not a
promise that the number is identical to a particular `scc` release or option
set.

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
or unnecessary visibility. Use the optional `rot-audit` companion when you need
compiler-proven visibility information.

## Revision comparisons

`--baseline REF` compares one committed Git tree with Rust discovered in the
live filesystem under Rot's positional-directory ignore policy. Tracked,
staged, unstaged, and untracked files contribute whenever that discovery policy
includes them; Git's ignored/untracked classification does not independently
filter metrics. Rot resolves `REF` to an exact commit before analysis and
reports the current `HEAD`.

The endpoint's `dirty` flag is separate: it describes the entire repository as
ordinary `git status` sees it, including non-Rust changes and paths outside the
selected input. Git-ignored untracked files do not make that flag dirty even
when a narrower Rot `PATH` boundary discovers their Rust source. Metric changes
do not make the command fail.

Both endpoints use the same positional-directory ignore boundaries as an
ordinary snapshot. This makes the comparison's working-tree totals identical
to a standalone snapshot run with the same paths and profile.

The human report shows before/after/delta/percentage for project metrics,
role-level file and code deltas, metric-changing file counts, and the ten
largest metric changes. Contributors are ranked first by role-aware code churn,
then by the remaining metrics; each row names every nonzero delta, so comment-
or blank-only edits stay actionable. `--files` removes that ten-row limit. A
zero-to-positive change is rendered as `new`; JSON uses
`percent_change: null`, never infinity or NaN. Zero-to-zero is `0%`. File
identity is repository-relative and deterministic. Rot reports a rename as a
deletion plus an addition and compares metrics, not textual Git churn.

The baseline accepts one ref, not `A..B` or merge-base semantics. To include a
file created after the baseline, select a containing directory; selecting that
new file directly is rejected because the corresponding baseline path does not
exist. A selected path must also have the same file/directory kind at both
endpoints; select a stable containing directory if its kind changed. Inputs must
belong to one Git repository. Rot maps the lexical repository-relative
positional path independently at each endpoint. A tracked symlink may therefore
resolve to different in-repository targets in the baseline and working tree;
either endpoint resolving outside its materialized repository is rejected.

Rot materializes committed objects into a private temporary directory without
registering a Git worktree or changing a branch. As in an ordinary snapshot,
Cargo metadata may resolve dependencies, and repository-controlled manifests,
Git attributes/filters, and filesystem links are a trust boundary. The
temporary location is normalized out of JSON, diagnostics, and file identity.
The complete committed tree is materialized even for a narrow `PATH`, because
Cargo metadata may need the containing workspace. Only tracked files from that
repository are available: submodule worktrees and path dependencies outside the
repository can make Cargo metadata fail. Rot reports that as an endpoint
diagnostic and falls back to standalone source discovery; `--strict` rejects
the comparison.

## Visibility audit

`rot` is always the fast source analyzer. It does not load the compiler driver,
run build scripts, or add compiler fields to its report. The optional
`rot-audit` binary is a separate, deliberately slow visibility audit for
refactoring work.

Build the audit binary and one rustc-private helper for the exact compiler you
want to run. Stable drivers need a crate-scoped `RUSTC_BOOTSTRAP` because
`rustc_private` is unstable even when the underlying compiler release is stable:

```console
rustup toolchain install 1.98.0 \
  --component rustc-dev --component rust-src --component llvm-tools-preview
RUSTC_BOOTSTRAP=rot_rustc_driver cargo +1.98.0 build \
  --manifest-path compiler/rot-rustc-driver/Cargo.toml \
  --target-dir compiler/rot-rustc-driver/target/1.98.0 --release
cargo build --release --features audit --bin rot-audit

./target/release/rot-audit path/to/workspace --locked --offline \
  --toolchain 1.98.0 \
  --driver compiler/rot-rustc-driver/target/1.98.0/release/rot-rustc-driver
```

`--driver` is required. Rot does not guess a driver location or read an
environment-variable fallback.

The default remains `nightly-2026-08-27`; build it without
`RUSTC_BOOTSTRAP` if you prefer the development toolchain. Protocol v4 is
compiler-independent, but a driver binary is not: the handshake requires the
driver's linked rustc release, full commit, and host to equal the selected
toolchain before any project build starts. Keep drivers in separate target
directories; never reuse one across compiler releases or patch versions.

As verified on 2026-08-29, every stable Rust release from the preceding 365
days is supported on `aarch64-apple-darwin`: `1.90.0`, `1.91.0`, `1.91.1`,
`1.92.0`, `1.93.0`, `1.93.1`, `1.94.0`, `1.94.1`, `1.95.0`, `1.96.0`,
`1.96.1`, `1.97.0`, `1.97.1`, and `1.98.0`. The exact releases, publication
dates, commits, host, and rolling-window date live in
[`compiler/supported-rustc.toml`](compiler/supported-rustc.toml). The audit
rejects identities outside that embedded evidence set. Supporting another host
or advancing the one-year window requires rebuilding and passing the complete
driver fixture matrix, then updating that manifest; a newer compiler building
the source is not proof for an older compiler.
Window membership is derived from Rust's official
[`RELEASES.md`](https://github.com/rust-lang/rust/blob/master/RELEASES.md).

`rot-audit` exits unsuccessfully when the driver is missing, unsupported, or
mismatched, Cargo or rustc fails, an invocation cannot be correlated, or the
visibility graph is otherwise incomplete. Missing evidence is never presented
as zero findings.

The audit runs `cargo check --workspace --all-targets --keep-going` in isolated
target and build directories. It can download dependencies unless `--offline`
is passed, and it executes project-controlled build scripts and procedural
macros. `--locked` and `--offline` have their ordinary Cargo meanings;
`--scratch-dir` chooses the parent for temporary isolated artifacts.
Rot sets `RUSTC_BOOTSTRAP=1` only on Cargo's read-only unit-graph and
configuration preflights. It removes ambient `RUSTC_BOOTSTRAP` from the project
`cargo check`, preventing either that preflight setting or the caller's process
environment from leaking into the build. Explicit Cargo user/project
configuration remains part of the trust boundary and may deliberately set the
variable.

Feature controls use actual Cargo semantics:

```console
rot-audit . --driver PATH --features serde,cli
rot-audit . --driver PATH --no-default-features --features minimal
rot-audit . --driver PATH --all-features
rot-audit . --driver PATH --target aarch64-unknown-linux-gnu
rot-audit . --driver PATH --cfg loom
```

Unlike fast `rot`, the audit does not accept `--exclude-feature` or
`--unset-cfg`: Cargo feature unification cannot force an enabled transitive
feature off, and a compiler audit only accepts realizable build profiles.

The audit has one fail-closed semantic status with two views:

- `required_visibility` lists declarations that genuinely must remain public
  for a selected cross-crate interface or use.
- `closed_world` lists `dead_public` and `unnecessary_public` candidates.
  “Unnecessary public” means unrestricted `pub` can be narrowed; it does not by
  itself choose between private, `pub(super)`, and `pub(crate)`.

Normal output prints every dead or narrowable declaration with its source
location, definition path, finding kind, and reason. JSON retains the complete
required-public list, findings, invocation identities, and evidence status.

Required visibility is intentionally conservative where expanded HIR erases
the spelling a downstream crate used. Directly exported namespaced `pub macro`
definitions retain their module path, and public inherent associated types are
retained with their interface dependencies. A `#[macro_export]` macro does not
retain the private module where its definition happens to appear.

The compiled-target scope excludes doctests and Cargo targets skipped because
their `required-features` are inactive. Both exclusions are carried in JSON as
`evidence_exclusions`; a finding such as `dead_public` is never a claim about
uses from those uncompiled roles. Doctest evidence would require a separate
rustdoc-backed pass and is not currently collected.

See [Compiler-backed visibility audit](docs/rustc-backed-analysis.md) for the
graph, completeness, and evidence contracts.

## JSON

`--format json` emits deterministic fast schema version 3. Every document has
`report_kind: "snapshot" | "comparison"` and
`detail: "files" | "summary"`. Snapshot JSON includes the exact
target/feature/cfg profile, project and role buckets, declared visibility, and
recoverable diagnostics. Comparison JSON carries separate baseline/current
profiles and diagnostics, exact endpoint identity, signed changes, percentages,
role changes, and changed-file counts. The fast schema never contains a
`compiler` field.

Both report kinds include `selection`: sorted, normalized positional paths with
file/directory kinds, `include_hidden`, `respect_ignores`, and
`ignore_boundary: "path"`. Stored JSON therefore identifies what was measured
and the discovery policy that selected it. Thread count, rendering format, and
strict exit policy are intentionally not metric provenance. Selection paths are
relative to that document's `root`: the common selected root for snapshots and
the repository root for comparisons.

Detailed JSON contains path-sorted per-file records by default.
`--summary-only` omits only those records; aggregate values are unchanged.
`--files` is table-only. `rot-audit --format json` uses a separate
`schema_version: 2` visibility-audit report with the requested toolchain and
observed exact compiler identity. Once that identity is validated, later
unavailable audit output retains it instead of substituting `unknown` values.

Pass `--strict` to return a non-zero exit status when any diagnostic remains.
Intentional feature/cfg controls are profile provenance rather than diagnostics.
For comparisons, diagnostics from either endpoint participate in strict mode.
Operational/Git errors also fail; metric changes do not. Broken pipes are
treated as successful exits so piping into tools such as `head` is safe.

## Current boundaries

- Rust source only. Ignore and hidden-path rules decide which files contribute
  metrics. Cargo targets and module edges outside that admitted set may still be
  parsed to classify visible source, but are not themselves reported.
- Cargo metadata is workspace-local (`--no-deps`); dependency source is not
  scanned.
- Analyze independent Cargo workspaces in separate invocations.
- Cargo feature options require available Cargo metadata.
- Non-literal `include!` and item-producing macros are not expanded.
- Markdown and other non-Rust files are not counted. Doctest snippets embedded
  in Markdown are not extracted as Rust test targets.
- The report describes the selected source/configuration profile, not a proof
  that the crate successfully compiles for that profile.

Run `rot --help` or `rot-audit --help` for the corresponding CLI surface.
