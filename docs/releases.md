# Release automation

Rot releases from accepted commits, not from manually created tags. CI proves
one exact `main` commit and the release workflow computes and pushes the next
version commit with a dedicated release credential. That push receives its own
CI run. Only successful CI for the generated commit may create its annotated
tag, build artifacts, and publish channels.

## Version policy

The authoritative input is the first-parent commit history after the last
generated release commit:

- `feat: ...`, `feat(scope): ...`, and `feat!: ...` advance `MINOR` and reset
  `PATCH`;
- every other non-generated commit advances `PATCH`;
- if a batch contains any feature commit, the minor bump wins;
- major bumps are intentionally absent while Rot is greenfield;
- `chore(release): vX.Y.Z` is trusted only with the workflow's
  `Release-Source` and `Release-Automation: rot-v1` trailers.

The first run uses virtual version `0.0.0` and the complete first-parent
history. Rot's existing feature history therefore bootstraps `v0.1.0`, which
must match the checked-in package version. After that, the version in all three
manifests and both relevant lockfiles must match the last generated release.
Manual version drift fails closed.

Examples from a released `0.4.2` baseline:

| Unreleased subjects | Next version |
| --- | --- |
| `fix: correct cfg accounting` | `0.4.3` |
| `refactor: simplify walker` | `0.4.3` |
| `feat(cli): add threshold` | `0.5.0` |
| `fix: ...` and `feat: ...` | `0.5.0` |

Commit subjects on `main` are literal inputs. Prefer squash merges whose title
is a conventional commit; a generic `Merge pull request ...` subject is, by
definition, a patch.

## State machine and recovery

`.github/workflows/ci.yml` runs for pull requests and `main`. A successful
normal push CI run lets `.github/workflows/release.yml` create the version
commit. CI runs again for that credentialed push and calls the reusable
`.github/workflows/publish.yml` only after the exact release commit passes every
CI job. The planner/publisher pair:

1. scans complete first-parent history and finds the last provenance-marked
   release commit;
2. coalesces queued pushes by exiting when its source is no longer `main`;
3. updates package and lock versions and pushes a generated commit with
   `RELEASE_TOKEN`, which starts ordinary CI for that exact child commit;
4. resumes only from that successful exact-commit CI, validates the release
   commit's parent, committer, diff, manifests, dependencies, and locks, then
   creates the annotated tag;
5. builds all native artifacts from the proven release commit and attaches a
   deterministic canonical set to a draft GitHub release;
6. publishes crates.io, then makes the attested GitHub assets public;
7. installs and tests the Homebrew formula against those public URLs before
   updating the tap. The workflow's successful conclusion is the all-channel
   completion marker.

The workflow has one FIFO concurrency group with GitHub's maximum pending queue,
so planners and exact-commit publishers are never silently replaced. If a newer
commit lands before the version-commit push, the older run exits cleanly and
the newer run includes the whole unreleased range. If a channel fails
transiently, rerun the failed jobs before merging another commit. Recovery
stays attached to that exact release-commit CI identity, validates the
annotated tag, and accepts attached assets only when their exact names and
bytes match the current deterministic build.

Immediately before publication, the workflow byte-compares every draft asset
with the canonical Actions artifact and verifies its build-provenance
attestation. Homebrew checks the exact public asset set, checksum manifest, and
build-provenance attestation for every downloaded file before testing the
formula. This checks workflow provenance independently of GitHub's
release-level immutability attestation and does not depend on short-lived
Actions artifacts. A retry before GitHub publication still requires the
canonical artifact retained for seven days by the release run.

A deterministic failure does not wedge later releases. A follow-up commit uses
the failed generated version as its baseline and creates the next semantic
version; the failed version is burned. Once that successor release commit
exists, the older workflow is explicitly superseded and cannot publish on a
retry. Published crates are accepted on recovery only when their repository,
crate checksum, and configured crates.io owner all match. Homebrew refuses to
downgrade a newer formula. There is intentionally no manual publish dispatch
that could attest a different `GITHUB_SHA`.

## Published channels

The Cargo package is `rot-metrics`, while its executables remain `rot` and
`rot-audit`. `rot-compiler-protocol` is published first because the optional
audit feature depends on its exact version. `rot-rustc-driver` stays private.

GitHub builds the fast `rot` command natively:

| Runner | Rust target | Assets |
| --- | --- | --- |
| `macos-15` | `aarch64-apple-darwin` | tarball, Homebrew |
| `macos-15-intel` | `x86_64-apple-darwin` | tarball, Homebrew |
| `ubuntu-24.04-arm` | `aarch64-unknown-linux-musl` | static tarball, arm64 `.deb` |
| `ubuntu-24.04` | `x86_64-unknown-linux-musl` | static tarball, amd64 `.deb` |

Every archive includes `rot`, the README, linked release/audit documentation,
and both license texts. The release also contains `SHA256SUMS`; GitHub
provenance attestations cover the canonical archives, Debian packages, and
checksum file from the exact CI-proven release commit. Linux jobs install,
execute, and uninstall their `.deb` before publishing it.

The Cargo package exposes `rot-audit` behind the `audit` feature, but a complete
audit setup still requires a Git checkout to build the separate rustc driver.
GitHub/Homebrew/Ubuntu binaries intentionally contain only fast `rot`. The
driver is compiler-identity- and host-specific, and the current support ledger
does not prove all four release hosts. Do not advertise portable audit bundles
until each exact host matrix is recorded.

## One-time GitHub setup

For a new GitHub repository, complete this setup before enabling releases:

1. Create `daulet/rot`, add it as `origin`, and push `main`.
2. Create a GitHub environment named `release`. Required reviewers are
   optional; adding one turns every channel into an approval gate.
3. Add `RELEASE_TOKEN` as a repository secret. Use a fine-grained credential
   with contents write access only to `daulet/rot`; it pushes both generated
   version commits and annotated tags. Unlike `GITHUB_TOKEN`, its commit push
   must start CI. If `main` rejects direct pushes, grant only this release
   identity a narrow ruleset bypass. A dedicated GitHub App installation token
   is the stronger long-term replacement. Never permit force pushes.
4. Protect `v*` tags from update and deletion, permit the release identity to
   create them, and enable immutable GitHub releases before the first release.
   Draft assets remain replaceable during staging; publication then locks the
   tag and assets.
5. Add `HOMEBREW_TAP_TOKEN` to the `release` environment. Use a fine-grained
   credential with contents write access only to `daulet/homebrew-tap`.
6. Set repository variable `CRATES_IO_OWNER` to the exact crates.io owner login
   that must own both packages. Recovery rejects a matching crate uploaded by
   any other owner.
7. For the first crates.io publication only, add `CARGO_REGISTRY_TOKEN` to the
   environment. It must own the new `rot-metrics` and
   `rot-compiler-protocol` packages. After that release, configure crates.io
   trusted publishing for both packages with repository `daulet/rot`, workflow
   `ci.yml`, and environment `release`, then delete the bootstrap token. The
   OIDC claim names the calling CI workflow even though publication jobs live
   in reusable `publish.yml`.
8. Set repository variable `RELEASES_ENABLED=true`. Until this exact value is
   present, CI runs normally and every release job stays disabled.
9. Push one normally reviewed semantic commit. The complete feature history
   creates `v0.1.0`; later commits use the table above.

The permanent crates.io path uses a short-lived OIDC token. Every referenced
GitHub Action is pinned to a full commit SHA; update those pins deliberately,
with a reviewed dependency change.

## Local preflight

Run the same inexpensive checks before changing the workflow:

```console
python3 -m unittest discover -s .github/scripts/tests -v
bash -n .github/scripts/package-deb.sh
cargo fmt --all --check
cargo test --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo package -p rot-compiler-protocol --locked --allow-dirty
cargo package -p rot-metrics --list --allow-dirty
cargo package -p rot-metrics --locked --allow-dirty \
  --config 'patch.crates-io.rot-compiler-protocol.path="crates/rot-compiler-protocol"'
```

The local Cargo patch lets package verification resolve the exact protocol from
this checkout before that version exists on crates.io. The publish job still
rebuilds the final crate after the protocol version becomes visible, so its
uploaded checksum reflects the real registry dependency.
