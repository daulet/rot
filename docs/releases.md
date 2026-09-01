# Release automation

Rot releases accepted `main` commits. Tags are generated identities, not
release triggers or version inputs.

## Version policy

The Rust planner in `.github/rot-release` compares the selected source tree
with the newest valid tagged release commit:

- no net change is `unchanged`;
- a net diff containing only Markdown is `markdown-only`;
- a net diff confined to `.github`, `docs`, licenses, repository config,
  examples, benches, or tests is `release-neutral`;
- every other or unknown path is release-relevant.

The first three states do not release. For a release-relevant range, each
first-parent commit is classified separately. A release-neutral commit is
excluded from versioning; `feat:`, `feat(scope):`, and their breaking forms
bump `MINOR`; every other included commit bumps `PATCH`. Any feature wins for
the range. Major bumps are intentionally absent while Rot is greenfield.

Examples from `0.4.2`:

| Unreleased changes | Result |
| --- | --- |
| `fix: correct cfg accounting` | `0.4.3` |
| `refactor: simplify walker` | `0.4.3` |
| `feat(cli): add threshold` | `0.5.0` |
| product fix plus docs-only `feat` | `0.4.3` |
| Markdown, tests, or release tooling only | no release |
| code added and fully reverted | no release |

This split is deliberate: the gate uses the net tree, while the bump uses the
surviving commit history. Unknown and mixed paths fail toward releasing.

## Release flow

After normal CI succeeds, `.github/workflows/plan-release.yml`:

1. plans from complete first-parent history;
2. exits if a newer `main` commit already owns the range;
3. updates the workspace version and exact protocol dependency in
   `Cargo.toml`, plus the root and rustc-driver lockfiles;
4. pushes a provenance-marked `chore(release)` commit with `[skip ci]` and an
   annotated tag;
5. dispatches `.github/workflows/release.yml` for that exact tagged `main`
   commit and follows it to completion.

The generated cargo-dist workflow verifies the tag and commit, builds four
native fast-mode archives, uses cargo-deb for amd64 and arm64 Debian packages,
publishes `rot-compiler-protocol` then `rot-metrics`, attests the release files,
creates the GitHub release, and finally updates Homebrew. The
private rustc driver is never published.

The planner and distributor are serialized. While the generated commit remains
the `main` tip, a retry reuses its tag, already-published crates, and
byte-identical Homebrew formula. It refuses duplicate distribution runs, moved
or lightweight tags, formula downgrades, and same-version formula drift. Rerun
failed jobs on the existing distribution run. If work continues instead, the
failed version is burned and the next accepted commit receives a new version.

## Published channels

The Cargo package is `rot-metrics`; the installed fast executable is `rot`.
GitHub releases contain `.tar.gz` archives for Apple arm64/x86_64 and static
Linux musl arm64/x86_64, their SHA-256 files, a combined checksum file, the
Homebrew formula, and Debian packages for arm64/amd64. cargo-dist generates the
archives, checksums, formula, release notes, and GitHub release; cargo-deb owns
Debian metadata and packaging.

`rot-audit` remains a Cargo feature requiring a host- and rustc-specific driver
built from a Git checkout. It is not part of the portable binary archives.

## Repository setup

Before enabling automation:

1. Add repository secret `RELEASE_TOKEN`, scoped to contents write on
   `daulet/rot`. Its identity needs a narrow direct-push bypass for generated
   version commits and protected `v*` tags; never allow force pushes.
2. Create the `release` environment, restricted to `main`, and add
   `HOMEBREW_TAP_TOKEN` with contents write only on `daulet/homebrew-tap`.
3. Configure crates.io trusted publishing for both packages with repository
   `daulet/rot`, workflow `release.yml`, and environment `release`. A
   `CARGO_REGISTRY_TOKEN` environment secret is supported only as bootstrap;
   delete it after trusted publishing works.
4. Protect `v*` tags from update/deletion, enable immutable GitHub releases,
   and set repository variable `RELEASES_ENABLED=true`.

`RELEASE_TOKEN` does not need Actions permission: the planner dispatches with
its short-lived `GITHUB_TOKEN`. Every referenced action and distribution tool
version is pinned in source.

## Local checks

```console
cargo fmt --manifest-path .github/rot-release/Cargo.toml -- --check
cargo test --manifest-path .github/rot-release/Cargo.toml --all-targets --locked
cargo clippy --manifest-path .github/rot-release/Cargo.toml \
  --all-targets --all-features --locked -- -D warnings
dist generate --check

cargo fmt --all --check
cargo test --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo package -p rot-compiler-protocol --locked --allow-dirty
cargo package -p rot-metrics --locked --allow-dirty \
  --config 'patch.crates-io.rot-compiler-protocol.path="crates/rot-compiler-protocol"'
```
