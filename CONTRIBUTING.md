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
your `git config user.name` / `user.email`. **Nothing in CI checks
this today** — no workflow inspects commit trailers — so a maintainer
will ask you to re-sign before merge rather than a red check telling
you first.

If you forget, amend with:

    git commit --amend -s --no-edit

Or sign off a whole branch:

    git rebase --signoff main

## Pull request checklist

- [ ] `cargo fmt --all --check` passes
- [ ] `cargo clippy --all-targets --locked -- -D warnings` passes
- [ ] `cargo clippy --all-targets --locked --features "multiviewer video-encoder-x264" -- -D warnings`
      passes (needs `libx264-dev`)
- [ ] `cargo test` passes in every project that has tests
- [ ] New public APIs have rustdoc
- [ ] Tests cover new behaviour
- [ ] All commits are `Signed-off-by`
- [ ] Root `CLAUDE.md` and project-level `CLAUDE.md` are still
      accurate if your change affects documented architecture

Those two clippy lines are what CI runs, verbatim. **`--all-features`
is deliberately not runnable** and should not be substituted: it
selects `sdi-decklink`, `rga-transfer` and the `*-rkmpp` backends,
whose build scripts panic without the EULA-gated Blackmagic DeckLink
SDK headers (`DECKLINK_SDK_DIR`) and Rockchip `librga` /
`rockchip_mpp`.

The SDI code is compile-gated separately by
`cargo check --all-targets --locked --features sdi-decklink`, which
runs only when a DeckLink SDK credential is available to the run and
is therefore **skipped on fork PRs**. If your change touches
`src/engine/sdi_io.rs` or `src/engine/output_sdi.rs`, say so in the
PR description — a green check set on a fork does not mean that code
compiled.

## Questions

Open an issue before starting non-trivial work so we can align on
scope and approach. For licensing questions, email
`contact@bilbycast.com`.
