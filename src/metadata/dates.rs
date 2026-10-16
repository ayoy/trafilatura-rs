//! Full htmldate search cascade for publication-date extraction.
//!
//! Port of the go-htmldate / rust-htmldate extraction pipeline
//! (Apache-2.0, Markus Mobius et al.), adapted to this crate:
//! dates only (no time/timezone), `chrono::NaiveDate` instead of
//! zoned datetimes, and no dateutil dependency — numeric, month-name
//! and free-text parsing are reimplemented with `regex` + `chrono`.
//!
//! Cascade order (matching upstream defaults, `UseOriginalDate=false`
//! i.e. newest-wins):
//! URL → meta elements → JSON-LD → `<abbr>` → date-ish elements →
//! title/h1 → `<time>` → raw-HTML timestamp → `og:image` URL →
//! idiosyncratic patterns → free text → raw-HTML page scan.
//! Fast mode stops after the cheap DOM stages (no free text,
//! no raw-HTML scan).

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use chrono::{Datelike, NaiveDate};
use regex::Regex;

use crate::dom::{Document, NodeId};

use super::{examine_meta_date, json_search_date};

/// Entry point: run the date-extraction cascade.
///
/// `url` is the page URL (used for URL-path dates), if known.
/// `extensive` enables free-text and raw-HTML scanning (upstream
/// `skip_extensive_search = false`); when false, the date-element
/// scan also restricts itself to free-text tags.
pub(super) fn extract_date(
    doc: &Document,
    url: Option<&str>,
    extensive: bool,
) -> Option<NaiveDate> {
    // 1. Date in the URL path (e.g. /2019/02/10/post.html).
    if let Some(date) = url.and_then(url_date) {
        return Some(date);
    }
    // 2. Meta elements and 3. JSON-LD (ported earlier from go-htmldate).
    if let Some(date) = examine_meta_date(doc, extensive) {
        return Some(date);
    }
    if let Some(date) = json_search_date(doc) {
        return Some(date);
    }
    // 4. <abbr> elements (unix stamps, published classes).
    if let Some(date) = abbr_dates(doc, extensive) {
        return Some(date);
    }
    // 5. Elements whose tag/id/class/itemprop look date-ish. Upstream
    // removes embedded/foreign subtrees before this stage.
    if let Some(date) = date_elements(doc, !extensive) {
        return Some(date);
    }
    // 6. <title>/<h1> text.
    if let Some(date) = title_dates(doc, extensive) {
        return Some(date);
    }
    // 7. <time> elements.
    if let Some(date) = time_element_dates(doc, extensive) {
        return Some(date);
    }
    // 8-11 need the serialized source (the <html> element, mirroring
    // upstream; `root()` itself is a Document node that serializes empty).
    let html = serialize(doc);
    // 8. ISO timestamp with clock time anywhere in the source.
    if let Some(date) = timestamp_date(&html) {
        return Some(date);
    }
    // 9. Date in the og:image URL.
    if let Some(date) = og_image_date(doc) {
        return Some(date);
    }
    // 10. Idiosyncratic "posted on …" patterns (EN/DE/TR).
    if let Some(date) = idiosyncrasy_date(&html) {
        return Some(date);
    }
    if !extensive {
        return None;
    }
    // 11. Newest date across candidate free-text snippets.
    if let Some(date) = free_text_date(doc) {
        return Some(date);
    }
    // 12. Last resort: scan the whole source with looser patterns.
    page_date(&html)
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parses a date out of an arbitrary text snippet (upstream `tryDate`):
/// normalize whitespace, bound the length, guard the digit count, discard
/// improbable strings, then try fast numeric, day-first, month-name and
/// year-month forms. With `extensive`, fall back to the external-parser
/// surface (upstream `external_parse`) when all of those fail. Note the
/// discard rules reject bare four-digit years — year-only dates only
/// surface through the raw-HTML page scan.
pub(super) fn parse_candidate(text: &str, extensive: bool) -> Option<NaiveDate> {
    let text = normalize(text);
    let text = limit(&text, 52);
    let digits = text.chars().filter(|c| c.is_ascii_digit()).count();
    if !(4..=18).contains(&digits) || is_discarded(text) {
        return None;
    }
    super::fast_parse_date(text)
        .or_else(|| extended_parse(text))
        .or_else(|| extensive.then(|| external_parse(text)).flatten())
}

/// English filler words whose dateparser translations are empty: the parser
/// drops them anywhere in the input before matching (case-insensitively),
/// which is how "on 1-21-2019" parses like the bare triple.
const IGNORABLE_WORDS: [&str; 13] = [
    "about", "ad", "and", "at", "by", "from", "just", "nd", "of", "on", "rd", "st", "the",
];

/// dateparser's tokenizer is ASCII-only: ASCII digits (and `:`) form
/// digit runs, ASCII letters form letter runs, and every other
/// character — punctuation, symbols, and non-ASCII letters/digits like
/// `é` or `𝑡` — forms a standalone "other" run. Runs at a token's edge
/// never break a date candidate, so `"1-21-2019."`, `"(1-21-2019)"` or
/// `"21st. 1 2019"` parse like their bare cores (a trailing `:` is
/// tolerated too: upstream keeps it inside the digit run / time
/// token). Interior separators stay intact: the separator choice still
/// groups the numbers. (A non-ASCII letter can additionally flip
/// upstream's locale-detected parse order — see DEVIATIONS.md.)
fn trim_edge_noise(token: &str) -> &str {
    token.trim_matches(|c: char| !c.is_ascii_alphanumeric())
}

/// `"21st"` → `"21"`: dateparser splits digit and letter runs into
/// separate tokens and drops an ordinal suffix directly after a number
/// (observed case-insensitively, like the filler words).
fn strip_ordinal_suffix(token: &str) -> Option<&str> {
    let head_len = token.len().checked_sub(2)?;
    if !token.is_char_boundary(head_len) {
        return None;
    }
    let (head, tail) = token.split_at(head_len);
    let is_suffix = ["st", "nd", "rd", "th"]
        .iter()
        .any(|suffix| tail.eq_ignore_ascii_case(suffix));
    (!head.is_empty() && head.bytes().all(|b| b.is_ascii_digit()) && is_suffix).then_some(head)
}

fn strip_ignorable_words(text: &str) -> Option<String> {
    let mut changed = false;
    let stripped: Vec<String> = text
        .split_whitespace()
        .filter_map(|token| {
            let trimmed = trim_edge_noise(token);
            if trimmed.is_empty() {
                changed = true;
                return None;
            }
            let lowered = trimmed.to_lowercase();
            if IGNORABLE_WORDS.contains(&lowered.as_str()) {
                changed = true;
                return None;
            }
            if let Some(digits) = strip_ordinal_suffix(trimmed) {
                changed = true;
                return Some(digits.to_string());
            }
            if trimmed.len() != token.len() {
                changed = true;
                return Some(trimmed.to_string());
            }
            Some(token.to_string())
        })
        .collect();
    (changed && !stripped.is_empty()).then(|| stripped.join(" "))
}

/// The language-independent numeric surface of upstream's external
/// parser (rust-dateparser, strict, past-preferring): month-first triples
/// the day-first cascade structurally cannot see (its month slot tops out
/// at 19), space/comma triples resolved in either order, an optional
/// trailing clock time. Gated like upstream on a separator character in
/// the text; rust-dateparser's multi-language month names stay unported.
fn external_parse(text: &str) -> Option<NaiveDate> {
    // Upstream's `text_date` guard: the text contains a separator
    // (the character class `[ -/]` covers `.`, `-`, `/` and space).
    if !text.chars().any(|c| matches!(c, ' '..='/' | ':' | '_')) {
        return None;
    }
    if let Some(stripped) = strip_ignorable_words(text) {
        if let Some(date) = external_parse(&stripped) {
            return Some(date);
        }
    }
    let caps = MDY_RE.captures(text)?;
    let leading = caps.name("lead")?.as_str().parse::<u32>().ok()?;
    let trailing = caps.name("trail")?.as_str().parse::<u32>().ok()?;
    let year_text = caps.name("year")?.as_str();
    // dateparser requires a positive year: a four-digit 0000 is invalid,
    // while the two-digit form resolves through `correct_year`.
    if year_text.len() == 4 && year_text.parse::<i32>().ok()? == 0 {
        return None;
    }
    let year = correct_year(year_text.parse::<i32>().ok()?);
    // Upstream groups triples by separator: dash/slash/dot binds the
    // numbers into one component (the day-first flip stays allowed even
    // when the second separator is a space or comma — "21-1 2019"),
    // while a space/comma first and dash/slash/dot second splits it
    // ("21 1-2019" is rejected).
    let spaced = |name: &str| {
        caps.name(name)
            .is_some_and(|sep| sep.as_str().contains(' ') || sep.as_str().contains(','))
    };
    let flip_allowed = !spaced("sep") || spaced("sep2");
    // Month-first; flip to day-first when the leading number cannot be
    // a month.
    let (month, day) = if leading <= 12 {
        (leading, trailing)
    } else if flip_allowed {
        (trailing, leading)
    } else {
        return None;
    };
    date_from_parts(year, month, day)
}

/// Day-first numeric, month-name and year-month forms beyond
/// [`super::fast_parse_date`].
fn extended_parse(text: &str) -> Option<NaiveDate> {
    // Day-first numeric: D.M.YYYY / D-M-YY / D. M . YYYY (upstream `ymd` alt).
    if let Some(caps) = DMY_RE.captures(text) {
        let mut day = caps[1].parse::<u32>().ok()?;
        let mut month = caps[2].parse::<u32>().ok()?;
        let year = correct_year(caps[3].parse::<i32>().ok()?);
        // Recover US-ordered dates when the month slot cannot be a month
        // (upstream `swap_parts`).
        swap_parts(&mut day, &mut month);
        if let Some(date) = date_from_parts(year, month, day) {
            return Some(date);
        }
    }
    // Month-name forms: "January 13, 2020" / "15. Januar 2015".
    if let Some((day, month, year)) = month_name_parse(text) {
        if let Some(date) = date_from_parts(year, month, day) {
            return Some(date);
        }
    }
    // Year-month: 2019/06 → first of month; or 06/2019.
    if let Some(caps) = YM_RE.captures(text) {
        let (year, month) = if caps.get(1).is_some() {
            (caps[1].parse::<i32>().ok()?, caps[2].parse::<u32>().ok()?)
        } else {
            (caps[4].parse::<i32>().ok()?, caps[3].parse::<u32>().ok()?)
        };
        if let Some(date) = date_from_parts(year, month, 1) {
            return Some(date);
        }
    }
    None
}

/// Matches "Month D, YYYY" or "D (of|.) Month YYYY" with multi-language
/// month names; returns (day, month, year).
fn month_name_parse(text: &str) -> Option<(u32, u32, i32)> {
    static PATTERNS: LazyLock<[Regex; 2]> = LazyLock::new(|| {
        let month = MONTH_NAMES;
        [
            Regex::new(&format!(r"(?i)({month})[\t\n\x0c\r ]([0-3]?[0-9])(?:st|nd|rd|th)?,?[\t\n\x0c\r ](199[0-9]|20[0-3][0-9])")).unwrap(),
            Regex::new(&format!(r"(?i)([0-3]?[0-9])(?:st|nd|rd|th|\.)?[\t\n\x0c\r ](?:of[\t\n\x0c\r ])?({month})[,.]?[\t\n\x0c\r ](199[0-9]|20[0-3][0-9])")).unwrap(),
        ]
    });
    let (index, caps) = PATTERNS
        .iter()
        .enumerate()
        .filter_map(|(index, re)| re.captures(text).map(|caps| (index, caps)))
        .min_by_key(|(index, caps)| {
            (
                caps.get(0).unwrap().start(),
                Reverse(caps.get(0).unwrap().len()),
                *index,
            )
        })?;
    let month = *MONTHS
        .get(caps[if index == 0 { 1 } else { 2 }].to_lowercase().as_str())
        .unwrap_or(&0);
    let day = caps[if index == 0 { 2 } else { 1 }].parse::<u32>().ok()?;
    let (mut day, mut month) = (day, month);
    swap_parts(&mut day, &mut month);
    let year = correct_year(caps[3].parse::<i32>().ok()?);
    Some((day, month, year))
}

/// Upstream `fastParse` for a bare URL: /YYYY/MM/DD/ path segment.
fn url_date(url: &str) -> Option<NaiveDate> {
    let caps = URL_DATE_RE.captures(url)?;
    date_from_parts(
        caps[1].parse::<i32>().ok()?,
        caps[2].parse::<u32>().ok()?,
        caps[3].parse::<u32>().ok()?,
    )
}

/// ISO date followed by a clock time (upstream `timestamp`), e.g.
/// `"datePublished":"2019-01-07T17:27:31+02:00"` anywhere in the source.
fn timestamp_date(html: &str) -> Option<NaiveDate> {
    let caps = TIMESTAMP_RE.captures(html)?;
    super::fast_parse_date(&caps[1])
}

/// "Posted on 3.2.2019" / "Stand: 08.05.2019" / Turkish update stamps.
fn idiosyncrasy_date(html: &str) -> Option<NaiveDate> {
    static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        [
            r#"(?i)(?:date[^0-9"]{0,20}|updated|last-modified|published|posted|on)[ :]*([0-9]{1,4})[./]([0-9]{1,2})[./]([0-9]{2,4})"#,
            r"(?i)(?:Datum|Stand|Ver[\x{00f6}\x{00d6}]ffentlicht am):? ?([0-9]{1,2})\.([0-9]{1,2})\.([0-9]{2,4})",
            r"(?i)(?:g[\x{00fc}\x{00dc}]ncellen?me|yay[\x{0131}I](?:m|n)lan?ma) *(?:tarihi)? *:? *([0-9]{1,2})[./]([0-9]{1,2})[./]([0-9]{2,4})",
            r"(?i)([0-9]{1,2})[./]([0-9]{1,2})[./]([0-9]{2,4}) *(?:['\x{2019}](?:de|da|te|ta)|tarihinde) *(?:g[\x{00fc}\x{00dc}]ncellendi|yay[\x{0131}I][mn]land[\x{0131}I])",
        ]
        .iter()
        .map(|p| Regex::new(p).unwrap())
        .collect()
    });
    let (_, caps) = PATTERNS
        .iter()
        .enumerate()
        .filter_map(|(index, re)| re.captures(html).map(|caps| (index, caps)))
        .min_by_key(|(index, caps)| {
            (
                caps.get(0).unwrap().start(),
                Reverse(caps.get(0).unwrap().len()),
                *index,
            )
        })?;
    if caps[1].len() == 4 {
        // Y/M/D form.
        date_from_parts(
            caps[1].parse::<i32>().ok()?,
            caps[2].parse::<u32>().ok()?,
            caps[3].parse::<u32>().ok()?,
        )
    } else if matches!(caps[3].len(), 2 | 4) {
        let mut day = caps[1].parse::<u32>().ok()?;
        let mut month = caps[2].parse::<u32>().ok()?;
        swap_parts(&mut day, &mut month);
        date_from_parts(correct_year(caps[3].parse::<i32>().ok()?), month, day)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// DOM stages
// ---------------------------------------------------------------------------

/// `<abbr>` elements: `data-utime` unix stamps, `published` classes.
fn abbr_dates(doc: &Document, extensive: bool) -> Option<NaiveDate> {
    let elements = doc.get_elements_by_tag_name(doc.root(), "abbr");
    if elements.is_empty() || elements.len() >= 1000 {
        return None;
    }
    let mut reference = Reference {
        extensive,
        ..Reference::default()
    };
    for &id in &elements {
        let utime = doc.get_attribute(id, "data-utime").unwrap_or_default();
        let utime = utime.trim();
        if !utime.is_empty() {
            if let Ok(seconds) = utime.parse::<i64>() {
                reference.observe_unix(seconds);
            }
        } else if matches!(
            doc.get_attribute(id, "class").unwrap_or_default().trim(),
            "published" | "date-published" | "time-published"
        ) {
            let title = doc.get_attribute(id, "title").unwrap_or_default();
            let title = title.trim();
            if !title.is_empty() {
                // Newest-wins: observe the title, stop once we have a date.
                reference.observe(title);
                if reference.date.is_some() {
                    break;
                }
            } else {
                let text = normalize(&doc.text(id));
                if text.chars().count() > 10 {
                    let text = text.strip_prefix("am ").unwrap_or(&text);
                    reference.observe(text);
                }
            }
        }
    }
    reference
        .finish()
        .or_else(|| first_element_date(doc, &elements, extensive))
}

/// Elements whose tag/id/class/itemprop look date-ish (footer, byline,
/// "date", "datum", "published", …): first parse wins in document order.
/// With `fast`, only free-text tags qualify (upstream `dateElement`).
fn date_elements(doc: &Document, fast: bool) -> Option<NaiveDate> {
    let mut elements = Vec::new();
    collect_wanted(
        doc,
        doc.root(),
        &mut elements,
        &|doc, id| is_date_element(doc, id, fast),
        0,
    );
    if elements.is_empty() || elements.len() >= 1000 {
        return None;
    }
    first_element_date(doc, &elements, !fast)
}

/// Same treatment for `<title>`/`<h1>` text.
fn title_dates(doc: &Document, extensive: bool) -> Option<NaiveDate> {
    let mut elements = Vec::new();
    collect_wanted(
        doc,
        doc.root(),
        &mut elements,
        &|doc, id| matches!(doc.tag_name(id), "title" | "h1"),
        0,
    );
    if elements.is_empty() || elements.len() >= 1000 {
        return None;
    }
    first_element_date(doc, &elements, extensive)
}

/// `<time>` elements: `datetime` attribute (shortcut for `updated` class
/// under newest-wins) or element text; newest date wins.
fn time_element_dates(doc: &Document, extensive: bool) -> Option<NaiveDate> {
    let mut elements = Vec::new();
    collect_wanted(
        doc,
        doc.root(),
        &mut elements,
        &|doc, id| doc.tag_name(id) == "time",
        0,
    );
    if elements.is_empty() || elements.len() >= 1000 {
        return None;
    }
    let mut reference = Reference {
        extensive,
        ..Reference::default()
    };
    for &id in &elements {
        let datetime = doc.get_attribute(id, "datetime").unwrap_or_default();
        let datetime = datetime.trim();
        let class = doc
            .get_attribute(id, "class")
            .unwrap_or_default()
            .trim()
            .to_string();
        if datetime.chars().count() > 6 {
            // UseOriginalDate=false: only the "updated" class is a shortcut.
            if class == "updated" {
                if let Some(date) = parse_candidate(datetime, extensive) {
                    return Some(date);
                }
            } else {
                reference.observe(datetime);
            }
        } else {
            let text = normalize(&doc.text(id));
            if text.chars().count() > 6 {
                reference.observe(&text);
            }
        }
    }
    reference.finish()
}

/// Tries full text content and `title` attribute of each element in order
/// (upstream `otherElements`; full subtree text like lxml `text_content()`
/// over the pruned tree).
fn first_element_date(doc: &Document, elements: &[NodeId], extensive: bool) -> Option<NaiveDate> {
    for &id in elements {
        for text in [
            pruned_text_content(doc, id, 0),
            doc.get_attribute(id, "title").unwrap_or_default(),
        ] {
            let normalized = normalize(&text);
            if normalized.chars().count() <= 6 {
                continue;
            }
            let shortened = limit(&normalized, 52)
                .trim_end_matches(|c: char| !c.is_ascii_digit())
                .to_string();
            if let Some(date) = parse_candidate(&shortened, extensive) {
                return Some(date);
            }
        }
    }
    None
}

// Depth cap for the recursive DOM walks in this module, matching the
// parser's MAX_TREE_DEPTH so pathological nesting cannot overflow the stack.
const MAX_WALK_DEPTH: usize = 500;

/// Subtree text like [`Document::text_content`], but skipping
/// embedded/foreign subtrees: upstream removes unwanted elements from
/// the tree before the DOM scanning stages, so their text is invisible
/// to those stages.
fn pruned_text_content(doc: &Document, id: NodeId, depth: usize) -> String {
    if depth >= MAX_WALK_DEPTH {
        return String::new();
    }
    let mut text = String::new();
    for child in doc.child_nodes(id) {
        if doc.is_element(child) {
            if !is_unwanted(doc, child) {
                text.push_str(&pruned_text_content(doc, child, depth + 1));
            }
        } else if doc.is_text(child) {
            text.push_str(&doc.text_content(child));
        }
    }
    text
}

/// Walks elements in document order, skipping embedded/foreign subtrees
/// (upstream removes these before the DOM scanning stages), collecting
/// elements matching `predicate`.
fn collect_wanted(
    doc: &Document,
    id: NodeId,
    out: &mut Vec<NodeId>,
    predicate: &dyn Fn(&Document, NodeId) -> bool,
    depth: usize,
) {
    if depth >= MAX_WALK_DEPTH {
        return;
    }
    for child in doc.child_nodes(id) {
        if doc.is_element(child) && !is_unwanted(doc, child) {
            if predicate(doc, child) {
                out.push(child);
            }
            collect_wanted(doc, child, out, predicate, depth + 1);
        }
    }
}

/// Embedded/foreign elements pruned before the DOM scanning stages
/// (upstream `find`).
fn is_unwanted(doc: &Document, id: NodeId) -> bool {
    let tag = doc.tag_name(id);
    matches!(
        tag,
        "object"
            | "embed"
            | "applet"
            | "frame"
            | "frameset"
            | "noframes"
            | "iframe"
            | "label"
            | "map"
            | "math"
            | "audio"
            | "canvas"
            | "datalist"
            | "picture"
            | "rdf"
            | "svg"
            | "track"
            | "video"
    ) || (tag == "div" && matches!(doc.id_attr(id).as_str(), "wm-ipp-base" | "wm-ipp"))
}

/// Upstream `dateElement` filter: footer/small tags, date/datum/meta/
/// publish/time substrings in id/class/itemprop, byline-ish classes.
fn is_date_element(doc: &Document, id: NodeId, fast: bool) -> bool {
    let tag = doc.tag_name(id);
    if tag == "footer" || tag == "small" {
        return true;
    }
    if fast && !is_free_text_tag(tag) {
        return false;
    }
    // Upstream reads the first attribute *present* among these names in
    // source order. Our DOM sorts attributes alphabetically, so source order
    // is unrecoverable; checking all present attributes is the closest
    // deterministic approximation (see DEVIATIONS.md) and errs inclusive.
    let id_attr = doc.id_attr(id);
    let class = doc.class_name(id);
    let itemprop = doc.get_attribute(id, "itemprop").unwrap_or_default();
    let id_class = format!("{id_attr} {class}");
    let dateish = format!("{id_attr} {class} {itemprop}").replace('D', "d");
    dateish.contains("date")
        || dateish.contains("datum")
        || id_class.replace('M', "m").contains("meta")
        || ["time", "publish", "footer"]
            .iter()
            .any(|part| id_class.contains(part))
        || [
            "info",
            "post_detail",
            "block-content",
            "byline",
            "subline",
            "posted",
            "submitted",
            "created-post",
            "publication",
            "author",
            "autor",
            "field-content",
            "fa-clock-o",
            "fa-calendar",
            "fecha",
            "parution",
        ]
        .iter()
        .any(|part| class.contains(part))
        || id_attr.contains("footer-info-lastmod")
}

/// Upstream `freeTextTag` filter.
fn is_free_text_tag(tag: &str) -> bool {
    matches!(
        tag,
        "div" | "h2" | "h3" | "h4" | "li" | "p" | "span" | "time" | "ul"
    )
}

/// Newest date across direct text children of free-text containers
/// (div/h2-h4/li/p/span/time/ul), skipping embedded/foreign subtrees.
fn free_text_date(doc: &Document) -> Option<NaiveDate> {
    // Stage 11 only runs in extensive mode.
    let mut reference = Reference {
        extensive: true,
        ..Reference::default()
    };
    walk_free_text(doc, doc.root(), &mut reference, 0);
    reference.finish()
}

fn walk_free_text(doc: &Document, id: NodeId, reference: &mut Reference, depth: usize) {
    if depth >= MAX_WALK_DEPTH {
        return;
    }
    for child in doc.child_nodes(id) {
        if doc.is_element(child) {
            if !is_unwanted(doc, child) {
                walk_free_text(doc, child, reference, depth + 1);
            }
        } else if doc.is_text(child) && is_free_text_container(doc, child) {
            let text = normalize(&doc.text_content(child));
            if (7..52).contains(&text.chars().count()) {
                reference.observe(&text);
            }
        }
    }
}

fn is_free_text_container(doc: &Document, text_id: NodeId) -> bool {
    match doc.parent(text_id) {
        Some(parent) => is_free_text_tag(doc.tag_name(parent)),
        None => false,
    }
}

/// Serializes the document for the raw-HTML scanning stages: outer HTML
/// of `<html>` (or the root's children when absent), skipping embedded/
/// foreign subtrees — upstream serializes its pruned document. Mirrors the
/// upstream serializer: escaped attributes and text, void tags as `<tag/>`,
/// raw text inside script/style-family elements, comments preserved.
fn serialize(doc: &Document) -> String {
    let comments: HashSet<NodeId> = doc.collect_comment_nodes(doc.root()).into_iter().collect();
    let mut out = String::new();
    for child in doc.child_nodes(doc.root()) {
        if doc.is_element(child) && doc.tag_name(child) == "html" {
            out.clear();
            serialize_node(doc, child, &comments, &mut out, 0);
            return out;
        }
    }
    for child in doc.child_nodes(doc.root()) {
        serialize_node(doc, child, &comments, &mut out, 0);
    }
    out
}

fn serialize_node(
    doc: &Document,
    id: NodeId,
    comments: &HashSet<NodeId>,
    out: &mut String,
    depth: usize,
) {
    if depth >= 1000 {
        return;
    }
    if comments.contains(&id) {
        out.push_str("<!--");
        out.push_str(doc.comment_data(id).unwrap_or_default());
        out.push_str("-->");
        return;
    }
    if doc.is_text(id) {
        let raw = doc.parent(id).is_some_and(|parent| {
            matches!(
                doc.tag_name(parent),
                "script"
                    | "style"
                    | "xmp"
                    | "iframe"
                    | "noembed"
                    | "noframes"
                    | "noscript"
                    | "plaintext"
            )
        });
        // `text_content` on a text node is its own text (`text()` is the
        // etree accessor and expects an element id).
        let text = doc.text_content(id);
        if raw {
            out.push_str(&text);
        } else {
            escape_into(&text, out);
        }
        return;
    }
    if !doc.is_element(id) || is_unwanted(doc, id) {
        return;
    }
    out.push('<');
    out.push_str(doc.tag_name(id));
    for name in doc.attribute_names(id) {
        out.push(' ');
        out.push_str(&name);
        out.push_str("=\"");
        escape_into(&doc.get_attribute(id, &name).unwrap_or_default(), out);
        out.push('"');
    }
    if doc.is_void_element(id) {
        out.push_str("/>");
        return;
    }
    out.push('>');
    for child in doc.child_nodes(id) {
        serialize_node(doc, child, comments, out, depth + 1);
    }
    out.push_str("</");
    out.push_str(doc.tag_name(id));
    out.push('>');
}

/// Upstream `escape`: character-escapes text and attribute values.
fn escape_into(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            '\r' => out.push_str("&#13;"),
            _ => out.push(c),
        }
    }
}

fn og_image_date(doc: &Document) -> Option<NaiveDate> {
    for id in doc.get_elements_by_tag_name(doc.root(), "meta") {
        if doc.get_attribute(id, "property").as_deref() == Some("og:image") {
            if let Some(content) = doc.get_attribute(id, "content") {
                // Upstream tries every og:image until one URL yields a date.
                if let Some(date) = url_date(&content) {
                    return Some(date);
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Raw-HTML page scan (extensive last resort)
// ---------------------------------------------------------------------------

/// A distinct captured pattern with its occurrence count.
struct Pattern {
    text: String,
    count: usize,
}

/// Loose patterns over the whole source, most specific first; a copyright
/// year bounds later matches, and the newest plausible candidate wins
/// (upstream `search::page` with UseOriginalDate=false).
fn page_date(html: &str) -> Option<NaiveDate> {
    let copyright = copyright_year(html).and_then(|year| date_from_parts(year, 1, 1));
    let floor = copyright.as_ref().map_or(0, |d| d.year());

    // Strict YYYY/MM/DD (URL paths) and YYYY[/.-]MM[/.-]DD forms.
    for (re, parts) in [
        (
            &*THREE_RE,
            three_parts as fn(&str) -> Option<(i32, u32, u32)>,
        ),
        (&*THREE_LOOSE_RE, three_loose_parts),
    ] {
        if let Some(date) = search(html, re, prefix_year, parts).filter(|d| d.year() >= floor) {
            return Some(date);
        }
    }
    // Day-first DD/MM/YYYY, re-keyed on the parsed date.
    if let Some(date) =
        search_normalized(html, &SELECT_YMD_RE, suffix_year).filter(|d| d.year() >= floor)
    {
        return Some(date);
    }
    // Compact YYYYMMDD strings.
    if let Some(date) = search(html, &DATE_STRINGS_RE, prefix_year, date_strings_parts)
        .filter(|d| d.year() >= floor)
    {
        return Some(date);
    }
    // DD/MM/YY with two-digit years (completed before plausibility).
    if let Some(date) =
        search_normalized(html, &SLASHES_RE, complete_year).filter(|d| d.year() >= floor)
    {
        return Some(date);
    }
    // YYYY/MM.
    if let Some(date) =
        search(html, &YYYY_MM_RE, prefix_year, yyyy_mm_parts).filter(|d| d.year() >= floor)
    {
        return Some(date);
    }
    // MM/YYYY, re-keyed on the parsed date.
    if let Some(date) =
        search_normalized(html, &MM_YYYY_RE, suffix_year).filter(|d| d.year() >= floor)
    {
        return Some(date);
    }
    // Month names anywhere in the source (upstream scans the raw html).
    if let Some((day, month, year)) = month_name_parse(html) {
        if let Some(date) = date_from_parts(year, month, day) {
            if date.year() >= floor {
                return Some(date);
            }
        }
    }
    if let Some(date) = copyright {
        return Some(date);
    }
    // Strip W3C validator badges, then scan for a bare plausible year.
    let cleaned = W3_CLEANER_RE.replace_all(html, " ");
    search(&cleaned, &SIMPLE_YEAR_RE, prefix_year, year_parts)
}

/// Most frequent, then newest "©"-style copyright year (upstream feeds
/// the copyright pattern through `plausible`/`select`).
fn copyright_year(html: &str) -> Option<i32> {
    select(collect(html, &COPYRIGHT_RE, year_itself), year_itself)
        .and_then(|pattern| pattern.text.parse().ok())
}

/// The pattern itself is a year (upstream `year` catcher for copyright).
fn year_itself(pattern: &str) -> Option<i32> {
    pattern.parse().ok()
}

/// Collects distinct plausible matches of `re` (capture 1), counting
/// occurrences (upstream `plausible`).
fn collect(html: &str, re: &Regex, year_of: fn(&str) -> Option<i32>) -> Vec<Pattern> {
    let mut candidates: Vec<Pattern> = Vec::new();
    let mut positions: HashMap<String, usize> = HashMap::new();
    for caps in re.captures_iter(html) {
        let text = caps[1].to_string();
        if !year_of(&text).is_some_and(plausible_year) {
            continue;
        }
        if let Some(&index) = positions.get(&text) {
            candidates[index].count += 1;
        } else {
            positions.insert(text.clone(), candidates.len());
            candidates.push(Pattern { text, count: 1 });
        }
    }
    candidates
}

/// Frequency-then-recency selection (upstream `select`): top-10 by count,
/// newest two pattern strings, runner-up wins when nearly as frequent,
/// from a different year.
fn select(mut patterns: Vec<Pattern>, year_of: fn(&str) -> Option<i32>) -> Option<Pattern> {
    if patterns.is_empty() || patterns.len() >= 1000 {
        return None;
    }
    patterns.sort_by_key(|p| Reverse(p.count));
    patterns.truncate(10);
    // Newest-wins: descending pattern strings.
    patterns.sort_by(|a, b| b.text.cmp(&a.text));
    patterns.truncate(2);
    if patterns.len() == 1 {
        return patterns.into_iter().next();
    }
    let mut iter = patterns.into_iter();
    let first = iter.next()?;
    let second = iter.next()?;
    let first_year = year_of(&first.text).filter(|y| plausible_year(*y));
    let second_year = year_of(&second.text).filter(|y| plausible_year(*y));
    match (first_year, second_year) {
        (None, Some(_)) => return Some(second),
        (Some(_), None) => return Some(first),
        (None, None) => return None,
        _ => {}
    }
    if first.count != second.count
        && first_year != second_year
        && second.count as f64 / first.count as f64 > 0.5
    {
        return Some(second);
    }
    Some(first)
}

/// Scan + select + catcher (upstream `search`).
fn search(
    html: &str,
    re: &Regex,
    year_of: fn(&str) -> Option<i32>,
    parts: fn(&str) -> Option<(i32, u32, u32)>,
) -> Option<NaiveDate> {
    let patterns = collect(html, re, year_of);
    select(patterns, year_of)
        .and_then(|pattern| parts(&pattern.text).and_then(|(y, m, d)| date_from_parts(y, m, d)))
}

/// Like [`search`], but re-keys candidates on their parsed date before
/// selection (upstream `normalized`).
fn search_normalized(
    html: &str,
    re: &Regex,
    year_of: fn(&str) -> Option<i32>,
) -> Option<NaiveDate> {
    let patterns = collect(html, re, year_of);
    let mut normalized: Vec<Pattern> = Vec::new();
    let mut positions: HashMap<String, usize> = HashMap::new();
    for pattern in patterns {
        // Stage 12 only runs in extensive mode.
        let Some(date) = parse_candidate(&pattern.text, true) else {
            continue;
        };
        let key = format!("{:04}-{:02}-{:02}", date.year(), date.month(), date.day());
        if let Some(&index) = positions.get(&key) {
            normalized[index].count += pattern.count;
        } else {
            positions.insert(key.clone(), normalized.len());
            normalized.push(Pattern {
                text: key,
                count: pattern.count,
            });
        }
    }
    select(normalized, prefix_year).and_then(|pattern| date_ymd_parts(&pattern.text))
}

/// First four-digit run of the pattern (upstream `year` year-pattern).
fn prefix_year(pattern: &str) -> Option<i32> {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[0-9]{4}").unwrap());
    RE.find(pattern)?.as_str().parse().ok()
}

/// Last four-digit run of the pattern (upstream `select_ymd_year` /
/// `mm_yyyy_year`).
fn suffix_year(pattern: &str) -> Option<i32> {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[0-9]{4}").unwrap());
    let mut last = None;
    for m in RE.find_iter(pattern) {
        last = Some(m.as_str());
    }
    last?.parse().ok()
}

/// Two-digit suffix completed to a full year (upstream `slashes_year`
/// with `complete = true`).
fn complete_year(pattern: &str) -> Option<i32> {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[0-9]{2}$").unwrap());
    Some(correct_year(
        RE.find(pattern)?.as_str().parse::<i32>().ok()?,
    ))
}

/// Catchers extracting (year, month, day) parts from a pattern.
fn three_parts(pattern: &str) -> Option<(i32, u32, u32)> {
    let mut parts = pattern.split('/');
    let year = parts.next()?.parse().ok()?;
    let month = parts.next()?.parse().ok()?;
    let day = parts.next()?.parse().ok()?;
    Some((year, month, day))
}

fn three_loose_parts(pattern: &str) -> Option<(i32, u32, u32)> {
    let mut parts = pattern.splitn(3, ['/', '.', '-']);
    let year = parts.next()?.parse().ok()?;
    let month = parts.next()?.parse().ok()?;
    let day = parts.next()?.parse().ok()?;
    Some((year, month, day))
}

fn date_strings_parts(pattern: &str) -> Option<(i32, u32, u32)> {
    if pattern.len() != 8 || !pattern.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((
        pattern[..4].parse().ok()?,
        pattern[4..6].parse().ok()?,
        pattern[6..8].parse().ok()?,
    ))
}

fn yyyy_mm_parts(pattern: &str) -> Option<(i32, u32, u32)> {
    // Single separator: "YYYY[/.-]MM"; day defaults to 1.
    let sep = pattern
        .rfind(['/', '.', '-'])
        .filter(|&i| !pattern[..i].contains(['/', '.', '-']))?;
    let year = pattern[..sep].parse().ok()?;
    let month = pattern[sep + 1..].parse().ok()?;
    Some((year, month, 1))
}

fn year_parts(pattern: &str) -> Option<(i32, u32, u32)> {
    Some((pattern.parse().ok()?, 1, 1))
}

fn date_ymd_parts(pattern: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(pattern, "%Y-%m-%d").ok()
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Newest-wins accumulator across candidate strings (upstream `Reference`
/// with UseOriginalDate=false).
#[derive(Default)]
struct Reference {
    /// Whether try-date may use the external-parser fallback
    /// (upstream threads `options` into `Reference::text`).
    extensive: bool,
    date: Option<NaiveDate>,
}

impl Reference {
    fn observe(&mut self, text: &str) {
        if let Some(date) = parse_candidate(text, self.extensive) {
            if self.date.is_none_or(|best| date > best) {
                self.date = Some(date);
            }
        }
    }

    fn observe_unix(&mut self, seconds: i64) {
        // Convert through the system timezone like upstream's environment
        // zone (TZ / iana_time_zone) and Python's local
        // `datetime.fromtimestamp`. chrono::Local is unavailable on
        // wasm32-unknown-unknown (it compiles in js-sys glue that the
        // loader stubs out), so wasm converts as UTC there — same gate
        // as plausible_year / is_plausible_date (DEVIATIONS.md).
        #[cfg(not(target_arch = "wasm32"))]
        self.observe_unix_zoned(seconds, &chrono::Local);
        #[cfg(target_arch = "wasm32")]
        self.observe_unix_zoned(seconds, &chrono::Utc);
    }

    /// Zone-sensitive core of `observe_unix`; generic so tests can pin
    /// a `FixedOffset` instead of the ambient zone (TZ cannot be pinned
    /// portably: chrono ignores it on Windows, and mutating it races
    /// sibling test threads).
    fn observe_unix_zoned<Z: chrono::TimeZone>(&mut self, seconds: i64, zone: &Z) {
        if let Some(datetime) = zone.timestamp_opt(seconds, 0).single() {
            let date = datetime.date_naive();
            // Same plausibility window as every other candidate
            // (upstream `reference_date` builds via `date_from_parts`).
            if super::is_plausible_date(date) && self.date.is_none_or(|best| date > best) {
                self.date = Some(date);
            }
        }
    }

    fn finish(self) -> Option<NaiveDate> {
        self.date
    }
}

/// Collapses runs of whitespace into single spaces (upstream `normalize`).
pub(super) fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Bounds `text` to `length` characters (upstream `limit`).
pub(super) fn limit(text: &str, length: usize) -> &str {
    text.char_indices()
        .nth(length)
        .map_or(text, |(end, _)| &text[..end])
}

/// Plausibility check from the existing meta-date code.
fn date_from_parts(year: i32, month: u32, day: u32) -> Option<NaiveDate> {
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    if super::is_plausible_date(date) {
        Some(date)
    } else {
        None
    }
}

fn plausible_year(year: i32) -> bool {
    // Upstream candidate filter: 1995 through the current year.
    #[cfg(not(target_arch = "wasm32"))]
    let max = chrono::Local::now().year();
    #[cfg(target_arch = "wasm32")]
    let max = 2035;
    (1995..=max).contains(&year)
}

/// Two-digit year correction (upstream `correct_year`).
fn correct_year(year: i32) -> i32 {
    if year < 100 {
        year + if year >= 90 { 1900 } else { 2000 }
    } else {
        year
    }
}

/// Swaps day and month when the month slot is invalid (upstream `swapParts`).
fn swap_parts(day: &mut u32, month: &mut u32) {
    if *month > 12 && *day <= 12 {
        std::mem::swap(day, month);
    }
}

/// Simplified ASCII version of upstream's `discard` regex: rejects clock
/// times, strings that are exactly four bare digits (bare years must come
/// from the page scan instead), currency/stock/URL fragments.
///
/// Deliberately dropped from upstream's alternation: phone numbers with a
/// leading `+NN`, 3- and 5-digit number groups, the word-boundary after the
/// TLD alternative, extra currency symbols (`Ұ฿₱₽`), and non-ASCII digit
/// classes — the net effect is mildly more permissive on free-text
/// candidates, which only widens the newest-wins candidate pool.
fn is_discarded(text: &str) -> bool {
    static DISCARD: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        [
            r"^[0-9]{1,4}:[0-9]{2}([ :]|$)",
            r"^[^0-9]*[0-9]{4}[^0-9]*$",
            r"[#$¢£¥€₹]",
            r"[A-Z]{3}[^A-Z]",
            r"ftps?|https?|sftp",
            r"\.(?:com|net|org|info|gov|edu|de|fr|io)",
            r"IBAN",
            r"[A-Z]{2}[0-9]{2}",
            r"®",
        ]
        .iter()
        .map(|p| Regex::new(p).unwrap())
        .collect()
    });
    DISCARD.iter().any(|re| re.is_match(text))
}

/// Multi-language month names (EN/DE/FR/TR/ID/NL variants), upstream table.
const MONTH_NAMES: &str = r"January?|February?|March|A[pv]ril|Ma[iy]|Jun[ei]|Jul[iy]|August|September|O[ck]tober|November|De[csz]ember|Jan|Feb|M[aä]r|Apr|Jun|Jul|Aug|Sep|O[ck]t|Nov|De[cz]|Januari|Februari|Maret|Mei|Agustus|Jänner|Feber|März|janvier|février|mars|juin|juillet|aout|septembre|octobre|novembre|décembre|Ocak|Şubat|Mart|Nisan|Mayıs|Haziran|Temmuz|Ağustos|Eylül|Ekim|Kasım|Aralık|Oca|Şub|Mar|Nis|Haz|Tem|Ağu|Eyl|Eki|Kas|Ara";

/// Lowercased month name → number (superset of the groups in MONTH_NAMES).
static MONTHS: LazyLock<HashMap<&'static str, u32>> = LazyLock::new(|| {
    HashMap::from([
        ("january", 1),
        ("januar", 1),
        ("januari", 1),
        ("janvier", 1),
        ("jan", 1),
        ("ocak", 1),
        ("oca", 1),
        ("february", 2),
        ("februar", 2),
        ("februari", 2),
        ("février", 2),
        ("feb", 2),
        ("şubat", 2),
        ("şub", 2),
        ("march", 3),
        ("märz", 3),
        ("mär", 3),
        ("maerz", 3),
        ("maret", 3),
        ("mars", 3),
        ("mar", 3),
        ("mart", 3),
        ("april", 4),
        ("avril", 4),
        ("apr", 4),
        ("nisan", 4),
        ("nis", 4),
        ("may", 5),
        ("mai", 5),
        ("mei", 5),
        ("mayıs", 5),
        ("mayis", 5),
        ("june", 6),
        ("juni", 6),
        ("juin", 6),
        ("jun", 6),
        ("junei", 6),
        ("haziran", 6),
        ("haz", 6),
        ("july", 7),
        ("juli", 7),
        ("juillet", 7),
        ("jul", 7),
        ("juliy", 7),
        ("temmuz", 7),
        ("tem", 7),
        ("august", 8),
        ("augustus", 8),
        ("agustus", 8),
        ("août", 8),
        ("aout", 8),
        ("ağustos", 8),
        ("agustos", 8),
        ("aug", 8),
        ("ağu", 8),
        ("agu", 8),
        ("september", 9),
        ("septembre", 9),
        ("eylül", 9),
        ("eylul", 9),
        ("sep", 9),
        ("eyl", 9),
        ("october", 10),
        ("oktober", 10),
        ("octobre", 10),
        ("ekim", 10),
        ("oct", 10),
        ("okt", 10),
        ("eki", 10),
        ("november", 11),
        ("novembre", 11),
        ("kasım", 11),
        ("kasim", 11),
        ("nov", 11),
        ("kas", 11),
        ("december", 12),
        ("desember", 12),
        ("dezember", 12),
        ("décembre", 12),
        ("decembre", 12),
        ("aralık", 12),
        ("aralik", 12),
        ("dec", 12),
        ("dez", 12),
        ("ara", 12),
        ("jänner", 1),
        ("jaenner", 1),
        ("feber", 2),
    ])
});

static URL_DATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[^0-9](199[0-9]|20[0-3][0-9])[-/_]([01]?[0-9])[-/_]([0-3]?[0-9])(?:[^0-9]|$)")
        .unwrap()
});

static TIMESTAMP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"((?:199[0-9]|20[0-3][0-9])-(?:[01]?[0-9])-(?:[0-3]?[0-9])).[0-9]{2}:[0-9]{2}:[0-9]{2}",
    )
    .unwrap()
});

static DMY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:[^0-9]|^)([0-3]?[0-9])[-/.]([01]?[0-9])[-/.]([0-9]{2,4})(?:[^0-9]|$)").unwrap()
});

/// Numeric triple for [`external_parse`]: strictly anchored (dateparser
/// is strict — surrounding prose fails), slots `[0-3]?[0-9]`, two- to
/// four-digit year, optional clock-time tail. The order resolves by value:
/// month-first, flipping to day-first when the leading number cannot be
/// a month (dateparser resolves both orders).
static MDY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?P<lead>[0-3]?[0-9])(?P<sep>[-/.]|[, ]+)(?P<trail>[0-3]?[0-9])(?P<sep2>[-/.]|[, ]+)(?P<year>[0-9]{2,4})([ ]?[0-2]?[0-9]:[0-5][0-9](:[0-5][0-9])?([ ]?(am|pm|AM|PM))?)?$",
    )
    .unwrap()
});

static YM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:[^0-9]|^)(?:(199[0-9]|20[0-3][0-9])[-/.]([01]?[0-9])|([01]?[0-9])[-/.](199[0-9]|20[0-3][0-9]))(?:[^0-9]|$)",
    )
    .unwrap()
});

static THREE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/([0-9]{4}/[0-9]{2}/[0-9]{2})[01/]").unwrap());

static THREE_LOOSE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^0-9]([0-9]{4}[/.-][0-9]{2}[/.-][0-9]{2})[^0-9]").unwrap());

static SELECT_YMD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^0-9]([0-3]?[0-9][/.-][01]?[0-9][/.-][0-9]{4})[^0-9]").unwrap());

static DATE_STRINGS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^0-9]((?:19|20)[0-9]{2}[01][0-9][0-3][0-9])[^0-9]").unwrap());

static SLASHES_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"[^0-9]([0-3]?[0-9]/[01]?[0-9]/[0129][0-9]|[0-3][0-9]\.[01][0-9]\.[0129][0-9])[^0-9]",
    )
    .unwrap()
});

static YYYY_MM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^0-9]([12][0-9]{3}[/.-](?:1[0-2]|0[1-9]))[^0-9]").unwrap());

static MM_YYYY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^0-9]([01]?[0-9][/.-][12][0-9]{3})[^0-9]").unwrap());

static SIMPLE_YEAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^0-9](199[0-9]|20[0-3][0-9])[^0-9]").unwrap());

static W3_CLEANER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"w3\.org[^0-9](199[0-9]|20[0-3][0-9])[^0-9]").unwrap());

static COPYRIGHT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:\x{00A9}|&copy;|Copyright|\(c\))[^0-9]*(?:199[0-9]|20[0-3][0-9])?-?(199[0-9]|20[0-3][0-9])[^0-9]",
    )
    .unwrap()
});

#[cfg(test)]
mod tests {
    use super::super::examine_meta_date;
    use super::*;

    fn doc(html: &str) -> Document {
        Document::parse(html)
    }

    /// Upstream: python_date_selector_preserves_attribute_order — the
    /// first *present* attribute among id/class/itemprop decides, not a
    /// concatenation. Divergence: our DOM (scraper default) sorts attributes
    /// alphabetically, losing source order, so we check all present
    /// attributes (see DEVIATIONS.md); elements whose date hint lives in a
    /// non-first attribute (e.g. `id="blog_date"` with a plain class) are
    /// found inclusive of upstream's rule.
    #[test]
    fn date_element_checks_all_attributes() {
        let cases = [
            // class first in source, class has no date hint: upstream false.
            // We see itemprop and match (documented over-inclusion).
            (
                r#"<time class='hidden' itemprop='dateModified'>2018-08-29</time>"#,
                true,
            ),
            // Real-world shape: date hint in id, class non-date-ish.
            (
                r#"<div id="blog_date" class="blog_teaser_firstline">22.05.2014</div>"#,
                true,
            ),
            // No hints anywhere.
            (r#"<div class="teaser">22.05.2014</div>"#, false),
        ];
        for (html, expected) in cases {
            let document = doc(html);
            let element = ["time", "div"]
                .iter()
                .find_map(|tag| {
                    document
                        .get_elements_by_tag_name(document.root(), tag)
                        .first()
                        .copied()
                })
                .expect("target element");
            assert_eq!(
                is_date_element(&document, element, false),
                expected,
                "{html}"
            );
        }
    }

    /// footer-info-lastmod matches as a substring, like upstream.
    #[test]
    fn date_element_footer_lastmod_substring() {
        let document = doc(r#"<p id="mw-footer-info-lastmod">Yesterday</p>"#);
        let p = document
            .get_elements_by_tag_name(document.root(), "p")
            .first()
            .copied()
            .unwrap();
        assert!(is_date_element(&document, p, false));
    }

    /// Deeply nested documents must not overflow the recursive walks.
    #[test]
    fn deep_nesting_does_not_overflow() {
        // Within the depth cap: the date is found.
        let mut html = String::new();
        for _ in 0..100 {
            html.push_str("<div>");
        }
        html.push_str("<time datetime='2018-08-29'></time>");
        let document = doc(&html);
        assert_eq!(
            time_element_dates(&document, true),
            Some(NaiveDate::from_ymd_opt(2018, 8, 29).unwrap())
        );

        // Far beyond the cap: no crash, no result.
        let mut deep = String::new();
        for _ in 0..50_000 {
            deep.push_str("<div>");
        }
        deep.push_str("<time datetime='2018-08-29'></time>");
        let document = doc(&deep);
        assert_eq!(time_element_dates(&document, true), None);
        let mut elements = Vec::new();
        collect_wanted(
            &document,
            document.root(),
            &mut elements,
            &|d, id| d.tag_name(id) == "time",
            0,
        );
        assert_eq!(elements.len(), 0, "time element beyond depth cap");
        assert!(!serialize(&document).is_empty());
    }

    /// og:image candidates are tried in order until one URL holds a date.
    #[test]
    fn og_image_tries_all_candidates() {
        let document = doc(
            r#"<meta property="og:image" content="https://example.com/logo.png">
               <meta property="og:image" content="https://example.com/photos/2019/06/01/cat.jpg">"#,
        );
        assert_eq!(
            og_image_date(&document),
            NaiveDate::from_ymd_opt(2019, 6, 1)
        );
    }

    /// Day-first numeric parsing recovers US-ordered dates when the month
    /// slot cannot be a month (upstream `swap_parts`).
    #[test]
    fn day_first_numeric_swaps_impossible_month() {
        // M/D/Y landing in day-first slots: month 13 is impossible, swap.
        assert_eq!(
            parse_candidate("5/13/2019", true),
            NaiveDate::from_ymd_opt(2019, 5, 13)
        );
        assert_eq!(
            parse_candidate("05/13/19", true),
            NaiveDate::from_ymd_opt(2019, 5, 13)
        );
        // Unambiguous day-first stays as-is.
        assert_eq!(
            parse_candidate("13/05/2019", true),
            NaiveDate::from_ymd_opt(2019, 5, 13)
        );
        // Ambiguous inputs remain day-first (no swap when both fit).
        assert_eq!(
            parse_candidate("05/06/2019", true),
            NaiveDate::from_ymd_opt(2019, 6, 5)
        );
    }

    /// Two-digit-year slashes flow through `page_date`'s `search_normalized`
    /// into `parse_candidate`, so the swap applies there too.
    #[test]
    fn page_date_slashes_recover_swapped_order() {
        let html = "<p>posted on 05/13/19 by bob</p>";
        assert_eq!(page_date(html), NaiveDate::from_ymd_opt(2019, 5, 13));
    }

    /// Frequency-then-recency selection (upstream `select`): the runner-up
    /// wins only when it is nearly as frequent as the newest pattern and
    /// from a different year.
    #[test]
    fn select_runner_up_wins_when_nearly_as_frequent() {
        let pattern = |text: &str, count: usize| Pattern {
            text: text.to_string(),
            count,
        };
        // Newest text leads; the older pattern is twice as frequent.
        let picked = select(vec![pattern("2015", 2), pattern("2019", 1)], year_itself);
        assert_eq!(picked.map(|p| p.text), Some("2015".to_string()));
        // Runner-up at 2/3 frequency still wins.
        let picked = select(vec![pattern("2019", 3), pattern("2015", 2)], year_itself);
        assert_eq!(picked.map(|p| p.text), Some("2015".to_string()));
        // Runner-up at 1/3 frequency loses to the newest.
        let picked = select(vec![pattern("2019", 3), pattern("2015", 1)], year_itself);
        assert_eq!(picked.map(|p| p.text), Some("2019".to_string()));
    }

    /// Upstream removes embedded/foreign subtrees from the tree before the
    /// DOM scanning stages, so a selected date element's text must not
    /// include text hidden inside e.g. `<svg>`.
    #[test]
    fn date_element_text_excludes_unwanted_subtrees() {
        let document =
            doc(r#"<div class="date"><svg><text>2000-01-01 </text></svg>2020-06-15</div>"#);
        assert_eq!(
            date_elements(&document, false),
            NaiveDate::from_ymd_opt(2020, 6, 15)
        );
    }

    /// Unix timestamps feed the same plausibility window as other dates
    /// (upstream `reference_date` routes them through `date_from_parts`).
    #[test]
    fn unix_timestamps_must_be_plausible() {
        // Epoch and far-future stamps are implausible years: ignored.
        let document = doc(r#"<abbr data-utime="0"></abbr>"#);
        assert_eq!(abbr_dates(&document, true), None);
        let document = doc(r#"<abbr data-utime="4102444800"></abbr>"#); // 2100-01-01
        assert_eq!(abbr_dates(&document, true), None);
        // A plausible stamp still parses. The local date depends on the
        // host zone (real offsets reach UTC+14, so even noon UTC can
        // shift a day), so expect the system-local conversion rather
        // than an absolute date; the fixed-offset test below pins the
        // conversion semantics against absolute dates.
        use chrono::TimeZone;
        let seconds = 1_560_600_000i64; // 2019-06-15 12:00Z
        let expected = chrono::Local
            .timestamp_opt(seconds, 0)
            .single()
            .unwrap()
            .date_naive();
        let document = doc(r#"<abbr data-utime="1560600000"></abbr>"#);
        assert_eq!(abbr_dates(&document, true), Some(expected));
    }

    /// Unix stamps convert through a timezone, like upstream's environment
    /// zone and Python's local `datetime.fromtimestamp`.
    #[test]
    fn unix_timestamp_uses_system_timezone() {
        // The zone-sensitive conversion lives in observe_unix_zoned;
        // exercise it at the seam with a fixed offset — deterministic
        // on every host and CI platform, unlike pinning TZ (chrono
        // ignores TZ on Windows, and mutating it races sibling test
        // threads). UTC-4 is New York daylight time: midnight UTC is
        // still 20:00 the previous day there, noon UTC keeps its date.
        let edt = chrono::FixedOffset::east_opt(-4 * 3600).unwrap();
        let mut reference = Reference::default();
        reference.observe_unix_zoned(1_560_556_800, &edt); // 2019-06-15T00:00Z
        assert_eq!(reference.finish(), NaiveDate::from_ymd_opt(2019, 6, 14));
        let mut reference = Reference::default();
        reference.observe_unix_zoned(1_560_600_000, &edt); // 2019-06-15T12:00Z
        assert_eq!(reference.finish(), NaiveDate::from_ymd_opt(2019, 6, 15));
        // The plausibility window applies after conversion: epoch is
        // 1970 even in the most eastern zone.
        let mut reference = Reference::default();
        reference.observe_unix_zoned(0, &edt);
        assert_eq!(reference.finish(), None);
    }

    /// Upstream gates the external-parser fallback behind extensive search:
    /// fast mode never consults it.
    #[test]
    fn mdy_fallback_is_extensive_only() {
        assert_eq!(parse_candidate("1-21-2019", false), None);
        assert_eq!(
            parse_candidate("1-21-2019", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
        // Meta attributes flow through the same gate.
        let document = doc(r#"<meta name="date" content="5/23/2019">"#);
        assert_eq!(examine_meta_date(&document, false), None);
        assert_eq!(
            examine_meta_date(&document, true),
            NaiveDate::from_ymd_opt(2019, 5, 23)
        );
    }

    /// US-ordered month-first numerics with day 20-31 are invisible to the
    /// day-first pattern (its month slot tops out at 19); upstream recovers
    /// them through its external-parser fallback (`external_parse`).
    #[test]
    fn mdy_fallback_recovers_us_order_beyond_swap() {
        assert_eq!(
            parse_candidate("1-21-2019", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
        assert_eq!(
            parse_candidate("5/23/2019", true),
            NaiveDate::from_ymd_opt(2019, 5, 23)
        );
        assert_eq!(
            parse_candidate("1.21.2019", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
        assert_eq!(
            parse_candidate("12/31/2019", true),
            NaiveDate::from_ymd_opt(2019, 12, 31)
        );
        // Two-digit years get the same correction as everywhere else.
        assert_eq!(
            parse_candidate("1-21-19", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
        assert_eq!(
            parse_candidate("1-21-99", true),
            NaiveDate::from_ymd_opt(1999, 1, 21)
        );
        // Space/comma-separated triples resolve month-first, falling back
        // to day-first when the leading number cannot be a month.
        assert_eq!(
            parse_candidate("1 21 2019", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
        assert_eq!(
            parse_candidate("31 12 2019", true),
            NaiveDate::from_ymd_opt(2019, 12, 31)
        );
        // A trailing clock time is tolerated (strict dateparser).
        assert_eq!(
            parse_candidate("1/21/2019 10:30", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
    }

    /// dateparser drops a fixed set of English filler words (anywhere,
    /// case-insensitively) before parsing; mirror that for the numeric MDY
    /// surface so "on 1-21-2019" parses like the bare triple.
    #[test]
    fn external_parse_strips_ignorable_words() {
        assert_eq!(
            parse_candidate("on 1-21-2019", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
        assert_eq!(
            parse_candidate("the 5-23-2019 and", true),
            NaiveDate::from_ymd_opt(2019, 5, 23)
        );
        assert_eq!(
            parse_candidate("ON 1-21-2019", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
        assert_eq!(
            parse_candidate("from 21.6.2019", true),
            NaiveDate::from_ymd_opt(2019, 6, 21)
        );
        // Unknown words are not dropped: strict parsing rejects the text.
        assert_eq!(parse_candidate("am 1-21-2019", true), None);
        assert_eq!(parse_candidate("extra 1-21-2019", true), None);
    }

    /// A four-digit year of 0000 is rejected by the external parser
    /// (dateparser requires a positive year); the two-digit form still
    /// resolves through `correct_year`, and the day-first cascade keeps
    /// upstream's `correct_year` behavior for 0000.
    #[test]
    fn mdy_fallback_rejects_four_digit_zero_year() {
        assert_eq!(parse_candidate("1-21-0000", true), None);
        assert_eq!(
            parse_candidate("21-1-0000", true),
            NaiveDate::from_ymd_opt(2000, 1, 21)
        );
        assert_eq!(
            parse_candidate("1-21-00", true),
            NaiveDate::from_ymd_opt(2000, 1, 21)
        );
    }

    /// The fallback stays strict: months 1-12, calendar-validated days,
    /// plausible years — and it only runs once the day-first cascade has
    /// failed, so ambiguous triples remain day-first.
    #[test]
    fn mdy_fallback_rejects_invalid_parts() {
        assert_eq!(parse_candidate("13-21-2019", true), None);
        assert_eq!(parse_candidate("0/21/2019", true), None);
        assert_eq!(parse_candidate("11/31/2019", true), None);
        assert_eq!(parse_candidate("2/29/2019", true), None);
        assert_eq!(parse_candidate("1-21-1518", true), None);
        // Neither slot can be a month.
        assert_eq!(parse_candidate("13 21 2019", true), None);
        assert_eq!(
            parse_candidate("2/29/2020", true),
            NaiveDate::from_ymd_opt(2020, 2, 29)
        );
        assert_eq!(
            parse_candidate("1-5-2019", true),
            NaiveDate::from_ymd_opt(2019, 5, 1)
        );
    }

    /// Upstream's `text_date` guard: no separator character anywhere
    /// means the external parser never runs, even for digit runs that
    /// would otherwise split into a date.
    #[test]
    fn mdy_fallback_requires_separator() {
        assert_eq!(parse_candidate("1212019", true), None);
    }

    /// Stripping succeeds but the stripped text is not a date either:
    /// the original text is retried unchanged and still rejected (any
    /// strip that changed the text implies prose the anchored triple
    /// regex cannot match, so the retry can only ever confirm the
    /// rejection — the neighboring success case shows stripping itself
    /// still works).
    #[test]
    fn external_parse_falls_back_to_original_after_strip() {
        assert_eq!(parse_candidate("on 13 21 2019", true), None);
        assert_eq!(
            parse_candidate("on 13 12 2019", true),
            NaiveDate::from_ymd_opt(2019, 12, 13)
        );
    }

    /// Three-digit years fall outside every year slot (2 or 4 digits);
    /// upstream's external parser rejects them too.
    #[test]
    fn mdy_fallback_rejects_three_digit_year() {
        assert_eq!(parse_candidate("1-21-201", true), None);
        assert_eq!(parse_candidate("5/23/201", true), None);
        assert_eq!(parse_candidate("21-1-201", true), None);
    }

    /// The clock-tail slot accepts seconds and meridiems, matching
    /// dateparser's strict time form.
    #[test]
    fn mdy_fallback_tolerates_clock_tails() {
        assert_eq!(
            parse_candidate("1/21/2019 10:30:00 am", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
        assert_eq!(
            parse_candidate("1/21/2019 2:05 PM", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
    }

    /// Noise punctuation is dropped from token edges just as dateparser's
    /// tokenizer splits it into ignorable runs — including separators
    /// (`.`, `-`, `:`, …) and paired punctuation at the outermost edges,
    /// which the anchored triple regex alone would reject.
    #[test]
    fn external_parse_strips_noise_punctuation() {
        assert_eq!(
            parse_candidate("1-21-2019 ,", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
        assert_eq!(
            parse_candidate("; 1-21-2019", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
        assert_eq!(
            parse_candidate("| 1-21-2019", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
        // Separator-class characters at the very edges never break a
        // candidate: the tokenizer splits them into standalone runs and
        // drops them (upstream parses all of these).
        for text in [
            "1-21-2019.",
            ".1-21-2019",
            "1-21-2019-",
            "1-21-2019:",
            "1-21-2019 !",
            "(1-21-2019)",
            "—1-21-2019",
            "\"1 21 2019\"",
        ] {
            assert_eq!(
                parse_candidate(text, true),
                NaiveDate::from_ymd_opt(2019, 1, 21),
                "{text}"
            );
        }
    }

    /// Comma-separated triples behave like space-separated ones.
    #[test]
    fn mdy_fallback_accepts_comma_separators() {
        assert_eq!(
            parse_candidate("1, 21, 2019", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
        assert_eq!(
            parse_candidate("21, 1, 2019", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
    }

    /// Ordinal suffixes attached to a number are dropped after the
    /// tokenizer's digit/letter split ("21st" → "21"), including the
    /// non-word "th".
    #[test]
    fn external_parse_strips_ordinal_suffixes() {
        assert_eq!(
            parse_candidate("on the 21st of 1 2019", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
        // The suffix strip follows the edge-noise trim, so sentence
        // punctuation after the ordinal does not shield it.
        assert_eq!(
            parse_candidate("on the 21st. 1 2019", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
        assert_eq!(
            parse_candidate("21st - 1 2019", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
        assert_eq!(
            parse_candidate("on 1st 21 2019", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
        assert_eq!(
            parse_candidate("3rd 12 2019", true),
            NaiveDate::from_ymd_opt(2019, 3, 12)
        );
        assert_eq!(
            parse_candidate("21th 1 2019", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
        // A standalone suffix is a filler word (empty translation) and
        // drops anywhere; the attached form is case-insensitive.
        assert_eq!(
            parse_candidate("st 21 1 2019", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
        assert_eq!(
            parse_candidate("21ST 1 2019", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
        // Multibyte characters can neither match a suffix nor split a
        // char (the char-boundary check in strip_ordinal_suffix guards
        // the panic case — "21é2" reaches it, "21é" does not).
        assert_eq!(parse_candidate("21s\u{00e9} 1 2019", true), None);
        assert_eq!(parse_candidate("21\u{00e9}2 1 2019", true), None);
        // Upstream's tokenizer is ASCII-only: a locale-less non-ASCII
        // character glued to a number's edge is an ignorable "other"
        // run and the candidate still parses (verified against
        // rust-dateparser: 𝑡, →).
        assert_eq!(
            parse_candidate("21\u{1d461} 1 2019", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
        // Deviation (locale-dependent orders, DEVIATIONS.md): a locale-
        // mapped é flips upstream to its day-first order — the same
        // value for this trailing shape, but it rejects the leading
        // form. We have no locale detection, so both resolve by value.
        assert_eq!(
            parse_candidate("21\u{00e9} 1 2019", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
        assert_eq!(
            parse_candidate("\u{00e9}1 21 2019", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
    }

    /// Mixed-separator triples follow upstream's component grouping: a
    /// dash/slash/dot first separator binds the triple (day-first flip
    /// allowed), a space/comma first separator before dash/slash/dot
    /// splits it (rejected).
    #[test]
    fn mdy_fallback_mixed_separators() {
        // Bound first: flip allowed regardless of the second separator.
        for text in ["21-1 2019", "21/1 2019", "21.1 2019", "21-1, 2019"] {
            assert_eq!(
                parse_candidate(text, true),
                NaiveDate::from_ymd_opt(2019, 1, 21),
                "{text}"
            );
        }
        // Unbound first: flip rejected — but the trailing "1-2019" is
        // still a year-month pair for the extended cascade, so the
        // candidate resolves to the first of that month (upstream's own
        // ymd pattern behaves the same end-to-end).
        for text in ["21 1-2019", "21 1/2019", "21 1.2019", "21, 1-2019"] {
            assert_eq!(
                parse_candidate(text, true),
                NaiveDate::from_ymd_opt(2019, 1, 1),
                "{text}"
            );
        }
        // With no rescuing pattern left (year outside every class), the
        // split triple is rejected outright — also with a two-digit
        // year, which no year-month pattern can rescue.
        assert_eq!(parse_candidate("21 1-1899", true), None);
        assert_eq!(parse_candidate("21 1-19", true), None);
        // Month-first needs no flip and tolerates any separator mix.
        assert_eq!(
            parse_candidate("1 21-2019", true),
            NaiveDate::from_ymd_opt(2019, 1, 21)
        );
    }

    /// Year-month pairs resolve to the first of the month in both
    /// orders; an impossible month falls through.
    #[test]
    fn extended_parse_year_month_both_orders() {
        assert_eq!(
            parse_candidate("2019/06", true),
            NaiveDate::from_ymd_opt(2019, 6, 1)
        );
        assert_eq!(
            parse_candidate("06/2019", true),
            NaiveDate::from_ymd_opt(2019, 6, 1)
        );
        assert_eq!(parse_candidate("2019/13", true), None);
    }

    /// Calendar-invalid day-first triples fall through to the later
    /// patterns instead of aborting the whole cascade.
    #[test]
    fn extended_parse_falls_through_on_invalid_parts() {
        // DMY matches (31.2) but Feb 31 doesn't exist; the year-month
        // pattern then reads "2.2019".
        assert_eq!(
            parse_candidate("31.2.2019", true),
            NaiveDate::from_ymd_opt(2019, 2, 1)
        );
        // Month-name match (31. Februar) is invalid; nothing else fits.
        assert_eq!(parse_candidate("31. Februar 2019", true), None);
    }

    /// Every string `MONTH_NAMES` can match must have a `MONTHS` entry —
    /// `Mär` (DE abbreviation) and `Desember` (ID) were missing.
    #[test]
    fn month_name_parse_covers_map_gaps() {
        assert_eq!(
            parse_candidate("13 Mär 2019", true),
            NaiveDate::from_ymd_opt(2019, 3, 13)
        );
        assert_eq!(
            parse_candidate("13 Desember 2019", true),
            NaiveDate::from_ymd_opt(2019, 12, 13)
        );
    }
}
