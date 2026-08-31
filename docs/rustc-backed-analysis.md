# Compiler-backed refactoring audit

`rot` and `rot-audit` answer different questions:

- `rot` measures authored Rust source quickly. It owns line counts,
  production/test classification, authored complexity, and explicit unrestricted
  `pub` counts.
- `rot-audit` compiles one concrete Cargo profile and resolves visibility
  requirements, public-name topology, and dependency-impact explanations
  within the selected compiled targets.

The audit is an optional companion binary, not a mode of `rot`. Fast JSON never
contains compiler output, and a compiler failure cannot turn an ordinary source
measurement into a slow or partial operation.

## Build and run

The stable orchestration binary is gated by the `audit` Cargo feature. The
rustc-private driver is a separate workspace compiled once for each exact
compiler identity. This stable example uses a version-specific target directory
so its binary cannot be confused with another compiler's driver:

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

# Compare public API topology with a commit. This performs two real builds.
./target/release/rot-audit path/to/workspace --baseline origin/main \
  --locked --offline --toolchain 1.98.0 --driver PATH

# Explain the selected compiled consumers of one exact definition.
./target/release/rot-audit path/to/workspace \
  --explain 'package-name:module::item' \
  --toolchain 1.98.0 --driver PATH
```

Stable rustc still gates `rustc_private`; `RUSTC_BOOTSTRAP=rot_rustc_driver`
opts out only for the named driver crate while building it. The default audit
toolchain is `nightly-2026-08-27`, whose driver can be built without that
variable.

Protocol v5 is independent of a particular rustc, but each driver binary is
linked to one exact compiler. The handshake checks the protocol and driver
versions plus the selected and linked rustc release, full commit, and host
before any project build starts. A driver must be rebuilt for every rustc patch
release; ABI-compatible-looking reuse is unsupported.

`--driver` is required. Rot does not guess a driver location or read an
environment-variable fallback, so automation always names the exact helper it
intends to execute.

## Supported compiler window

Rot treats compatibility as tested evidence, not a version range. The embedded
[`supported-rustc.toml`](../compiler/supported-rustc.toml) ledger records the
verification date, host, Rust publication dates, and exact compiler identities.
As of 2026-08-29, the supported stable window on
`aarch64-apple-darwin` contains all 14 releases published in the preceding 365
days: 1.90.0, 1.91.0, 1.91.1, 1.92.0, 1.93.0, 1.93.1, 1.94.0, 1.94.1,
1.95.0, 1.96.0, 1.96.1, 1.97.0, 1.97.1, and 1.98.0. The default development
nightly is recorded separately.

Every listed stable compiler built the driver and passed all 12 driver unit
tests and all 17 semantic-graph fixtures, including finite public bindings.
`rot-audit` rejects a selected release, commit, or host absent from the ledger.
A rolling update therefore has three atomic steps: derive the current stable
list from Rust's release record, run the complete exact-toolchain matrix on each
claimed host, and update the ledger only after every row passes. Building
old-compatible source with a newer rustc does not establish support.

## Execution boundary

The audit runs the selected-toolchain equivalent of:

```console
cargo check --workspace --all-targets --keep-going
```

It uses isolated Cargo target and build directories, then correlates every
selected driver sidecar with Cargo's expected unit graph and artifact messages.
`--scratch-dir` selects the parent of those temporary directories; the individual
run directory is still temporary.

This operation executes project-controlled build scripts and procedural macros.
It may download dependencies unless `--offline` is supplied. `--locked` and
`--offline` have their normal Cargo meanings. The audit also rejects compiler or
workspace-wrapper overrides that would make its observations ambiguous.
Rot sets `RUSTC_BOOTSTRAP=1` only on the read-only unstable configuration and
unit-graph preflights. It removes ambient `RUSTC_BOOTSTRAP` from the actual
project `cargo check`, preventing either that preflight setting or the caller's
process environment from leaking into the build. Explicit Cargo user/project
configuration remains part of the build trust boundary and may deliberately set
the variable.

`--baseline REF` repeats that execution once for the exact Git commit and once
for the live working tree. The committed tree is materialized as a temporary
sibling of the repository, preserving ancestor Cargo configuration and `../`
path-dependency topology without registering a Git worktree. Both endpoints use
the same explicit target, Cargo/profile flags, driver, and exact compiler
identity, but separate target/build directories. If either endpoint is
incomplete, no API diff is produced.

## Concrete Cargo profiles

Each invocation describes one real Cargo program configuration. The supported
profile controls are:

```console
rot-audit . --driver PATH --features serde,cli
rot-audit . --driver PATH --no-default-features --features minimal
rot-audit . --driver PATH --all-features
rot-audit . --driver PATH --target aarch64-unknown-linux-gnu
rot-audit . --driver PATH --cfg loom
```

Features are resolved by Cargo, including dependency feature unification and
transitive activation. The audit deliberately has no `--exclude-feature`: Cargo
cannot force an already enabled transitive feature off. To compare profiles, run
the audit separately with the desired `--features`, `--all-features`, or
`--no-default-features` arguments and compare the findings.

Likewise, the audit has no `--unset-cfg`. A positive custom `--cfg` is passed to
the real compilation only when it can be composed safely with the workspace's
rustflags. Synthetic all-except-feature and forced-false-cfg profiles remain
source-analysis features of `rot`, not compiler evidence.

Cargo targets whose `required-features` are not active are not compiled. That
absence is disclosed in every completed visibility report rather than treated as
proof that their uses do not exist.

## Semantic graph

The driver records the compiler-resolved facts needed for one atomic audit
product:

- definitions and their nominal/effective visibility;
- source spans and whether a declaration is authored or generated;
- runtime and public-interface roots;
- resolved references between definitions;
- finite public namespace bindings, including direct and reexported names;
- Cargo unit, feature, target, test, host/target, and build-script identity.

The aggregator keeps concrete invocation identity. A definition compiled once
for production and again for a test harness is not allowed to form an impossible
path by mixing edges from the two configurations. Authored declarations that
share one physical visibility span are compared across every relevant
invocation before a narrowing candidate is emitted.

Production and nonproduction roots are traversed separately. Nonproduction
includes unit/integration tests, benches, and examples selected by Cargo. The
`test_compiled_only` field marks a declaration that exists only in a test-mode
compilation.

Compiler expansion is used for resolution, not as a maintainability metric.
Generated definitions can participate in reachability, while only authored,
source-editable visibility is reported as a refactoring candidate.

### Public API topology

For selected production library and proc-macro units, the audit projects the
complete graph into a deterministic API-topology snapshot:

- named externally reachable definitions;
- finite `(parent, exported name, namespace) -> resolved target` bindings.

It never composes every possible public path through reexports; cycles would
make that set unbounded. Cross-revision identity uses workspace-relative package
root, Cargo target, definition path, exported name, and namespace. It never uses
Cargo package IDs containing checkout paths, revision-local rustc definition
hashes, source hashes, or line numbers.

`--baseline REF` reports added and removed definitions/bindings plus bindings
whose stable export slot now resolves to a different target. Exposure-route or
source-location changes alone are provenance changes, not public-name topology
changes. The full current snapshot is available in JSON; ordinary human output
stays focused on actionable differences.

This is intentionally not semver analysis. A parameter, return type, generic
bound, ABI, or marker-trait implementation can change while retaining the same
definition path. Anonymous implementation containers and unstable opaque-type
identities remain graph evidence but are excluded from cross-revision API keys.

### Dependency impact explanations

`--explain PACKAGE:DEFINITION_PATH` selects an exact Cargo package name and
rustc definition path. If multiple physical declarations match, add
`--explain-at PATH:LINE:COLUMN`; Rot lists candidates rather than choosing one
silently. Copy `definition_path` from audit JSON or a visibility finding; do not
prepend the crate name unless rustc included it in that field.

The result includes direct reference relationships, the exact unique transitive
consumer count, and one shortest root-to-definition witness for each available
provenance class: production, nonproduction, build-time, and public interface.
Normal/test invocation state remains separate, and synthetic cross-profile
visibility-equivalence edges are never presented as real consumers.

Reference locations are representative sites, not an exhaustive list of call
sites. The driver intentionally retains one canonical relationship for each
resolved source-definition/target pair. Missing source spans do not erase a
valid compiler-resolved relationship.

Host build-script and proc-macro consumers are classified as build-time, not
production. A query with no exact match or multiple physical matches leaves
`impact.status` unavailable and exits unsuccessfully even when the underlying
semantic graph remains complete.

## Visibility findings

The audit reports one atomic visibility result with two related views.

### Required public

`required_visibility` contains authored declarations that must remain
unrestricted public for a selected cross-crate interface or resolved use. A
required declaration is compiler evidence for this selected workspace and
profile; it is not a claim that every possible external consumer needs it.

The analysis is conservative where lowering erases the source spelling used by
a consumer. For example, directly exported namespaced `pub macro` definitions
retain their module path, public inherent associated types retain their
interface dependencies, and a legacy `#[macro_export]` definition does not keep
its otherwise private source module public.

### Narrowable and dead public

`closed_world.findings` contains:

- `unnecessary_public`: the declaration is reachable, but no selected
  cross-crate use or public-interface requirement needs unrestricted `pub`.
- `dead_public`: the declaration is not reachable from any compiled production
  or nonproduction root.

“Unnecessary public” does not mean “make private.” The evidence proves that
unrestricted public visibility is unnecessary, but the appropriate replacement
may be private, `pub(super)`, or `pub(crate)`.

Human output prints each actionable finding directly. For example, the locked,
default-feature Harness acceptance run reports:

```text
Compiler audit: complete (4/4 Cargo invocations correlated)
Compiler: 1.100.0-nightly (bff8e12ff5e6bcd53dfb1dbccdcec80a60a856ed 2026-08-26 for aarch64-apple-darwin)
Scope: selected-workspace compiled-target closed world
Evidence excludes: doctests, Cargo targets skipped by the active feature profile
Required public: 27
Can narrow unrestricted pub: 4
Dead public: 0

Findings:
src/config.rs:127:5  config::ModelProfile::reasoning_summary  unnecessary_public  [production+nonproduction]  reachable, but no selected cross-crate use requires unrestricted public visibility
```

The required-public set remains available in JSON; normal text emphasizes the
declarations a refactoring pass can act on.

## Closed-world scope

Both visibility views state their scope exactly as:

```text
selected-workspace compiled-target closed world
```

They also carry these `evidence_exclusions`:

- `doctests`
- `Cargo targets skipped by the active feature profile`

`cargo check --all-targets` does not compile rustdoc doctests. A declaration used
only by a doctest may therefore appear dead inside the explicitly disclosed
compiled-target scope. Similarly, an inactive `required-features` target is not
evidence for or against visibility in another feature profile.

A finding is never a global safe-delete claim. Re-run the audit for every
supported feature/target profile and account for external downstream consumers
before narrowing a published library API.

## Fail-closed completeness

Semantic results are emitted only when all selected evidence is complete. The
audit exits unsuccessfully when any of these conditions holds:

- the driver is missing, unsupported, mismatched, or fails its handshake;
- Cargo metadata or the unit graph cannot be obtained;
- Cargo/rustc does not complete successfully;
- an expected selected invocation is missing, duplicated, or ambiguously
  correlated;
- a sidecar is malformed, incomplete, or violates its integrity constraints;
- a public binding has an unknown endpoint, duplicate namespace slot, or
  inconsistent resolved target;
- build-script cfg, target, feature, or codegen evidence disagrees;
- the reference graph cannot be closed safely.

When a structured report can be formed, its top-level `status` is `complete`,
`partial`, or `unavailable`, with a reason for non-complete evidence. Partial or
unavailable is never interpreted as zero required, narrowable, or dead
declarations, and the process still exits nonzero.

## JSON contract

`rot-audit --format json` emits a separate `schema_version: 3` report. A normal
snapshot declares `report_kind: "snapshot"`; its main
fields are:

```text
schema_version
root
profile
status
reason
expected_invocations
correlated_invocations
invocations
required_visibility
closed_world
api_surface
impact
diagnostics
```

The top-level report retains invocation identity and observed target/profile
facts, including the requested toolchain and exact rustc release, commit, date,
and host, so a result can be audited later. Once the selected compiler validates,
that exact identity is retained even when a later driver, Cargo, or correlation
failure makes the audit unavailable. `required_visibility`, `closed_world`, and
`impact` repeat scope/exclusion details where they qualify data.

Baseline output declares `report_kind: "comparison"` and contains stable Git
endpoint identities, the shared requested profile/compiler identity, endpoint
status/diagnostics, and `api_diff`. It deliberately omits raw invocation keys,
Cargo package IDs, and baseline checkout paths. `api_diff` is absent—not an
empty set—when either endpoint lacks complete evidence.

Fast `rot --format json` uses schema version 3 and contains only source metrics,
profile information, file/role buckets, and diagnostics. Its snapshot and
comparison documents declare `report_kind` and `detail`; fast JSON has no
`compiler` field.

## Deliberate non-goals

- The audit does not replace source line or authored-complexity measurement.
- It does not judge signature, ABI, generic, implementation, or semver
  compatibility at an unchanged public path.
- It does not assign maintainability complexity to derive or procedural-macro
  output.
- It does not enumerate the power set of Cargo features or target triples.
- It does not observe arbitrary external consumers of a published library.
- It does not compile doctests.

Use `rot` routinely for fast source metrics and source-metric revision
comparisons. Use `rot-audit` explicitly when aggressive cross-crate refactoring
justifies compiler-resolved visibility, API-topology, or consumer evidence.
