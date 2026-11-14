# Contributing to till

Thanks for helping out. till is a payment broker sidecar: it stores card
details encrypted at rest, gets human approval over Discord, and fills the
card fields into a Camofox browser tab — without card digits ever entering
an agent's model context, the event bus, or any log.

This repo is primarly written by AI, but all code is human-reviewed.

**Read [`AGENTS.md`](AGENTS.md) before you write code.** It holds the
non-negotiable security rules (card data never logged, filled only via
`/tabs/:id/evaluate`, submit clicked exactly once, fields masked before any
screenshot, no card data without explicit owner approval) and the repository
layout. The design itself is in [`docs/SPEC.md`](docs/SPEC.md); read it
before touching the fill path. When the two disagree, the security rules in
`AGENTS.md` win.

## Development setup

The workspace is a Rust workspace (edition 2024) with four crates:
`broker`, `camofox-client`, `discord-gw`, and `mock-checkout`.

1. **Install Rust with [rustup](https://rustup.rs/).** Do not install a
   distro-packaged `rustc`/`cargo`; rustup is what honours the pinned
   toolchain.
2. **Clone and build.** `rust-toolchain.toml` pins the toolchain to the
   `stable` channel and requests the `rustfmt` and `clippy` components.
   rustup reads it automatically, so the first `cargo` invocation in this
   directory will download the right toolchain if you don't already have it:

   ```bash
   git clone git@github.com:mplorentz/till.git
   cd till
   cargo build --workspace
   ```

   You do not need to run `rustup toolchain install` or `rustup override`
   by hand — the file is the single source of truth for the toolchain, and
   CI uses the same stable channel. If `cargo --version` reports something
   unexpected, check that you are inside the repository directory and that
   `rustup show` lists the `stable` toolchain.

3. **Check the components are present.** `rustup component list --installed`
   should include `rustfmt` and `clippy`. If they are missing, run
   `rustup component add rustfmt clippy`.

The crates are currently scaffolds with no external dependencies, so
`cargo build` needs no system packages. If that changes — sqlite for
`crates/broker`, TLS for outbound calls — this section will be updated
alongside the dependency.

The Hermes-side plugin in `plugin/` is Python, not Rust; see
[`plugin/README.md`](plugin/README.md). The deployment stack in `docker/` is
documented in [`docker/README.md`](docker/README.md).

## Running the checks

`./script/checks` is the repository's definition of "CI passes": it runs the
same three checks as [`.github/workflows/ci.yml`](.github/workflows/ci.yml)
(CI adds `--locked` to the test step and uses the same pinned stable
toolchain):

```bash
./script/checks
```

That script runs, in order:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Run it before every push; a red `./script/checks` means a red CI run. To
iterate faster, run the individual steps:

```bash
cargo fmt --all                 # apply formatting instead of just checking it
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p broker            # a single crate
cargo test -p broker <name>     # a single test, by name substring
```

Clippy warnings are errors in CI (`-D warnings`) and the workspace lints
also deny `unsafe_code` and warn on `unwrap_used`/`expect_used` — prefer
propagating errors to unwrapping.

To exercise the checkout flow locally, `cargo run -p mock-checkout` starts
the four-mode test site (`success`, `declined`, `ambiguous`, `short_hold`)
with a `/stats` submission counter.

## Branches and pull requests

- **Never commit directly to `main`.** All work happens on a branch and
  lands through a pull request; `main` is what CI builds and what deploys.
- Branch from an up-to-date `main` and give the branch a short, descriptive
  kebab-case name, e.g. `add-contributing-md` or
  `broker-submit-once-latch`.
- Keep pull requests focused on one change. Small, reviewable PRs are much
  easier to check against the security rules, which is where most of the
  review effort goes.
- Base PRs against `main` and make sure `./script/checks` is green locally
  before opening one. CI runs the same checks, so a failure there is
  almost always reproducible on your machine. If you changed any
  `Cargo.toml`, run `cargo test --workspace --locked` once so CI's locked
  build sees the updated `Cargo.lock`.
- In the PR description, say what changed and why, and call out anything
  that touches card data, network egress, the storage path, or the payment
  state machine — those changes get closer review and need a justification
  for any new dependency.
- The state machine is the product. When a change alters a decision, encode
  it as a state transition with a test rather than as a comment, and add
  that test to the PR.

## Where to look next

| Document | What it covers |
|---|---|
| [`AGENTS.md`](AGENTS.md) | Agent instructions, non-negotiable security rules, layout, commands |
| [`docs/SPEC.md`](docs/SPEC.md) | The canonical design, user stories, and state machine |
| [`README.md`](README.md) | What till is and how the pieces fit together |
| [`plugin/README.md`](plugin/README.md) | The Hermes-side Python plugin and its tool contracts |
| [`docker/README.md`](docker/README.md) | Deployment on the Mac Mini |

License: AGPL-3.0-only. By contributing you agree your work is licensed
under the same terms.
