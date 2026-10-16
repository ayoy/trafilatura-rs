# Deviations from Go Source

This document tracks intentional differences between trafilatura-rs and the
[Go implementation](https://github.com/markusmobius/go-trafilatura) it was
ported from.

## Architecture

### Element handlers return HTML strings, not NodeIds

Go element handlers return a cloned `*etree.Element` that gets spliced into
the result tree. Rust handlers return `Option<String>` (serialized HTML
fragments) because ego-tree's `NodeId` is not transferable across different
`Document` instances.

**Files**: `src/extraction/elements.rs`

### In-place mutation with text/tail snapshots

Where Go clones elements before mutation, Rust works in-place and snapshots
text/tail content beforehand. If processing fails, the snapshot is restored.
This avoids unnecessary cloning while preserving identical behavior.

**Files**: `src/extraction/elements.rs`

### DOM text/tail model

The DOM layer uses Python's ElementTree text/tail concept (inherited from the
Go port of `internal/etree/`). This is architecturally faithful to Go but
requires explicit handling in a few places where Go's separate text nodes
behave differently (e.g., leading spaces between elements).

**Files**: `src/dom/tree.rs`

## Readability Fallback

### Different readability crate

trafilatura-rs uses the `readability` Rust crate (readeck port) instead of
go-readability (go-shiori). These produce different extraction results:

- Comment sections may be included where Go excludes them
- Navigation content may be extracted instead of article body
- Ad links may not be filtered as aggressively

This is the largest behavioral difference and accounts for the two ignored
integration tests:

- `test_extract_love_hina`: readability-rs includes comment sections
- `test_extract_rnz_witzel`: readability-rs extracts nav instead of article

### No dom-distiller fallback

Go's fallback pipeline includes dom-distiller as a third option. Rust omits
it — no suitable Rust equivalent is available. Fallback uses user-provided
candidates and readability only.

**Files**: `src/extraction/external.rs`

## Language Detection

### Fewer languages supported

The `whatlang` Rust crate covers ~69 languages; Go's `whatlanggo` covers ~87.
Language detection may not work for ~18 languages supported in Go. An
ISO 639-3 → 639-1 mapping table bridges available languages.

**Files**: `src/utils/language.rs`

## Date Extraction

### Full htmldate cascade (port of rust-htmldate / htmldate)

Date extraction is a port of the [rust-htmldate](https://crates.io/crates/rust-htmldate)
cascade (itself a port of Python's htmldate, Apache-2.0): meta elements,
JSON-LD, `<abbr>`, date-ish elements, `<title>`/`<h1>`, `<time>`, ISO
timestamps, og:image URLs, idiosyncratic "posted on" patterns, free-text
snippets, and a final raw-HTML scan (incl. copyright-year fallback), all
with newest-wins semantics (`UseOriginalDate = false`, the default).

On the 113-file test corpus this matches rust-htmldate 1.10.2 on every
file but one; the remaining difference (caktusgroup) is intentional:
like Python trafilatura, we feed `og:url`/canonical as the page URL and a
date in that URL path wins over later stages — rust-htmldate only sees
the URL the caller passes it.

`HtmlDateMode::Extensive` (the default) runs the full cascade;
`HtmlDateMode::Fast` restricts to meta + JSON-LD and the fast date-element
subset, as before.

Known approximation: the date-element selector reads `id`/`class`/
`itemprop` attributes the way upstream does, but upstream takes the first
attribute *present* in source order while our DOM (scraper default
features) sorts attributes alphabetically, losing source order. We check
all present attributes, which is over-inclusive when an element's
non-first attribute carries a date hint; a real corpus shows this loses
fewer dates than any deterministic ordering of the sorted attributes.

**Files**: `src/metadata/dates.rs`, `src/metadata/mod.rs`

## Output Format

### Apostrophe escaping

Go serializes apostrophes as `&#39;`; html5ever leaves them unescaped. Both
are valid HTML — the difference is cosmetic.

## Test Adjustments

### `test_extract_schleifen_ucoz`

Removed the `"Aufrufe:"` assertion. Go extracts this page-view counter via
go-readability, but the Python original does not. Rust aligns with Python
behavior.
