# Compiler-backed visibility audit

`rot` and `rot-audit` answer different questions:

- `rot` measures authored Rust source quickly. It owns line counts,
  production/test classification, authored complexity, and explicit unrestricted
  `pub` counts.
- `rot-audit` compiles one concrete Cargo profile and determines which authored
  public declarations are required, narrowable, or unreachable within the
  selected compiled targets.

The audit is an optional companion binary, not a mode of `rot`. Fast JSON never
contains compiler output, and a compiler failure cannot turn an ordinary source
measurement into a slow or partial operation.

## Build and run

The stable orchestration binary is gated by the `audit` Cargo feature. The
rustc-private driver is a separate workspace pinned to the exact nightly it was
written against:

```console
rustup toolchain install nightly-2026-08-27 \
  --component rustc-dev --component rust-src --component llvm-tools-preview

cargo +nightly-2026-08-27 build \
  --manifest-path compiler/rot-rustc-driver/Cargo.toml --release

cargo build --release --features audit --bin rot-audit

./target/release/rot-audit path/to/workspace --locked --offline \
  --driver compiler/rot-rustc-driver/target/release/rot-rustc-driver
```

Protocol v4 requires rustc `1.100.0-nightly` at commit
`bff8e12ff5e6bcd53dfb1dbccdcec80a60a856ed`. The handshake checks the protocol,
driver, linked-rustc, and toolchain identities before any project build starts.

`--driver` is optional when `rot-rustc-driver` is next to the audit executable or
exists in the repository's driver target directory. Passing it explicitly makes
automation independent of those lookup rules.

## Execution boundary

The audit runs the pinned equivalent of:

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

## Concrete Cargo profiles

Each invocation describes one real Cargo program configuration. The supported
profile controls are:

```console
rot-audit . --features serde,cli
rot-audit . --no-default-features --features minimal
rot-audit . --all-features
rot-audit . --target aarch64-unknown-linux-gnu
rot-audit . --cfg loom
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

## Visibility graph

The driver records the compiler-resolved facts needed for one audit product:

- definitions and their nominal/effective visibility;
- source spans and whether a declaration is authored or generated;
- runtime and public-interface roots;
- resolved references between definitions;
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

## Findings

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
Visibility audit: complete (4/4 Cargo invocations correlated)
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

Visibility results are emitted only when all selected semantic evidence is
complete. The audit exits unsuccessfully when any of these conditions holds:

- the pinned driver is missing or fails its handshake;
- Cargo metadata or the unit graph cannot be obtained;
- Cargo/rustc does not complete successfully;
- an expected selected invocation is missing, duplicated, or ambiguously
  correlated;
- a sidecar is malformed, incomplete, or violates its integrity constraints;
- build-script cfg, target, feature, or codegen evidence disagrees;
- the reference graph cannot be closed safely.

When a structured report can be formed, its top-level `status` is `complete`,
`partial`, or `unavailable`, with a reason for non-complete evidence. Partial or
unavailable is never interpreted as zero required, narrowable, or dead
declarations, and the process still exits nonzero.

## JSON contract

`rot-audit --format json` emits a separate `schema_version: 1` report. Its main
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
diagnostics
```

The top-level report retains invocation identity and observed target/profile
facts so a result can be audited later. `required_visibility` and `closed_world`
both repeat the scope and exclusions at the point where they qualify the data.

Fast `rot --format json` remains schema version 2 and contains only source
metrics, profile information, file/role buckets, and diagnostics. It has no
`compiler` field.

## Deliberate non-goals

- The audit does not replace source line or authored-complexity measurement.
- It does not inventory an abstract stable library API.
- It does not assign maintainability complexity to derive or procedural-macro
  output.
- It does not enumerate the power set of Cargo features or target triples.
- It does not observe arbitrary external consumers of a published library.
- It does not compile doctests.

Use `rot` routinely for fast source metrics and revision comparisons. Use
`rot-audit` explicitly when aggressive visibility refactoring justifies a real
Cargo/rustc build.
