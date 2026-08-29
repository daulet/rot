# Compiler-backed Rust analysis

Status: source/HIR architecture implemented behind `--compiler`; selective
THIR and normalized MIR products remain deferred

This document describes how `rot` adds compiler-backed analysis without
giving up its fast, source-backed mode. It covers the compiler representations
we could consume, the measurements each representation enables, the cost of
syntax and semantic analysis, configuration profiles, production/test
ownership, and aggregation by physical files and directories.

The central design decision is:

> Keep the current lossless source analysis as the authority for physical
> lines, comments, blank lines, authored complexity, inactive source, and
> synthetic profiles. Use a pinned rustc helper to add configuration-specific
> semantic facts and expansion-only decisions.

HIR should be the primary compiler representation. THIR should be queried only
for measurements that genuinely require typed executable bodies. MIR should be
an optional CFG view, not the default complexity metric.

## Implemented architecture

The implementation keeps the fast source pass intact and adds a separate,
exactly pinned compiler pass. Cargo's unstable unit graph is the expected-unit
ledger: target kind, compile mode, feature closure, platform, profile, and
host/target context must match the collected rustc invocation. Build-script cfg
is joined by the invocation's canonical `OUT_DIR`, not unioned by package.

Protocol v3 carries bounded, atomic per-invocation sidecars with stable compiler
definition IDs, finite public bindings, typed roots/references, expansion
provenance, editable-visibility evidence, and product-local availability. The
stable main binary contains no rustc-private types.

The landed HIR products are:

- effective API definitions and finite namespace bindings for production
  libraries and proc macros;
- compiler-required public visibility and selected-workspace closed-world
  liveness, with production and non-production roots evaluated per concrete
  invocation configuration;
- expansion-only macro body bases and normalized decision deltas.

Runtime liveness deliberately preserves invocation identity so normal and test
fragments cannot form an impossible path. Visibility-edit findings use a
separate physical-span equivalence relation so one authored `pub` token included
in several compilation contexts is changed only when every occurrence is safe.

## Goals

- Resolve the current syntax-only API uncertainties: re-exports, glob exports,
  inherent items on externally nameable types, and items produced by macros.
- Confirm concrete active bodies and measure expansion-only complexity with
  explicit treatment of Rust desugarings and macro expansions.
- Optionally expose a normalized control-flow-graph metric.
- Preserve package, Cargo-target, source-span, module/item, and source-role
  provenance so semantic facts can be related to the physical source report
  without turning compiler spans into LOC.
- Use Cargo's actual feature, target, build-script, proc-macro, and custom-cfg
  behavior for realizable profiles.
- Preserve useful output when a project does not compile or a requested profile
  is intentionally synthetic.
- Keep rustc-private API churn isolated from the report model and source engine.

## Non-goals

- Replacing the source lexer with HIR, THIR, or MIR. These representations do
  not retain ordinary comments or blank lines.
- Enumerating the power set of Cargo features. Each feature selection is a
  different program, and the number of combinations is unbounded in practice.
- Pretending that a synthetically excluded feature is a compilable Cargo
  profile when another feature implies it.
- Supporting arbitrary rustc versions with one compiler-driver binary.
- Treating raw MIR block counts as a stable source-quality metric.
- Proving that a public API is useful to downstream crates. Effective
  visibility is compiler-resolvable; external usefulness requires observing
  consumers or defining an additional policy.

## Existing source-backed mode

The existing mode remains useful even after a compiler mode exists.

| Concern | Existing authority | Property |
|---|---|---|
| Physical, code, comment, doc, and blank lines | Lossless Rust syntax tokens | Reads every file once and retains comments and whitespace |
| Lexical complexity | Rust tokens | SCC-compatible definition without SCC's lexer mistakes |
| Authored cyclomatic and cognitive complexity | Lossless Rust AST | Preserves authored constructs, nesting, inactive code, and synthetic profiles |
| `cfg` and feature ownership | Symbolic syntax evaluation | Can describe inactive and intentionally synthetic source |
| Module/file reachability | Cargo metadata plus syntax module edges | Works without compiling dependencies or running project code |
| Explicit unrestricted `pub` declarations | Syntax visibility | Physical declaration count only; no semantic API claim |

Compiler analysis augments these facts. It must not silently change the
meaning of `lexical_complexity`, `cyclomatic_authored`, or
`cognitive_authored`; expanded and CFG products need distinct names.

## Compiler representations

### rustc AST

rustc has an internal AST before and after expansion. It can provide the exact
grammar accepted by the selected compiler, but it is not a good long-term
analysis boundary for `rot`:

- Before expansion it does not close the macro and name-resolution gaps that
  motivate compiler mode.
- After expansion, inactive `cfg` source is gone and authored syntax has begun
  to change shape.
- Ordinary comments have already been treated as whitespace; doc comments have
  become attributes.
- It is an unstable rustc-private API while duplicating much of what the
  existing lossless syntax layer already does.

We may use AST callbacks for diagnostics or experiments, but should not make
the report contract depend on rustc AST structures.

### HIR: primary semantic layer

HIR is produced after parsing, macro expansion, name resolution, and built-in
desugaring. It includes the crate's item hierarchy and executable bodies.

HIR enables:

- Resolved modules, definitions, imports, re-exports, and glob imports.
- Effective visibility and externally reachable item analysis through compiler
  privacy/visibility queries.
- Correct ownership of inherent methods once the receiver type is resolved.
- Items and implementations generated by declarative, derive, attribute, and
  procedural macros.
- Compiler-confirmed active bodies and decisions originating in macro
  expansion.
- Authored-versus-expanded attribution through source spans and expansion
  metadata.
- Stable semantic ownership keys based on crate and definition paths rather
  than filenames alone.

HIR is not identical to source syntax. For example, a source `for` loop is
lowered into matches and a loop, and `?` is represented by a tagged match.
The source AST therefore owns authored decisions. The adapter uses lowering
and expansion provenance only for compiler-confirmed activation and
expansion-only events; it must not emit a competing authored total.

HIR is sufficient for the initial compiler-backed complexity and API slices.

### THIR: selective typed-body enrichment

THIR is constructed after type checking and represents executable bodies, not
the crate's declaration hierarchy. It makes more implicit operations explicit:
method calls and overloaded operators become calls, adjustments are applied,
and patterns carry resolved type information.

THIR can add:

- Type-aware pattern and enum-variant analysis.
- Call-site categories after method/operator resolution.
- Typed complexity policies, if we decide that a decision's type matters.
- A normalized executable-body view for functions, closures, constants, and
  static initializers.

THIR does not contain structs, traits, modules, or the public interface as a
whole. It is also temporary inside rustc and more heavily lowered than HIR.
Using it for every metric would add compiler coupling without improving the
ordinary source cyclomatic score. Query it per body only when a metric states a
type-dependent requirement.

### MIR: optional control-flow graph

MIR represents a body as basic blocks, statements, and terminators. It enables
measurements that HIR and THIR do not directly expose:

- Nodes, normal edges, exits, and branch fanout.
- Graph-theoretic cyclomatic complexity.
- Loop backedges and strongly connected regions.
- Normal versus unwind/cleanup paths.
- Diverging calls, assertions, yields, and coroutine behavior.

Raw MIR is unsuitable as the headline complexity number. Its shape depends on
the MIR phase, panic strategy, optimization settings, async/coroutine lowering,
drop elaboration, bounds and overflow checks, and compiler-version changes.

A normalized CFG mode must define all of the following:

- Use one named MIR phase and record it in provenance.
- Traverse only blocks reachable from the body entry.
- Add one synthetic exit when applying `E - N + 2P` to multiple returns.
- Exclude imaginary `FalseEdge` and `FalseUnwind` edges.
- Exclude cleanup and unwind edges by default; report exceptional complexity
  separately if requested.
- Decide whether explicit `assert!`, implicit bounds checks, overflow checks,
  `yield`, and coroutine state transitions count.
- Attribute compiler-generated blocks back to source decisions where possible.

This metric should be named `cfg_complexity`, not replace source cyclomatic
complexity.

### Cost and maintenance comparison

| Layer | Rust grammar maintenance | Semantic precision | Runtime class | Main maintenance risk |
|---|---|---|---|---|
| Existing lossless syntax | Upgrade the parser dependency and review new syntax kinds | Source-faithful; macros and names unresolved | Source scan | New syntax lowering into an unclassified syntax node |
| rustc AST | Compiler parses supported syntax | Low before expansion; moderate after expansion | Compiler frontend | Unstable API without closing the important semantic gaps |
| HIR | Compiler parses and expands supported syntax | High for ownership, visibility, and source decisions | Approximately `cargo check` | HIR API and desugaring changes |
| THIR | Compiler parses, expands, and type-checks | High for typed executable bodies | Type-checking plus per-body construction | Temporary API and additional lowering changes |
| MIR | Compiler produces the CFG | Highest for one concrete lowered program | MIR construction and selected passes | Graph drift across phases, flags, and compiler versions |

rustc removes the need to teach `rot` how to parse each new grammar feature. It
does not remove the need to decide how a new semantic construct contributes to
the metric.

## Complexity products

The source AST emits authored body and decision facts; the compiler adapter
emits stable active-body and expansion-only events rather than a competing
total:

```text
DecisionKind = conditional | loop | match | match_alternative | guard
             | short_circuit | try | let_else
DecisionOrigin = authored | builtin_desugaring | local_macro | external_macro
Decision = { body, kind, origin, span, nesting }
```

The report layer can derive several products from the same events:

- `lexical_complexity`: the existing zero-based SCC-compatible token score.
- `cyclomatic_authored`: base one per body plus authored decisions, with Rust
  desugarings normalized and external macro bodies excluded.
- `macro_expansion_cyclomatic_delta`: compiler-confirmed macro body bases and
  decisions, kept separate until exact authored-body/profile correlation can
  justify a combined expanded score.
- `cognitive_authored`: an explicitly versioned nesting-weighted policy.
- `cfg_complexity`: a normalized graph measurement from MIR.

Keeping the event breakdown makes compiler upgrades reviewable. A changed
total should identify which decision kind, origin, body, and span changed.

## API-surface products

HIR plus compiler visibility and resolution can add evidence-backed semantic
categories that the source pass deliberately does not approximate:

- Declared `pub` items.
- Effectively externally reachable items.
- Re-exported items and the public paths that expose them.
- Glob re-exports after expansion.
- Public inherent items whose receiver is externally reachable.
- Public trait items and implementations.
- Items generated by macros.
- Exported signature spans and the resolved types appearing in them.

Definition and exposure are different locations. A re-export should produce:

- one definition fact attributed to the defining file; and
- one public-path edge attributed to the `pub use` file.

Directory totals must not duplicate the definition merely because it has
multiple public paths. Public-path counts may be reported separately.

Compiler visibility still does not prove Hawk-style "unnecessary public" or
"dead export" conclusions for arbitrary external users. We can make those
claims only under a declared closed-world scope. The implemented scope is
`selected-workspace compiled-target closed world`: it excludes doctests and
Cargo targets skipped because their `required-features` are inactive. Those
limits are serialized as `evidence_exclusions`, so consumers can reject the
whole finding set when either omitted role matters.

The closed-world graph fails conservative when expansion removes a selected
consumer's original namespace spelling. A directly exported namespaced
`pub macro` therefore keeps its containing module public; `#[macro_export]`
does not keep its definition module public because the exported path is at the
crate root. Public inherent associated types are also required-public roots on
the pinned compiler, and their interface edges retain the self type and RHS.
This is deliberate until rustc exposes complete type-position qualified-path
resolution outside body type-checking results.

## Syntax and semantic cost

### Source mode

The source mode reads and parses workspace Rust files in parallel. It does not
compile dependencies, run build scripts, or execute procedural macros. Its
cost is primarily proportional to source bytes and is appropriate as the
default interactive command.

### HIR mode

Producing useful HIR semantics requires Cargo/rustc to perform expansion and
name resolution. Effective visibility and resolved body facts generally need
analysis to progress far enough that the operational cost resembles
`cargo check`, not a line scanner.

Costs and side effects include:

- Cargo dependency resolution and possibly dependency downloads.
- Compilation or loading of dependencies and proc-macro crates.
- Execution of build scripts and procedural macros.
- Writes to a Cargo target directory.
- Failure when the selected program does not compile far enough.
- Compiler memory proportional to the crates being analyzed.

Warm Cargo/incremental caches can reduce repeated cost, but compiler mode must
never be marketed with source-scan latency.

### THIR and MIR modes

THIR requires type checking and is constructed per body. MIR construction adds
lowering and, depending on the selected query, borrow checking and transform
passes. Their incremental cost may be modest once a compiler session has
already reached analysis, but their maintenance and semantic-normalization cost
is materially higher than HIR.

### Trust boundary

Compiler mode executes project-controlled build scripts and proc macros. It is
explicit rather than silently replacing the default command. The CLI offers
Cargo-equivalent `--locked` and `--offline` controls and an isolated-artifact
parent through `--compiler-target-dir`.

## Compiler integration boundary

rustc's internal crates are unstable and tied to an exact compiler build. Keep
that dependency in a small helper:

```text
rot
  source inventory, LOC, symbolic cfg, aggregation, reporting
    |
    | versioned event stream
    v
rot-rustc-driver
  pinned nightly + rustc-dev (and llvm-tools where required), Cargo rustc
  wrapper and HIR adapter; selective THIR/MIR work is deferred
```

The main report model contains no rustc types, numeric HIR IDs, or debug
renderings. The helper translates them into a small versioned protocol whose
core facts include:

```text
CrateInvocation { package, target, mode, target_triple, feature_set }
ActiveRange     { file, byte_range, body, origin }
Decision        { body, kind, origin, span, nesting }
PublicItem      { definition, visibility, definition_span, signature_spans }
PublicPath      { definition, path, exposure_span }
Diagnostic      { phase, severity, span, message }
```

Cargo's workspace rustc wrapper intercepts selected workspace compilations
while ordinary dependency compilation proceeds normally. An exact Cargo unit
graph supplies the expected invocation ledger. Each sidecar is matched by Cargo
unit identity, artifact metadata, compilation context, and canonical output
directory; target roles are never guessed from filenames.

The current protocol-v3 helper pins `nightly-2026-08-27` at rustc commit
`bff8e12ff5e6bcd53dfb1dbccdcec80a60a856ed` and exchanges bounded atomic JSONL
sidecars with the stable main binary. Supporting the
project-selected arbitrary toolchain would require building or shipping a
matching helper per compiler and is not an initial goal. If a project requires
newer syntax than the pin supports, report the mismatch and retain source-mode
results.

## Configuration profiles

A compiler result describes one concrete compilation profile, not "the
project" in the abstract. Its identity must include at least:

```text
package
Cargo target
target triple
resolved feature closure
normal/test/bench mode
relevant rustc cfg values
panic and compiler settings that affect MIR
rustc commit/version
report schema version governing the metric definitions
```

### Cargo features

For realizable profiles, pass ordinary Cargo controls:

- default features;
- `--no-default-features`;
- an explicit `--features` set; or
- `--all-features`.

Cargo computes the actual closure. The compiler events must record that
resolved set rather than only the requested flags.

The existing `--exclude-feature` can describe profiles Cargo cannot construct.
For example, feature `a` may imply feature `b` while the user requests all
features except `b`. The source engine can force the `b` predicate false and
mark the report synthetic. Cargo and rustc cannot honestly compile that profile
without rewriting the manifest or lying about cfg values.

Compiler mode therefore follows this rule:

- If the requested feature profile is realizable, compile it.
- If an exclusion is already absent from Cargo's closure, compile normally.
- If an exclusion contradicts the closure, retain source results, mark semantic
  facts unavailable for that profile, and explain why.
- Never rewrite user manifests to manufacture a compiler profile.

`--all-features` can itself fail when a project intentionally defines mutually
exclusive features. That is a compiler diagnostic, not a reason to fall back
to an invented semantic result.

### Targets and custom cfg

Each target triple is another concrete profile. Compiler mode automatically
observes rustc built-ins and build-script-emitted cfg values for that
invocation. A requested target may require its standard library or other target
support to be installed.

Adding a custom `--cfg` can be represented by rustc flags. Arbitrarily removing
a built-in or build-script-emitted cfg generally cannot. Synthetic `--unset-cfg`
profiles remain source-only unless the requested state has a faithful compiler
option, such as a corresponding codegen setting.

We should analyze explicit named profiles, not attempt every target and feature
combination.

The capability boundary is:

| Requested profile | Source mode | Compiler mode |
|---|---|---|
| Ordinary Cargo feature closure | Exact symbolic source view | Exact concrete compiler view |
| Feature forced off despite being implied | Synthetic but explicit | Unavailable without manifest rewriting |
| Unknown custom cfg | Preserved as conditional | Concrete if Cargo/build scripts supply it |
| Arbitrary cfg forced false | Synthetic but explicit | Usually unavailable |
| Inactive source | Visible and countable | Removed before HIR |
| Source with parse errors | Partial source metrics and diagnostics | Unavailable past the compiler failure |

Multiple profiles are separate report dimensions. Their physical LOC must not
be summed, because the same file can participate in every profile.

## Production, test, and other target roles

Production versus test is not a permanent property of a file. It is the
difference between compilation modes under the same target and feature
profile.

For each selected library or binary target, collect semantic events from:

1. A normal compilation, where `cfg(test)` is false.
2. Its unit-test compilation, where Cargo passes test mode and `cfg(test)` is
   true.

Let `P` be normal events and `T` be test-mode events, keyed by stable body,
source span, decision kind, and origin:

```text
production/shared = P
test-only          = T - P
production-only    = P - T
shared             = P intersect T
```

The default disjoint report continues to count shared code as production.
Optional diagnostics can expose `production-only` and `shared` explicitly.

Additional roles come from Cargo metadata and the actual compiler invocation:

- Integration tests are separate test targets, regardless of their pathname.
- Benchmarks, examples, and build scripts retain the existing distinct report
  roles. Libraries, binaries, and proc-macro crates are normal production
  targets while retaining their exact Cargo target kind in provenance.
- A custom test target outside `tests/` is still a test.
- A file named `tests.rs` is not necessarily test-only.
- Doctests require a separate rustdoc-backed path and are deferred from the
  first compiler mode.

Compiler expansion recognizes custom attribute macros that generate tests, but
their source attribution may be only the attribute callsite. The source mode's
configurable test-attribute list remains useful when compiler mode is absent.

## Physical lines, comments, and active source

HIR, THIR, and MIR do not emit physical/comment/blank line counts. Ordinary
comments disappear before these representations, and doc comments are lowered
to attributes. The current lossless token pass remains authoritative.

For every physical line, source analysis continues to record:

```text
has_code
has_comment
has_doc_comment
symbolic production/test reachability
```

Compiler facts are overlaid by filename and byte range. They may confirm which
code tokens are active in a concrete profile, but compiler spans must not be
summed to produce LOC: spans overlap, can be broad, and may represent generated
code.

The mutually exclusive line policy remains:

- any code token makes the line code;
- otherwise a comment token makes it a comment;
- otherwise it is blank;
- docs are a subset of comments.

Comments inside a clearly gated source node can inherit that node's role.
Standalone comments between differently gated items follow their enclosing
module/file role rather than being guessed from the nearest item.

## Files and directories

HIR and THIR nodes carry spans. MIR statements and terminators carry source
information. rustc's `SourceMap` resolves these positions into a source file,
byte range, line, and column.

The merger should maintain two independent hierarchies:

```text
physical: workspace-relative directories and files
semantic: package -> Cargo target -> module -> item/body
```

They are intentionally separate because Rust supports `#[path]`, literal
`include!`, generated `OUT_DIR` source, custom Cargo target paths, and multiple
module paths involving the same physical file.

### File identity and deduplication

- Normalize real source files against the Cargo workspace/package root.
- Preserve a source hash and original/remapped identity where needed for
  compiler sessions using `--remap-path-prefix`.
- Count physical LOC once per file.
- In the physical view, merge identical semantic events across normal, test,
  and overlapping Cargo target invocations by source span, decision kind, and
  origin while retaining their role activations.
- Retain an optional compiled-instance view when one source file is deliberately
  included or compiled multiple times; do not let it inflate physical totals.
- Attribute API definitions to their definition file and public-path edges to
  their re-export file.

Directory aggregation is then a prefix-tree sum over unique physical files.
Semantic module aggregation is a separate sum over definition ownership.

Each directory node can reuse the existing disjoint role buckets:

```text
DirectoryReport {
    path,
    unique_files,
    production: { lines, complexity, API },
    test:       { lines, complexity, API },
    bench, example, build, conditional, inactive, orphan,
}
```

Directory parents sum child files exactly once. Compiled-instance counts, if
exposed, live in a separate semantic report and never alter these physical
totals.

### Macro and generated locations

rustc distinguishes real and virtual source files and retains macro expansion
metadata. Not every generated node has a precise physical location.

Use an explicit origin policy:

- `authored`: walk expansion spans back to their source callsite and charge the
  decision to that physical file.
- `expanded`: preserve macro-generated decisions and group nodes without a real
  file under a virtual `<generated>/<kind>` hierarchy.
- `all`: additionally retain dependency-definition locations where available.

Derive and procedural macros can emit dummy, mixed-site, or coarse callsite
spans. The report must surface unattributed generated facts rather than invent
line precision. Included files under `OUT_DIR` should be labeled generated even
when rustc gives them a real path.

## Reporting and provenance

Source and compiler products remain additive and independently available:

```text
lexical_complexity
cyclomatic_authored
cognitive_authored
compiler.products[effective_api].status
compiler.products[required_visibility].status
compiler.products[closed_world_liveness].status
compiler.products[macro_expansion_cyclomatic_delta].status
compiler.macro_expansion_complexity  # invocation-local deltas
```

Each compiler product status is `complete`, `partial`, or `unavailable`.
`cfg_complexity` remains a deferred MIR product rather than a placeholder zero.

Every compiler-backed report records:

- exact rustc version/commit and driver protocol version;
- report schema version plus each product's named metric, scope, and
  baseline/policy fields;
- target triple and Cargo target;
- requested and resolved features;
- normal/test/other compilation mode;
- authored/expanded macro policy;
- compiler diagnostics and the phase reached;
- whether build scripts and proc macros ran;
- whether results are complete or source-only fallback.

Raw or normalized MIR metrics should not be compared across compiler versions
unless the report explicitly confirms a compatible report schema and
metric-specific MIR phase/policy.

## Maintainability

rustc handles grammar recognition, but it does not maintain `rot`'s metric
definition. New Rust behavior can appear in three ways:

1. A new HIR/THIR/MIR variant makes the driver fail to compile after a toolchain
   update. This is visible and desirable.
2. New syntax lowers into existing nodes. The driver compiles but can silently
   overcount or undercount unless provenance fixtures cover the construct.
3. Existing syntax lowers to a different MIR graph. A raw graph score changes
   without a source change.

Keep compiler enum handling exhaustive where possible. Where rustc requires a
wildcard for a non-exhaustive type, emit an explicit unclassified event or
diagnostic instead of silently assigning zero complexity.

The upgrade gate for the pinned compiler should include:

- one fixture per decision kind and Rust desugaring;
- normal/test diffs, including `cfg(test)` and `cfg(not(test))`;
- default, disabled, enabled, and mutually exclusive features;
- target cfg and build-script-emitted cfg;
- declarative, derive, attribute, and procedural macros;
- `#[path]`, literal `include!`, generated files, and remapped paths;
- public uses, glob re-exports, inherent items, and macro-generated APIs;
- event-level golden comparisons before total comparisons;
- representative real-workspace smoke runs.

## Delivery status

1. **Source-authored complexity — complete**
   - Emit AST body and decision facts for authored Rust.
   - Add cyclomatic and cognitive products while preserving lexical complexity.

2. **Source API ownership cut — complete**
   - Retain declared visibility only in fast mode.
   - Delete the approximate effective-export collector and unresolved counters.

3. **Pinned HIR driver and Cargo boundary — complete**
   - Pin one nightly and build a workspace rustc wrapper.
   - Emit crate invocations, real source identities, body identities, and
     per-product availability.
   - Prove file attribution and normal/test correlation on existing fixtures.

4. **Effective API surface — complete**
   - Resolve public uses, globs, receiver reachability, and macro-produced
     items.
   - Retain definition and public-path locations separately.
   - Emit no semantic zero when compiler status is incomplete.

5. **Profiles, required visibility, and closed-world liveness — complete**
   - Drive normal and test compilations for realizable feature/target profiles.
   - Retain stable source spans and target-role provenance while keeping
     compiler semantics separate from physical LOC totals.
   - Make synthetic profiles explicitly source-only.

6. **Expansion-only complexity — complete as an explicit delta**
   - Add HIR decisions originating in declarative and procedural expansion.
   - Keep them separate as `macro_expansion_cyclomatic_delta` until exact
     authored-body/profile correlation exists.

7. **Selective THIR enrichment — deferred**
   - Add only measurements with an approved type-dependent definition.

8. **Optional MIR CFG — deferred**
   - Specify the normalized graph contract before implementation.
   - Report raw graph facts for diagnosis and normalized complexity separately.

## References

- [rustc_driver and rustc_interface](https://rustc-dev-guide.rust-lang.org/rustc-driver/intro.html)
- [`rustc_private` requirements and stability](https://doc.rust-lang.org/beta/unstable-book/language-features/rustc-private.html)
- [HIR](https://rustc-dev-guide.rust-lang.org/hir.html)
- [AST-to-HIR lowering](https://rustc-dev-guide.rust-lang.org/hir/lowering.html)
- [THIR](https://rustc-dev-guide.rust-lang.org/thir.html)
- [MIR construction](https://rustc-dev-guide.rust-lang.org/mir/construction.html)
- [MIR queries and phases](https://rustc-dev-guide.rust-lang.org/mir/passes.html)
- [rustc source maps](https://doc.rust-lang.org/nightly/nightly-rustc/rustc_span/source_map/struct.SourceMap.html)
- [rustc real and virtual filenames](https://doc.rust-lang.org/nightly/nightly-rustc/rustc_span/enum.FileName.html)
- [Cargo workspace rustc wrappers](https://doc.rust-lang.org/cargo/reference/config.html#buildrustc-workspace-wrapper)
- [Cargo unit graph](https://doc.rust-lang.org/cargo/reference/unstable.html#unit-graph)
- [Cargo features](https://doc.rust-lang.org/cargo/reference/features.html)
- [Cargo targets and test mode](https://doc.rust-lang.org/cargo/reference/cargo-targets.html)
- [Hawk closed-world visibility analysis](https://github.com/astral-sh/hawk)
