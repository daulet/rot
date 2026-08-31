# Agent feedback resolution

The fast/source-mode feedback is resolved. Rot now exposes one deterministic,
agent-oriented snapshot/comparison contract and documents the boundary between
source evidence and compiler evidence.

## Completed

- [x] Native revision comparison: `rot PATH --baseline REF` compares an exact
  committed tree with live Rust discovered under the same PATH-local ignore
  policy; repository-wide Git `dirty` status is reported separately. Lexical
  repository paths are resolved independently at both endpoints, so retargeted
  tracked symlinks compare the intended two targets and cannot escape either
  tree.
- [x] Compact JSON: `--format json --summary-only` removes only per-file records
  while preserving totals, profile provenance, diagnostics, and endpoint
  identity.
- [x] Self-describing JSON: both report kinds record normalized selected paths,
  file/directory kind, ignore boundary, hidden-path policy, and ignore policy.
- [x] Honest discovery admission: ignored or hidden Cargo targets and
  module-graph files may inform ownership traversal but cannot leak into
  reported metrics; `--no-ignore`, `--hidden`, and explicit file inputs are the
  documented opt-ins.
- [x] Actionable human comparison: before/after/delta/percent metrics, role
  changes, changed-file counts, and the ten largest role-aware contributors;
  `--files` expands the complete list.
- [x] Clean intentional feature exclusion: exclusion happens after feature
  closure, remains marked `synthetic` provenance, emits no diagnostic, and
  unknown selectors remain errors.
- [x] PATH-rooted Cargo feature closure: forward workspace dependencies,
  package-versus-alias selectors, optional/default/dependency features, and
  target-versus-host platform edges have Cargo-backed controls.
- [x] Explicit cfg preset: development is the default and `--release` changes
  only rustc's built-in `debug_assertions` default without claiming arbitrary
  Cargo profile resolution; JSON names the fast requested-target-global cfg
  approximation so cross-target host-unit source is not mistaken for Cargo
  evidence.
- [x] Help and README teach agent workflows, stdout/stderr and exit behavior,
  `--files` versus positional input, overlapping role file counts, and Rot's
  SCC-style-but-not-identical lexical metric.
- [x] Every stable rustc release published in the 365 days through 2026-08-29
  is supported by `rot-audit` on `aarch64-apple-darwin`: 1.90.0, 1.91.0,
  1.91.1, 1.92.0, 1.93.0, 1.93.1, 1.94.0, 1.94.1, 1.95.0, 1.96.0,
  1.96.1, 1.97.0, 1.97.1, and 1.98.0.

## Compiler compatibility evidence

`compiler/supported-rustc.toml` is the exact rolling ledger. Each of its 14
stable releases built its own rustc-private driver and passed 12 driver unit
tests plus 16 semantic visibility fixtures: 392/392 tests. Full Cargo-backed
audits also completed for all 14 stable identities with 4/4 correlated products
and no diagnostics; the default development nightly passed the same full
audit. Audit rejects an unlisted compiler identity or a mismatched driver;
support is never inferred from a newer compiler building older-compatible
source.

## References

- https://github.com/XAMPPRocky/tokei
- https://github.com/aldanial/cloc
- https://github.com/boyter/scc
- https://github.com/astral-sh/hawk
- https://github.com/rust-lang/rust/blob/master/RELEASES.md
