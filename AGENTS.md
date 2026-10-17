# AGENTS.md

Rust port of [go-trafilatura](https://github.com/markusmobius/go-trafilatura) (itself a port of Python trafilatura): HTML main-content and metadata extraction, plus uniffi bindings (Swift/Kotlin/Ruby/Dart/C#/JS) in `uniffi/`.

## Commands

```sh
cargo test                                      # full suite (fixtures + doc tests)
cargo test --lib metadata::dates                # fast loop for date-cascade work
cargo fmt --check && cargo clippy --lib --all-targets
(cd uniffi/wasm && cargo check)                 # wasm32 build (restores Cargo.lock after)
cargo bench                                     # criterion: single_document / corpus / modes
cargo test --test comparison_test -- --nocapture  # 960-entry quality scores
make test-js                                    # or test-swift/test-kotlin/... for bindings
```

There is no `check` script; run the fmt/clippy/wasm lines above after changes.

## Layout

- `src/extraction/` — main content extraction (go-trafilatura port)
- `src/metadata/` — title/sitename/author/date; `dates.rs` is the full htmldate cascade (port of rust-htmldate, Apache-2.0)
- `src/dom`, `src/selector`, `src/utils` — shared HTML stack (html5ever/scraper)
- `test-files/{mock,comparison,simple}/`, `comparison-data/` — fixtures and the eval dataset

## Parity contract

Behavior parity with the upstream ports is the bar, not code similarity. Every intentional deviation must be documented in `DEVIATIONS.md`. For date work, compare against rust-htmldate sources in `~/.cargo/registry/src/` (probe in a scratch cargo project when unsure) — not just against our own tests.

## Platform gates

- `chrono::Local` is unavailable on wasm32-unknown-unknown: gate every use behind `#[cfg(not(target_arch = "wasm32"))]` with a UTC fallback (see `observe_unix`, `plausible_year`, `is_plausible_date`).
- Tests must not pin the `TZ` env var: chrono ignores it on Windows, and `set_var` races sibling test threads. Inject `FixedOffset` at a seam instead.

## Releases

One tag does everything: pushing `vX.Y.Z` triggers five workflows — crates.io (`publish.yml`) plus Maven Central, NuGet, RubyGems, and Swift packages. All publish steps are tag-conditional; ordinary branch pushes never publish.

1. Bump `version` in `Cargo.toml` (refresh `Cargo.lock` via `cargo check`). Minor bump for new public API or changed defaults, patch otherwise.
2. Commit `Release vX.Y.Z` with the rationale; annotated tag; push `main` + tag together.
3. The crates.io publish is version-guarded (tag must match `Cargo.toml`), so bindings-only tags skip it; `workflow_dispatch` re-runs a single registry.
4. Watch `gh run list`, then `gh release create vX.Y.Z` — notes style follows previous releases, ending with the `vA...vB` compare link.

## Conventions

- MSRV 1.88, edition 2021.
- Commits: imperative subject, body explaining what and why (see `git log`).
- Perf numbers in README tables must be re-measured with `cargo bench`, never extrapolated. Machine drift is ±2%: use warm A/B/A runs before believing small deltas; prefer per-benchmark medians.
