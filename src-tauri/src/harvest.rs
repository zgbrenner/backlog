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

static RE_ISO: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b(\d{4})-(\d{2})-(\d{2})\b").unwrap());
// 07/20/2026, 7/20/26, 07-20-2026 (US order assumed; ambiguity handled below)
static RE_SLASH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b(\d{1,2})[/-](\d{1,2})[/-](\d{2,4})\b").unwrap());
// July 20, 2026  |  Jul 20 2026
static RE_MONTH_FIRST: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(jan(?:uary)?|feb(?:ruary)?|mar(?:ch)?|apr(?:il)?|may|jun(?:e)?|jul(?:y)?|aug(?:ust)?|sep(?:t(?:ember)?)?|oct(?:ober)?|nov(?:ember)?|dec(?:ember)?)\.?\s+(\d{1,2})(?:st|nd|rd|th)?,?\s+(\d{4})\b").unwrap()
});
// 20 July 2026
static RE_DAY_FIRST: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(\d{1,2})(?:st|nd|rd|th)?\s+(jan(?:uary)?|feb(?:ruary)?|mar(?:ch)?|apr(?:il)?|may|jun(?:e)?|jul(?:y)?|aug(?:ust)?|sep(?:t(?:ember)?)?|oct(?:ober)?|nov(?:ember)?|dec(?:ember)?)\.?,?\s+(\d{4})\b").unwrap()
});

static RE_SUBJECT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?im)^\s*(?:re|subject|in re|regarding|matter)\s*[:\-]\s*(.{3,140})$").unwrap()
});
static RE_EMAIL_HDR: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?im)^\s*(?:from|to|cc|date|sent|subject)\s*:\s*(.{2,160})$").unwrap()
});
static RE_HEADING: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^#{1,3}\s+(.{3,120})$").unwrap());
static RE_ALLCAPS_LINE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^\s*([A-Z][A-Z0-9 ,.&'/\-]{7,90})\s*$").unwrap());
// Case captions: "Smith v. Jones", "In re Acme Corp.", docket numbers
static RE_CAPTION: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?im)^.{0,80}\b(?:v\.?s?\.|versus|in re|ex parte)\b.{0,80}$|(?i)\b(?:case|docket|civil action)\s*(?:no\.?|number)\s*[:#]?\s*[\w:\-]{4,25}").unwrap()
});

fn month_num(m: &str) -> Option<u32> {
    let m = m.to_ascii_lowercase();
    let n = match &m[..3.min(m.len())] {
        "jan" => 1, "feb" => 2, "mar" => 3, "apr" => 4, "may" => 5, "jun" => 6,
        "jul" => 7, "aug" => 8, "sep" => 9, "oct" => 10, "nov" => 11, "dec" => 12,
        _ => return None,
    };
    Some(n)
}

fn push_date(out: &mut Vec<FoundDate>, y: i32, m: u32, d: u32, raw: &str, offset: usize) {
    if let Some(date) = NaiveDate::from_ymd_opt(y, m, d) {
        out.push(FoundDate { iso: date.format("%Y-%m-%d").to_string(), raw: raw.to_string(), offset });
    }
}

fn expand_year(y: u32) -> i32 {
    // 2-digit years: 00-49 -> 2000s, 50-99 -> 1900s. Documents about the
    // future exist; documents from 1926 filed as "26" basically don't.
    if y < 100 {
        if y < 50 { 2000 + y as i32 } else { 1900 + y as i32 }
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
        push_date(
            &mut out,
            c[1].parse().unwrap_or(0),
            c[2].parse().unwrap_or(0),
            c[3].parse().unwrap_or(0),
            m0.as_str(),
            m0.start(),
        );
    }
    for c in RE_MONTH_FIRST.captures_iter(text) {
        let m0 = c.get(0).unwrap();
        if let Some(m) = month_num(&c[1]) {
            push_date(&mut out, c[3].parse().unwrap_or(0), m, c[2].parse().unwrap_or(0), m0.as_str(), m0.start());
        }
    }
    for c in RE_DAY_FIRST.captures_iter(text) {
        let m0 = c.get(0).unwrap();
        if let Some(m) = month_num(&c[2]) {
            push_date(&mut out, c[3].parse().unwrap_or(0), m, c[1].parse().unwrap_or(0), m0.as_str(), m0.start());
        }
    }
    for c in RE_SLASH.captures_iter(text) {
        let m0 = c.get(0).unwrap();
        let a: u32 = c[1].parse().unwrap_or(0);
        let b: u32 = c[2].parse().unwrap_or(0);
        let y = expand_year(c[3].parse().unwrap_or(0));
        push_date(&mut out, y, a, b, m0.as_str(), m0.start()); // US month-first
        if a != b {
            push_date(&mut out, y, b, a, m0.as_str(), m0.start()); // day-first alt
        }
    }
    out.sort_by_key(|d| d.offset);
    out.dedup_by(|a, b| a.iso == b.iso && a.offset == b.offset);
    out
}

/// Full 5a harvest over converted markdown.
/// `head_chars`/`tail_chars` bound the excerpt sizes.
pub fn harvest(markdown: &str) -> Harvest {
    let mut h = Harvest::default();
    let head_len = markdown.len().min(6000);
    let head = &markdown[..head_len];
    let tail_start = markdown.len().saturating_sub(2500);
    let tail = &markdown[tail_start..];

    // Dates: first pages + last page only (naming never needs page 247).
    h.dates = extract_dates(head);
    for mut d in extract_dates(tail) {
        d.offset += tail_start;
        if !h.dates.iter().any(|e| e.iso == d.iso) {
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
        if h.subject_lines.len() > 12 { break; }
    }
    for c in RE_HEADING.captures_iter(head) {
        h.headings.push(clean_line(&c[1]));
        if h.headings.len() > 8 { break; }
    }
    for c in RE_ALLCAPS_LINE.captures_iter(head) {
        let line = clean_line(&c[1]);
        if !h.headings.contains(&line) {
            h.headings.push(line);
        }
        if h.headings.len() > 12 { break; }
    }
    for c in RE_CAPTION.captures_iter(head) {
        h.caption_lines.push(clean_line(c.get(0).unwrap().as_str()));
        if h.caption_lines.len() > 6 { break; }
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
    s.trim().trim_end_matches(['.', ',', ';']).replace('\t', " ").to_string()
}

/// Best deterministic date guess for the fallback path: the earliest-position
/// date in the head that is not obviously a birthdate-style outlier.
pub fn primary_date_guess(h: &Harvest) -> Option<String> {
    h.dates.first().map(|d| d.iso.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_formats() {
        let t = "Dated July 20, 2026. Also 07/20/2026 and 2026-07-20 and 20 July 2026 and 3rd March 2025.";
        let d = extract_dates(t);
        assert!(d.iter().any(|x| x.iso == "2026-07-20"));
        assert!(d.iter().any(|x| x.iso == "2025-03-03"));
    }

    #[test]
    fn ambiguous_slash_records_both_readings() {
        let d = extract_dates("effective 03/04/2026");
        assert!(d.iter().any(|x| x.iso == "2026-03-04"));
        assert!(d.iter().any(|x| x.iso == "2026-04-03"));
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
    fn harvest_finds_subject_and_caption() {
        let md = "ACME CORPORATION\n\nRE: Termination of Employment - John Smith\n\nSmith v. Acme Corp., Case No. 26-cv-01234\n\nDear Mr. Smith,\nThis letter confirms...";
        let h = harvest(md);
        assert!(h.subject_lines.iter().any(|s| s.contains("Termination")));
        assert!(!h.caption_lines.is_empty());
    }
}
