# Contributing to bilbycast-edge

Thanks for considering a contribution. Before you open a pull
request, a few logistics:

## Licensing

bilbycast-edge is **dual-licensed**:

- **AGPL-3.0-or-later** for open-source use (see [LICENSE](LICENSE)).
- **Commercial licence** from Softside Tech Pty Ltd for OEMs and
  commercial integrators who need to avoid AGPL's copyleft (see
  [LICENSE.commercial](LICENSE.commercial)).

By contributing, you agree that your contribution can be distributed
under both licences. We confirm this through the Developer
Certificate of Origin sign-off described in [DCO.md](DCO.md).

## Sign off every commit

Use `git commit -s` — this appends a `Signed-off-by:` line using
your `git config user.name` / `user.email`. CI rejects unsigned
commits.

If you forget, amend with:

    git commit --amend -s --no-edit

Or sign off a whole branch:

    git rebase --signoff main

## Pull request checklist

- [ ] `cargo fmt --all --check` passes
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
      passes (or the CI equivalent)
- [ ] `cargo test` passes in every project that has tests
- [ ] New public APIs have rustdoc
- [ ] Tests cover new behaviour
- [ ] All commits are `Signed-off-by`
- [ ] Root `CLAUDE.md` and project-level `CLAUDE.md` are still
      accurate if your change affects documented architecture

## Questions

Open an issue before starting non-trivial work so we can align on
scope and approach. For licensing questions, email
`commercial@softsidetech.com`.
