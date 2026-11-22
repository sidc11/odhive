# Contributing to OdHive

Thanks for your interest in OdHive! This guide explains how to propose changes.

## Developer Certificate of Origin (DCO)

Every commit must be signed off, certifying that you wrote the change or have the
right to submit it under the project's license (see <https://developercertificate.org/>):

```sh
git commit -s -m "feat: add something"
```

This adds a `Signed-off-by: Your Name <you@example.com>` line. PRs with unsigned
commits cannot be merged.

## Workflow

1. Fork the repository and create a branch from `main` (e.g. `feat/log-filter`).
2. Make focused changes. Every change ships with tests.
3. Use [Conventional Commits](https://www.conventionalcommits.org/):
   `feat:`, `fix:`, `test:`, `docs:`, `ci:`, `chore:`, `refactor:`.
4. Open a pull request to `main`. CI must be green and a code owner must approve.
   `main` uses linear history; PRs are squash- or rebase-merged.

## Running checks locally

Rust:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
cargo deny check
```

Frontend (in `web/`):

```sh
pnpm install
pnpm lint
pnpm check
pnpm test
pnpm build
```

Fuzzing (optional locally, runs in CI; needs nightly and `cargo install cargo-fuzz`):

```sh
cargo +nightly fuzz run log_parser fuzz/corpus/log_parser \
  crates/odhive-core/src/logs/parse/fixtures -- -max_total_time=60
```

Use `mkdir -p fuzz/corpus/log_parser` first; the corpus is not committed.

## Security rules for contributions

- No `unsafe` code (`#![forbid(unsafe_code)]` in every crate).
- Never execute commands through a shell; use `Command::new` with an argument list.
- Never render untrusted content as HTML (`{@html}` is banned by lint).
- Canonicalize file paths and keep them inside allowed roots.
- Risky features must include tests that prove the bad case is rejected.

See [SECURITY.md](SECURITY.md) for reporting vulnerabilities.
