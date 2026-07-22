//! §6: the deterministic checker. This is the trust core. The SLM proposes
//! fields; nothing reaches a filesystem or SharePoint without passing here.
//! Every rule is boring on purpose.

use crate::harvest::Harvest;
use chrono::{Datelike, Duration, NaiveDate, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlmOutput {
    pub date: String,        // "YYYY-MM-DD" or "none"
    pub date_source: String, // "document" | "metadata" | "none"
    pub subject: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Validated {
    pub date_iso: String,
    pub date_source: String,
    pub subject: String,
    pub description: String,
    /// Base filename without extension: "YYYY-MM-DD Subject".
    pub base_name: String,
    pub soft_flags: Vec<String>,
}

#[derive(Debug, thiserror::Error, Clone, Serialize)]
pub enum CheckError {
    #[error("date '{0}' is not a valid calendar date")]
    BadDate(String),
    #[error("date '{0}' outside plausible range 1900-01-01..today+30d")]
    DateOutOfRange(String),
    #[error("date '{0}' not present in document evidence or file metadata")]
    DateNotInEvidence(String),
    #[error("date_source '{0}' invalid")]
    BadDateSource(String),
    #[error("subject invalid: {0}")]
    BadSubject(String),
    #[error("description invalid: {0}")]
    BadDescription(String),
    #[error("composed filename too long ({0} chars, max {1})")]
    TooLong(usize, usize),
}

static GENERIC_SUBJECTS: &[&str] = &[
    "document", "scan", "scanned document", "untitled", "pdf", "file",
    "attachment", "img", "image", "doc", "new document", "letter",
];

// Anything SharePoint/Windows dislikes, plus '#' and '%' which break some
// SharePoint URL paths, plus control chars.
static RE_ILLEGAL: Lazy<Regex> = Lazy::new(|| Regex::new(r#"[\\/:*?"<>|#%\x00-\x1f]"#).unwrap());
static RE_MULTISPACE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s{2,}").unwrap());
static RE_SSN: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").unwrap());
static RE_CCN: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(?:\d[ -]?){13,19}\b").unwrap());
static RE_SENTENCE_END: Lazy<Regex> = Lazy::new(|| Regex::new(r"[.!?]").unwrap());

pub struct Checker {
    pub max_filename_len: usize,
}

impl Checker {
    pub fn new(max_filename_len: usize) -> Self {
        Self { max_filename_len }
    }

    pub fn check(
        &self,
        out: &SlmOutput,
        harvest: &Harvest,
        file_metadata_dates: &[String], // ISO dates from fs/doc properties
        file_modified_iso: &str,
        ettin_date: Option<&str>, // top DATE span from the Ettin lane, if any
    ) -> Result<Validated, CheckError> {
        let mut soft_flags = Vec::new();

        // ---- date_source sanity -------------------------------------------
        match out.date_source.as_str() {
            "document" | "metadata" | "none" => {}
            other => return Err(CheckError::BadDateSource(other.to_string())),
        }

        // ---- date ----------------------------------------------------------
        let (date_iso, date_source) = if out.date == "none" || out.date_source == "none" {
            // Undated documents exist (policies, org charts). Fall back to the
            // file modified date and be honest about provenance in the index.
            (file_modified_iso.to_string(), "metadata".to_string())
        } else {
            let d = NaiveDate::parse_from_str(&out.date, "%Y-%m-%d")
                .map_err(|_| CheckError::BadDate(out.date.clone()))?;
            let today = Utc::now().date_naive();
            let min = NaiveDate::from_ymd_opt(1900, 1, 1).unwrap();
            if d < min || d > today + Duration::days(30) {
                return Err(CheckError::DateOutOfRange(out.date.clone()));
            }
            // Anti-hallucination tripwire: the date must exist somewhere we
            // can point to.
            let in_doc = harvest.dates.iter().any(|f| f.iso == out.date);
            let in_meta = file_metadata_dates.iter().any(|m| m == &out.date);
            if !in_doc && !in_meta {
                return Err(CheckError::DateNotInEvidence(out.date.clone()));
            }
            let src = if in_doc { "document" } else { "metadata" };
            if src != out.date_source {
                soft_flags.push(format!("DATE_SOURCE_CORRECTED:{}->{}", out.date_source, src));
            }
            (out.date.clone(), src.to_string())
        };

        // Ettin/SLM consistency (soft; the retry ladder handles the hard path).
        if let Some(ed) = ettin_date {
            if date_source == "document" && ed != date_iso {
                soft_flags.push(format!("SPAN_MISMATCH:ettin={ed}"));
            }
        }

        // ---- subject -------------------------------------------------------
        let subject = self.sanitize_subject(&out.subject)?;

        // ---- description ---------------------------------------------------
        let description = Self::validate_description(&out.description, &subject)?;

        // ---- compose -------------------------------------------------------
        let base_name = format!("{date_iso} {subject}");
        // Reserve room for " (99)" collision suffix + "." + a long extension.
        if base_name.chars().count() + 12 > self.max_filename_len {
            return Err(CheckError::TooLong(base_name.chars().count(), self.max_filename_len));
        }

        Ok(Validated { date_iso, date_source, subject, description, base_name, soft_flags })
    }

    pub fn sanitize_subject(&self, raw: &str) -> Result<String, CheckError> {
        let mut s = RE_ILLEGAL.replace_all(raw, " ").to_string();
        s = RE_MULTISPACE.replace_all(&s, " ").trim().to_string();
        s = s.trim_matches(['.', ' ']).to_string();

        let words: Vec<&str> = s.split_whitespace().collect();
        if words.len() < 3 || words.len() > 8 {
            return Err(CheckError::BadSubject(format!(
                "{} words (need 3-8): '{s}'", words.len()
            )));
        }
        let lower = s.to_lowercase();
        if GENERIC_SUBJECTS.contains(&lower.as_str()) {
            return Err(CheckError::BadSubject(format!("generic subject '{s}'")));
        }
        if RE_SSN.is_match(&s) || RE_CCN.is_match(&s) {
            return Err(CheckError::BadSubject(
                "subject contains an identifier pattern (SSN/card-like)".into(),
            ));
        }
        Ok(s)
    }

    fn validate_description(raw: &str, subject: &str) -> Result<String, CheckError> {
        let d = raw.trim().replace(['\n', '\r'], " ");
        let d = RE_MULTISPACE.replace_all(&d, " ").to_string();
        let n = d.chars().count();
        if !(15..=200).contains(&n) {
            return Err(CheckError::BadDescription(format!("{n} chars (need 15-200)")));
        }
        let terminals = RE_SENTENCE_END.find_iter(&d).count();
        let ends_ok = d.ends_with(['.', '!', '?']);
        // Allow internal periods only for common abbreviations by tolerating
        // up to 2 terminal marks when the string ends with one.
        if !ends_ok || terminals > 2 {
            return Err(CheckError::BadDescription(
                "must be exactly one sentence ending in terminal punctuation".into(),
            ));
        }
        if d.to_lowercase() == subject.to_lowercase()
            || d.to_lowercase().trim_end_matches('.') == subject.to_lowercase()
        {
            return Err(CheckError::BadDescription(
                "description merely restates the filename subject".into(),
            ));
        }
        if RE_SSN.is_match(&d) || RE_CCN.is_match(&d) {
            return Err(CheckError::BadDescription(
                "description contains an identifier pattern".into(),
            ));
        }
        Ok(d)
    }
}

/// ISO dates derivable from filesystem metadata for the presence check.
pub fn fs_metadata_dates(path: &std::path::Path) -> (Vec<String>, String) {
    let mut dates = Vec::new();
    let mut modified_iso = Utc::now().date_naive().format("%Y-%m-%d").to_string();
    if let Ok(meta) = std::fs::metadata(path) {
        if let Ok(m) = meta.modified() {
            let dt: chrono::DateTime<Utc> = m.into();
            modified_iso = format!("{:04}-{:02}-{:02}", dt.year(), dt.month(), dt.day());
            dates.push(modified_iso.clone());
        }
        if let Ok(c) = meta.created() {
            let dt: chrono::DateTime<Utc> = c.into();
            dates.push(format!("{:04}-{:02}-{:02}", dt.year(), dt.month(), dt.day()));
        }
    }
    dates.dedup();
    (dates, modified_iso)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harvest;

    fn harvest_with(dates: &[&str]) -> Harvest {
        let text = dates.join(" and ");
        Harvest { dates: harvest::extract_dates(&text), ..Default::default() }
    }

    fn ok_out() -> SlmOutput {
        SlmOutput {
            date: "2026-07-20".into(),
            date_source: "document".into(),
            subject: "Termination Notice for John Smith".into(),
            description: "Letter from Acme Corporation notifying John Smith of employment termination effective July 20, 2026.".into(),
        }
    }

    #[test]
    fn accepts_valid_output() {
        let c = Checker::new(120);
        let h = harvest_with(&["2026-07-20"]);
        let v = c.check(&ok_out(), &h, &[], "2026-07-21", None).unwrap();
        assert_eq!(v.base_name, "2026-07-20 Termination Notice for John Smith");
        assert!(v.soft_flags.is_empty());
    }

    #[test]
    fn rejects_hallucinated_date() {
        let c = Checker::new(120);
        let h = harvest_with(&["2026-01-05"]);
        let e = c.check(&ok_out(), &h, &[], "2026-07-21", None).unwrap_err();
        assert!(matches!(e, CheckError::DateNotInEvidence(_)));
    }

    #[test]
    fn rejects_future_and_ancient_dates() {
        let c = Checker::new(120);
        let mut o = ok_out();
        o.date = "2062-07-20".into();
        let h = harvest_with(&["2062-07-20"]);
        assert!(c.check(&o, &h, &[], "2026-07-21", None).is_err());
        o.date = "1899-12-31".into();
        let h = harvest_with(&["1899-12-31"]);
        assert!(c.check(&o, &h, &[], "2026-07-21", None).is_err());
    }

    #[test]
    fn undated_falls_back_to_metadata() {
        let c = Checker::new(120);
        let mut o = ok_out();
        o.date = "none".into();
        o.date_source = "none".into();
        let v = c.check(&o, &Harvest::default(), &[], "2026-07-21", None).unwrap();
        assert_eq!(v.date_iso, "2026-07-21");
        assert_eq!(v.date_source, "metadata");
    }

    #[test]
    fn strips_illegal_chars_and_rejects_generic() {
        let c = Checker::new(120);
        assert_eq!(
            c.sanitize_subject("Invoice #4411: Acme/Q3 <final>").unwrap(),
            "Invoice 4411 Acme Q3 final"
        );
        assert!(c.sanitize_subject("Scanned Document").is_err()); // 2 words
        assert!(c.sanitize_subject("Document").is_err());
    }

    #[test]
    fn rejects_ssn_in_subject() {
        let c = Checker::new(120);
        assert!(c.sanitize_subject("W2 for 123-45-6789 John Smith").is_err());
    }

    #[test]
    fn description_must_be_one_sentence() {
        let c = Checker::new(120);
        let h = harvest_with(&["2026-07-20"]);
        let mut o = ok_out();
        o.description = "First sentence. Second sentence. Third one here.".into();
        assert!(c.check(&o, &h, &[], "2026-07-21", None).is_err());
        o.description = "no terminal punctuation at all here sadly".into();
        assert!(c.check(&o, &h, &[], "2026-07-21", None).is_err());
    }

    #[test]
    fn span_mismatch_is_soft() {
        let c = Checker::new(120);
        let h = harvest_with(&["2026-07-20", "2026-06-01"]);
        let v = c.check(&ok_out(), &h, &[], "2026-07-21", Some("2026-06-01")).unwrap();
        assert!(v.soft_flags.iter().any(|f| f.starts_with("SPAN_MISMATCH")));
    }
}
