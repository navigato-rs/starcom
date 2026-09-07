# Contributing

Starcom follows [FileMan](https://github.com/kvark/fileman), its sister project.
The reference inspected for the initial plan is FileMan commit
`26499ebdb3d983190c61b9016a0ea31b2711aacf`.

## Code style

- Keep dependencies and code volume low. Simple is good; do not anticipate a
  plugin framework, workspace, or abstraction without a concrete need.
- One `use` per crate. Prefer modules over long lists of imported members; use
  qualified names when a list would not fit comfortably on one line.
- Do not rely on implicit references in `match`; use explicit `ref` bindings.
- Use enums instead of boolean arguments that obscure meaning.
- Keep I/O out of protocol, terminal-model, and state-transition tests.
- Use `anyhow` for application-level context; use explicit error types where
  callers need to distinguish recovery decisions.
- Never log keys, passwords, terminal input, or full remote output by default.

## Organization

Start with one Rust 2024 package, a small binary, and a reusable library. Split
modules rather than introducing multiple crates prematurely. Proposed modules
are listed in PLAN.md; create them when implemented, not as empty placeholders.
Use `tests/data/` for small synthetic fixtures, `scripts/` for development tools,
and `etc/` for desktop metadata (entry, SVG, PNG, ICO) and replay screenshots.
Icon rasters come from `scripts/generate-icons.py`.

Use FileMan's Blade/egui/winit versions together when introducing the GUI.
Do not copy image/archive/SFTP dependencies that Starcom does not use. Do not add
`eframe`, GPUI, or a second renderer just to get a window. Add SSH dependencies
only with the transport implementation. Commit Cargo.lock once dependency
resolution has actually been performed; do not invent lockfile entries.

## Workflow

Use a short-lived branch and pull request: protected `main` requires the five CI
checks before merge. Do not create extra milestone branches unless requested.
CI runs on pull requests and pushes to `main`, not on unreviewed branch pushes.
Preserve merged work before deleting an old branch.

Run these checks before submitting changes:

```sh
cargo fmt --check
cargo clippy --locked --all-features --all-targets -- -D warnings
cargo test --locked --all-features --all-targets
python3 scripts/check-dependencies.py
cargo deny --all-features check
```

CI should exercise Linux, macOS, and Windows clients. Tests requiring a real tmux
server run only on Linux and must use an isolated socket/session, never the
contributor's existing server. A tmux process surviving a connection loss is not
proof that terminal restoration works: test screen, modes, layout, and history.

Keep PLAN.md's implementation status honest. Separate implemented code, checks
actually run, and manual acceptance still outstanding. Never claim a performance
improvement from dependency counts or architecture alone.

## Security and compatibility

All remote output is untrusted. Keep it as bytes until the terminal parser;
escape diagnostics before printing them to another terminal. Bound framing,
reply, history, pending-command, and UI queues. Preserve host-key verification.
Do not weaken SSH authentication to make a test pass.

Never replay uncertain input across reconnects, silently create a replacement
session, or modify global tmux settings. Keep the ordinary tmux client usable as
a fallback. Changes affecting another attached client's geometry need an
explicit, tested sizing policy.

## Desktop validation

Default features now build the Blade/egui desktop. Keep protocol/terminal tests
usable with `--no-default-features`, and SSH-only tools with
`--no-default-features --features ssh`. GUI tests use egui input replay without
requiring a display. `--demo --snapshot PATH` renders the actual UI through Blade;
only demo data may be captured by that path. The Linux Xvfb smoke script tests
the native window and system clipboard. Do not equate these checks with native
macOS/Windows/Wayland/IME acceptance or call local viewport resizing remote pane
resizing. Keep CI triggers on PRs and main; no repository-writing preparation
jobs belong in the final workflow.

## Releasing

A `v*` tag builds and publishes a GitHub Release; nothing else does. Before
tagging:

1. Add a `## vX.Y.Z` section at the top of `CHANGELOG.md`. The workflow extracts
   exactly that section as the release notes and fails if it is missing, so a
   release cannot be published with an empty body.
2. Make sure `version` in `Cargo.toml` matches the tag.
3. Tag a commit whose CI is green. The release workflow builds artifacts; it
   does not re-run the test suite.

The workflow can also be run by hand from the Actions tab with the tag as an
input, which creates the tag at the dispatched commit if it does not exist yet.
That is how a release is re-run after a green fix without deleting and
re-pushing its tag, and the only route available where pushing a tag ref is not.

Retagging is still the way to fix a released commit: delete the release and the
tag, push the fix, and tag again. An asset uploaded twice under one name
replaces the first rather than appearing twice, so a partial re-run is safe.

## Patching a dependency

Sometimes a dependency is missing something Starcom needs. Carry that as a patch
file under `etc/<crate>/`, with a README stating what it changes and what was
measured.

Do not vendor a dependency's source into this repository. Pointing the default
build at a fork is a decision to record in PLAN.md, with the `deny.toml`
`allow-git` entry. Blade and Sunset are pinned by revision on `main`; Blade's
pin ends with a published crate, Sunset is the SSH stack. A patch nobody has
decided how to carry is a patch, not a dependency.

## Dependency advisories

Starcom uses the shared `sunset-client`, and still owns its dependency advisories.
`deny.toml` is checked on every change and on a weekly schedule, because an
advisory can land against a lockfile nobody touched.

An entry in `advisories.ignore` is an accepted risk, not a way to quiet CI. Each
one states why the exposure is tolerable and how it ends. Do not add one to make
a build pass; raise it instead. Release-candidate crypto dependencies are release
blockers tracked in PLAN.md, not permanent fixtures.

## Native dependency policy

Use Rust SSH and cryptography implementations. Do not add libssh2, OpenSSL,
ring, AWS-LC, or native crypto fallbacks through transitive features. The CI
policy script checks the resolved graph; builds also disable C/C++ compiler
invocation. This does not ban system linkers, OS FFI, graphics drivers, or Rust
build-helper crates whose C compilation path is disabled. Keep dependency
claims tied to the feature graph and actual builds, not a crate's marketing.
