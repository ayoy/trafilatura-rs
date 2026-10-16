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
    if let Some(date) = examine_meta_date(doc) {
        return Some(date);
    }
    if let Some(date) = json_search_date(doc) {
        return Some(date);
    }
    // 4. <abbr> elements (unix stamps, published classes).
    if let Some(date) = abbr_dates(doc) {
        return Some(date);
    }
    // 5. Elements whose tag/id/class/itemprop look date-ish. Upstream
    // removes embedded/foreign subtrees before this stage.
    if let Some(date) = date_elements(doc, !extensive) {
        return Some(date);
    }
    // 6. <title>/<h1> text.
    if let Some(date) = title_dates(doc) {
        return Some(date);
    }
    // 7. <time> elements.
    if let Some(date) = time_element_dates(doc) {
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
/// year-month forms. Note the discard rules reject bare four-digit years —
/// year-only dates only surface through the raw-HTML page scan.
pub(super) fn parse_candidate(text: &str) -> Option<NaiveDate> {
    let text = normalize(text);
    let text = limit(&text, 52);
    let digits = text.chars().filter(|c| c.is_ascii_digit()).count();
    if !(4..=18).contains(&digits) || is_discarded(text) {
        return None;
    }
    super::fast_parse_date(text).or_else(|| extended_parse(text))
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
fn abbr_dates(doc: &Document) -> Option<NaiveDate> {
    let elements = doc.get_elements_by_tag_name(doc.root(), "abbr");
    if elements.is_empty() || elements.len() >= 1000 {
        return None;
    }
    let mut reference = Reference::default();
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
        .or_else(|| first_element_date(doc, &elements))
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
    first_element_date(doc, &elements)
}

/// Same treatment for `<title>`/`<h1>` text.
fn title_dates(doc: &Document) -> Option<NaiveDate> {
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
    first_element_date(doc, &elements)
}

/// `<time>` elements: `datetime` attribute (shortcut for `updated` class
/// under newest-wins) or element text; newest date wins.
fn time_element_dates(doc: &Document) -> Option<NaiveDate> {
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
    let mut reference = Reference::default();
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
                if let Some(date) = parse_candidate(datetime) {
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
fn first_element_date(doc: &Document, elements: &[NodeId]) -> Option<NaiveDate> {
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
            if let Some(date) = parse_candidate(&shortened) {
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
    let mut reference = Reference::default();
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
        let Some(date) = parse_candidate(&pattern.text) else {
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
    date: Option<NaiveDate>,
}

impl Reference {
    fn observe(&mut self, text: &str) {
        if let Some(date) = parse_candidate(text) {
            if self.date.is_none_or(|best| date > best) {
                self.date = Some(date);
            }
        }
    }

    fn observe_unix(&mut self, seconds: i64) {
        if let Some(datetime) = chrono::DateTime::from_timestamp(seconds, 0) {
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
            time_element_dates(&document),
            Some(NaiveDate::from_ymd_opt(2018, 8, 29).unwrap())
        );

        // Far beyond the cap: no crash, no result.
        let mut deep = String::new();
        for _ in 0..50_000 {
            deep.push_str("<div>");
        }
        deep.push_str("<time datetime='2018-08-29'></time>");
        let document = doc(&deep);
        assert_eq!(time_element_dates(&document), None);
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
            parse_candidate("5/13/2019"),
            NaiveDate::from_ymd_opt(2019, 5, 13)
        );
        assert_eq!(
            parse_candidate("05/13/19"),
            NaiveDate::from_ymd_opt(2019, 5, 13)
        );
        // Unambiguous day-first stays as-is.
        assert_eq!(
            parse_candidate("13/05/2019"),
            NaiveDate::from_ymd_opt(2019, 5, 13)
        );
        // Ambiguous inputs remain day-first (no swap when both fit).
        assert_eq!(
            parse_candidate("05/06/2019"),
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
        assert_eq!(abbr_dates(&document), None);
        let document = doc(r#"<abbr data-utime="4102444800"></abbr>"#); // 2100-01-01
        assert_eq!(abbr_dates(&document), None);
        // A plausible stamp still parses.
        let document = doc(r#"<abbr data-utime="1560556800"></abbr>"#); // 2019-06-15
        assert_eq!(abbr_dates(&document), NaiveDate::from_ymd_opt(2019, 6, 15));
    }

    /// Every string `MONTH_NAMES` can match must have a `MONTHS` entry —
    /// `Mär` (DE abbreviation) and `Desember` (ID) were missing.
    #[test]
    fn month_name_parse_covers_map_gaps() {
        assert_eq!(
            parse_candidate("13 Mär 2019"),
            NaiveDate::from_ymd_opt(2019, 3, 13)
        );
        assert_eq!(
            parse_candidate("13 Desember 2019"),
            NaiveDate::from_ymd_opt(2019, 12, 13)
        );
    }
}
