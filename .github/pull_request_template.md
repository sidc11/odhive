## Summary

<!-- What does this PR change and why? Link related issues (e.g. "Closes #12"). -->

## Checklist

- [ ] Commits are signed off (`git commit -s`, DCO)
- [ ] Commit messages follow Conventional Commits
- [ ] Tests added or updated for the new behavior
- [ ] `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo nextest run` pass
- [ ] `pnpm lint`, `pnpm check`, `pnpm test` pass, and `pnpm e2e` for UI or API changes (if `web/` changed)
- [ ] Security self-check: no shell execution, no `unsafe`, no `{@html}`, paths canonicalized, secrets masked
- [ ] Docs updated if behavior changed
