//! Stage 5a: deterministic evidence harvest. Regex + positional heuristics
//! over the converted markdown. Zero models, fully testable, and the backbone
//! of both the evidence bundle and the anti-hallucination date check.

use chrono::NaiveDate;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FoundDate {
    /// Normalized ISO form.
    pub iso: String,
    /// Verbatim text as it appeared.
    pub raw: String,
    /// Character offset in the source markdown (for position weighting).
    pub offset: usize,
    /// True when this reading exists only because a numeric form was read the
    /// other way round (day-first alternate, dotted European). The checker
    /// whitelists it like any other date but flags the coin flip, so a manifest
    /// never claims `date_source: "document"` for a 50/50 guess.
    #[serde(default)]
    pub ambiguous: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Harvest {
    pub dates: Vec<FoundDate>,
    pub subject_lines: Vec<String>,
    pub headings: Vec<String>,
    pub caption_lines: Vec<String>,
    pub head_excerpt: String,
    pub signature_block: String,
}

static RE_ISO: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(\d{4})-(\d{2})-(\d{2})\b").unwrap());
// 2026/07/20, 2026.7.2 — year-first is never ambiguous, so one reading only.
static RE_YEAR_FIRST: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b(\d{4})[/.-](\d{1,2})[/.-](\d{1,2})\b").unwrap());
// 07/20/2026, 7/20/26, 07-20-2026 (US order assumed; ambiguity handled below)
static RE_SLASH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b(\d{1,2})[/-](\d{1,2})[/-](\d{4}|\d{2})\b").unwrap());
// 20.07.2026 — dotted is a European convention, so day-first is the primary
// reading, but a US author writing 07.20.2026 exists too: mark it ambiguous.
// The year must be 4 digits: with a 2-digit tail this shape is outline and
// sub-clause numbering far more often than it is a date ("1.2.34 The Tenant
// shall pay rent" minted 2034-02-01 *and* 2034-01-02), and a phantom is worse
// than a miss — filter.rs renders harvest.dates into the naming prompt as
// "DATES FOUND IN DOCUMENT" and check() then whitelists whatever comes back.
static RE_DOTTED: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b(\d{1,2})\.(\d{1,2})\.(\d{4})\b").unwrap());
// July 20, 2026  |  Jul 20 2026  |  Jul-20-26
static RE_MONTH_FIRST: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(jan(?:uary)?|feb(?:ruary)?|mar(?:ch)?|apr(?:il)?|may|jun(?:e)?|jul(?:y)?|aug(?:ust)?|sep(?:t(?:ember)?)?|oct(?:ober)?|nov(?:ember)?|dec(?:ember)?)\.?[\s/-]+(\d{1,2})(?:st|nd|rd|th)?,?[\s/-]+(\d{4}|\d{2})\b").unwrap()
});
// 20 July 2026  |  20-Jul-2026
static RE_DAY_FIRST: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(\d{1,2})(?:st|nd|rd|th)?[\s/-]+(jan(?:uary)?|feb(?:ruary)?|mar(?:ch)?|apr(?:il)?|may|jun(?:e)?|jul(?:y)?|aug(?:ust)?|sep(?:t(?:ember)?)?|oct(?:ober)?|nov(?:ember)?|dec(?:ember)?)\.?,?[\s/-]+(\d{4}|\d{2})\b").unwrap()
});
// Clause, version and docket numbering that looks exactly like a numeric date.
// A phantom date is worse than a missed one: it widens the set the checker's
// anti-hallucination tripwire accepts, so a made-up date can pass because a
// section number happened to have the same shape.
static RE_CLAUSE_LEAD: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:^|[^\p{L}\d])(rule|section|sect|sec|§|version|ver|rev|release|build|exhibit|part|no|nos|article|clause|paragraph|para|item|chapter|appendix|schedule|figure|fig|table)\s*\.?\s*[:#]?\s*$").unwrap()
});

static RE_SUBJECT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?im)^\s*(?:re|subject|in re|regarding|matter)\s*[:\-]\s*(.{3,140})$").unwrap()
});
static RE_EMAIL_HDR: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?im)^\s*(?:from|to|cc|date|sent|subject)\s*:\s*(.{2,160})$").unwrap()
});
static RE_HEADING: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^#{1,3}\s+(.{3,120})$").unwrap());
static RE_ALLCAPS_LINE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^\s*([A-Z][A-Z0-9 ,.&'/\-]{7,90})\s*$").unwrap());
// Case captions: "Smith v. Jones", "In re Acme Corp.", docket numbers
static RE_CAPTION: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?im)^.{0,80}\b(?:v\.?s?\.|versus|in re|ex parte)\b.{0,80}$|(?i)\b(?:case|docket|civil action)\s*(?:no\.?|number)\s*[:#]?\s*[\w:\-]{4,25}").unwrap()
});

fn month_num(m: &str) -> Option<u32> {
    let m = m.to_ascii_lowercase();
    let n = match &m[..3.min(m.len())] {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    Some(n)
}

fn push_date(
    out: &mut Vec<FoundDate>,
    y: i32,
    m: u32,
    d: u32,
    raw: &str,
    offset: usize,
    ambiguous: bool,
) {
    if let Some(date) = NaiveDate::from_ymd_opt(y, m, d) {
        out.push(FoundDate {
            iso: date.format("%Y-%m-%d").to_string(),
            raw: raw.to_string(),
            offset,
            ambiguous,
        });
    }
}

/// True when the text immediately before `start` is clause/version numbering.
/// Only the purely numeric patterns consult this — "Section March 3, 2026" is
/// not a thing anyone writes.
fn is_clause_numbering(text: &str, start: usize) -> bool {
    let from = floor_char_boundary(text, start.saturating_sub(24));
    RE_CLAUSE_LEAD.is_match(&text[from..start])
}

/// True when nothing but whitespace or list punctuation precedes `start` on its
/// own line. A numeric run in that position carries no lead word for
/// `is_clause_numbering` to see, so "1.2.2034 The Tenant shall pay rent" looked
/// exactly like a date line to the harvester.
fn is_line_leading(text: &str, start: usize) -> bool {
    let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
    text[line_start..start]
        .chars()
        .all(|c| c.is_whitespace() || matches!(c, '-' | '*' | '•' | '>' | '(' | '[' | '#' | '|'))
}

fn expand_year(y: u32) -> i32 {
    // 2-digit years: 00-49 -> 2000s, 50-99 -> 1900s. Documents about the
    // future exist; documents from 1926 filed as "26" basically don't.
    if y < 100 {
        if y < 50 {
            2000 + y as i32
        } else {
            1900 + y as i32
        }
    } else {
        y as i32
    }
}

/// Extract every recognizable date, normalized, with position. For the
/// ambiguous numeric form (a/b/y) we assume US month-first, and when the
/// day-first reading is *also* valid we record both so the checker's
/// presence test never falsely rejects a correct SLM answer.
pub fn extract_dates(text: &str) -> Vec<FoundDate> {
    let mut out = Vec::new();
    for c in RE_ISO.captures_iter(text) {
        let m0 = c.get(0).unwrap();
        if is_clause_numbering(text, m0.start()) {
            continue;
        }
        push_date(
            &mut out,
            c[1].parse().unwrap_or(0),
            c[2].parse().unwrap_or(0),
            c[3].parse().unwrap_or(0),
            m0.as_str(),
            m0.start(),
            false,
        );
    }
    for c in RE_YEAR_FIRST.captures_iter(text) {
        let m0 = c.get(0).unwrap();
        if is_clause_numbering(text, m0.start()) {
            continue;
        }
        push_date(
            &mut out,
            c[1].parse().unwrap_or(0),
            c[2].parse().unwrap_or(0),
            c[3].parse().unwrap_or(0),
            m0.as_str(),
            m0.start(),
            false,
        );
    }
    for c in RE_MONTH_FIRST.captures_iter(text) {
        let m0 = c.get(0).unwrap();
        if let Some(m) = month_num(&c[1]) {
            let y = expand_year(c[3].parse().unwrap_or(0));
            push_date(
                &mut out,
                y,
                m,
                c[2].parse().unwrap_or(0),
                m0.as_str(),
                m0.start(),
                false,
            );
        }
    }
    for c in RE_DAY_FIRST.captures_iter(text) {
        let m0 = c.get(0).unwrap();
        if let Some(m) = month_num(&c[2]) {
            let y = expand_year(c[3].parse().unwrap_or(0));
            push_date(
                &mut out,
                y,
                m,
                c[1].parse().unwrap_or(0),
                m0.as_str(),
                m0.start(),
                false,
            );
        }
    }
    for c in RE_SLASH.captures_iter(text) {
        let m0 = c.get(0).unwrap();
        if is_clause_numbering(text, m0.start()) {
            continue;
        }
        let a: u32 = c[1].parse().unwrap_or(0);
        let b: u32 = c[2].parse().unwrap_or(0);
        let y = expand_year(c[3].parse().unwrap_or(0));
        push_date(&mut out, y, a, b, m0.as_str(), m0.start(), false); // US month-first
        if a != b {
            // day-first alt: a coin flip, and labelled as one
            push_date(&mut out, y, b, a, m0.as_str(), m0.start(), true);
        }
    }
    for c in RE_DOTTED.captures_iter(text) {
        let m0 = c.get(0).unwrap();
        if is_clause_numbering(text, m0.start()) {
            continue;
        }
        let a: u32 = c[1].parse().unwrap_or(0);
        let b: u32 = c[2].parse().unwrap_or(0);
        // Both components in day-and-month range, opening a line: that is
        // outline numbering, not the one place a document writes a bare date.
        // "20.07.2026" on its own line survives, because 20 is not a month.
        if a <= 12 && b <= 12 && is_line_leading(text, m0.start()) {
            continue;
        }
        let y = expand_year(c[3].parse().unwrap_or(0));
        push_date(&mut out, y, b, a, m0.as_str(), m0.start(), true); // European day-first
        if a != b {
            push_date(&mut out, y, a, b, m0.as_str(), m0.start(), true); // US alt
        }
    }
    // Sort ambiguous-last within an offset so the dedup below keeps the
    // confident reading when two patterns agree on the same date.
    out.sort_by_key(|d| (d.offset, d.ambiguous));
    out.dedup_by(|a, b| a.iso == b.iso && a.offset == b.offset);
    out
}

/// Full 5a harvest over converted markdown.
/// `head_chars`/`tail_chars` bound the excerpt sizes.
/// Largest char-boundary offset `<= i`, so `&s[..floor]` / `&s[floor..]` can't
/// split a multi-byte codepoint. Slicing at raw byte offsets (6000 / len-2500)
/// panics on any document whose text straddles those cut points — trivially
/// reachable with a single accented or CJK character.
fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

pub fn harvest(markdown: &str) -> Harvest {
    let mut h = Harvest::default();
    let head_len = floor_char_boundary(markdown, 6000);
    let head = &markdown[..head_len];
    let tail_start = floor_char_boundary(markdown, markdown.len().saturating_sub(2500));
    let tail = &markdown[tail_start..];

    // Dates: first pages + last page only (naming never needs page 247).
    h.dates = extract_dates(head);
    for mut d in extract_dates(tail) {
        d.offset += tail_start;
        let has_same_date = h.dates.iter().any(|e| e.iso == d.iso);
        let has_confident_reading = h.dates.iter().any(|e| e.iso == d.iso && !e.ambiguous);
        // The first-page slice wins ordinary duplicates, but a confident
        // reading in the tail must survive an ambiguous alternate in the head.
        // Otherwise the checker can mark a date as a coin flip even though the
        // same document prints it unambiguously later on.
        if !has_same_date || (!d.ambiguous && !has_confident_reading) {
            h.dates.push(d);
        }
    }

    for c in RE_SUBJECT.captures_iter(head) {
        h.subject_lines.push(clean_line(&c[1]));
    }
    for c in RE_EMAIL_HDR.captures_iter(head) {
        let line = clean_line(&c[0]);
        if !h.subject_lines.contains(&line) {
            h.subject_lines.push(line);
        }
        if h.subject_lines.len() > 12 {
            break;
        }
    }
    for c in RE_HEADING.captures_iter(head) {
        h.headings.push(clean_line(&c[1]));
        if h.headings.len() > 8 {
            break;
        }
    }
    for c in RE_ALLCAPS_LINE.captures_iter(head) {
        let line = clean_line(&c[1]);
        if !h.headings.contains(&line) {
            h.headings.push(line);
        }
        if h.headings.len() > 12 {
            break;
        }
    }
    for c in RE_CAPTION.captures_iter(head) {
        h.caption_lines.push(clean_line(c.get(0).unwrap().as_str()));
        if h.caption_lines.len() > 6 {
            break;
        }
    }

    // First ~40 non-empty lines of page 1.
    h.head_excerpt = head
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(40)
        .collect::<Vec<_>>()
        .join("\n");

    // Signature-block region: last 15 non-empty lines.
    let tail_lines: Vec<&str> = tail.lines().filter(|l| !l.trim().is_empty()).collect();
    let sig_start = tail_lines.len().saturating_sub(15);
    h.signature_block = tail_lines[sig_start..].join("\n");

    h
}

fn clean_line(s: &str) -> String {
    s.trim()
        .trim_end_matches(['.', ',', ';'])
        .replace('\t', " ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn isos(text: &str) -> Vec<String> {
        extract_dates(text).into_iter().map(|d| d.iso).collect()
    }

    #[test]
    fn parses_common_formats() {
        // (text, iso that must be found)
        let cases: &[(&str, &str)] = &[
            ("Dated July 20, 2026.", "2026-07-20"),
            ("Also 07/20/2026 here", "2026-07-20"),
            ("Also 2026-07-20 here", "2026-07-20"),
            ("Also 20 July 2026 here", "2026-07-20"),
            ("on 3rd March 2025", "2025-03-03"),
            // C13: formats the harvester used to be blind to. A missed format
            // is invisible to both the model and the tripwire, so the document
            // silently falls back to mtime and is labelled "metadata".
            ("filed 2026/07/20 by clerk", "2026-07-20"),
            ("filed 2026.7.20 by clerk", "2026-07-20"),
            ("signed 20-Jul-2026 in Oslo", "2026-07-20"),
            ("signed Jul-20-2026 in Reno", "2026-07-20"),
            ("signed 20 Jul 26 in Oslo", "2026-07-20"),
            ("unterzeichnet 20.07.2026", "2026-07-20"),
            ("Berlin, 20.07.2026", "2026-07-20"),
            // A dotted date alone on its own line is a date line, and must
            // survive the outline-numbering suppression below.
            ("Acme GmbH\n20.07.2026\n\nSehr geehrte", "2026-07-20"),
        ];
        for (text, want) in cases {
            assert!(
                isos(text).iter().any(|x| x == want),
                "{text:?} should yield {want}"
            );
        }
    }

    #[test]
    fn ambiguous_slash_records_both_readings() {
        let d = extract_dates("effective 03/04/2026");
        assert!(d.iter().any(|x| x.iso == "2026-03-04" && !x.ambiguous));
        // C12: the day-first reading is a coin flip and now says so, so the
        // checker can flag a manifest that rests on it alone.
        assert!(d.iter().any(|x| x.iso == "2026-04-03" && x.ambiguous));
    }

    #[test]
    fn unambiguous_forms_are_not_marked_ambiguous() {
        for t in [
            "dated 2026-07-20",
            "dated 2026/07/20",
            "dated July 20, 2026",
            "dated 20 July 2026",
        ] {
            let d = extract_dates(t);
            assert!(
                d.iter().any(|x| x.iso == "2026-07-20" && !x.ambiguous),
                "{t}"
            );
        }
    }

    #[test]
    fn rejects_impossible_dates() {
        let d = extract_dates("on 02/30/2026 and 13/45/2026");
        assert!(d.iter().all(|x| x.iso != "2026-02-30"));
    }

    #[test]
    fn two_digit_year_expansion() {
        let d = extract_dates("signed 7/20/26 and archived 1/2/99");
        assert!(d.iter().any(|x| x.iso == "2026-07-20"));
        assert!(d.iter().any(|x| x.iso == "1999-01-02"));
    }

    #[test]
    fn three_digit_years_are_not_dates() {
        // "0345" would sail past a naive \d{2,4} year group and then be blamed
        // on the model as an out-of-range date.
        assert!(isos("clause 3/4/345 applies").is_empty());
    }

    #[test]
    fn clause_and_version_numbering_mints_no_dates() {
        let cases: &[&str] = &[
            "Section 1-2-3 of the lease",
            "Rule 12-31-99 of the local rules",
            "version 3-1-24 of the handbook",
            "Exhibit 1/2/03 attached",
            "see Part 4-5-2026 hereof",
            "Docket No. 12-31-99",
            "release 3.1.24 notes",
            "Article 2026-07-20 hereof",
            // Unprefixed outline numbering: at the start of a line there is no
            // lead word for is_clause_numbering to see, so these four each
            // minted one or two phantom dates (1.2.34 -> 2034-02-01 AND
            // 2034-01-02) which filter.rs then handed the model as
            // "DATES FOUND IN DOCUMENT".
            "1.2.34 The Tenant shall pay rent",
            "4.5.67 Termination",
            "10.1.20 Notices",
            "12.31.99 header",
            // Same shape with a 4-digit tail: caught by the line-leading rule.
            "1.2.2034 The Tenant shall pay rent",
            "  2.10.2031 Quiet enjoyment",
            "- 3.4.2025 Assignment and subletting",
            // A dotted 2-digit year is indistinguishable from a version string,
            // so it is deliberately no longer read as a date at all.
            "unterzeichnet 20.07.26",
        ];
        for t in cases {
            assert!(
                isos(t).is_empty(),
                "{t:?} minted phantom dates: {:?}",
                isos(t)
            );
        }
    }

    #[test]
    fn dotted_dates_survive_where_a_date_really_is_written() {
        // The outline-numbering suppression is deliberately narrow: it needs
        // both components in 1..=12 *and* the start of a line. Anything a
        // European letterhead actually writes still parses.
        for (t, want) in [
            ("Berlin, 01.02.2026", "2026-02-01"),
            ("Datum: 03.04.2026", "2026-04-03"),
            ("20.07.2026", "2026-07-20"),
            ("31.12.2025 Jahresabschluss", "2025-12-31"),
        ] {
            assert!(
                isos(t).iter().any(|x| x == want),
                "{t:?} should yield {want}"
            );
        }
    }

    #[test]
    fn ordinary_prose_still_yields_dates_next_to_words() {
        // The clause suppression must not swallow real dates: only the listed
        // numbering words suppress, not any preceding word.
        assert!(isos("signed on 12-31-99 by both parties").contains(&"1999-12-31".to_string()));
    }

    #[test]
    fn harvest_finds_subject_and_caption() {
        let md = "ACME CORPORATION\n\nRE: Termination of Employment - John Smith\n\nSmith v. Acme Corp., Case No. 26-cv-01234\n\nDear Mr. Smith,\nThis letter confirms...";
        let h = harvest(md);
        assert!(h.subject_lines.iter().any(|s| s.contains("Termination")));
        assert!(!h.caption_lines.is_empty());
    }

    #[test]
    fn harvest_keeps_a_confident_tail_reading_of_an_ambiguous_head_date() {
        let md = format!(
            "effective 04/03/2026\n{}March 4, 2026",
            "filler ".repeat(1_100)
        );
        let h = harvest(&md);
        let readings: Vec<&FoundDate> = h
            .dates
            .iter()
            .filter(|date| date.iso == "2026-03-04")
            .collect();

        assert!(
            readings.iter().any(|date| date.ambiguous),
            "the head reading should still be retained"
        );
        assert!(
            readings.iter().any(|date| !date.ambiguous),
            "a later unambiguous reading must not be discarded as a duplicate"
        );
    }

    #[test]
    fn unicode_boundary_in_head_does_not_panic() {
        // '☃' occupies bytes 5999..6002, straddling the 6000-byte head cut.
        let md = format!(
            "{}☃ trailing text past the head boundary.",
            "a".repeat(5999)
        );
        let _ = harvest(&md); // must not panic
    }

    #[test]
    fn unicode_boundary_in_tail_does_not_panic() {
        // A tail of multi-byte chars must not panic at the tail-start cut.
        let md = format!("{}{}", "a".repeat(1000), "☃".repeat(1000));
        let _ = harvest(&md); // must not panic
    }
}
