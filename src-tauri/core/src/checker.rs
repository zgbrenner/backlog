//! §6: the deterministic checker. This is the trust core. The SLM proposes
//! fields; nothing reaches a filesystem or SharePoint without passing here.
//! Every rule is boring on purpose.

use crate::harvest::{self, Harvest};
use chrono::{Duration, Local, NaiveDate, TimeZone, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

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
    #[error("date '{0}' outside plausible range 1800-01-01..today+400d")]
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

impl CheckError {
    /// Value-free reason code for logs and audit trails. The `Display` form
    /// embeds the offending subject/date/description (needed to re-prompt the
    /// on-device model), so anything that PERSISTS a rejection must log this
    /// instead — never the raw document-derived text.
    pub fn code(&self) -> &'static str {
        match self {
            CheckError::BadDate(_) => "BAD_DATE",
            CheckError::DateOutOfRange(_) => "DATE_OUT_OF_RANGE",
            CheckError::DateNotInEvidence(_) => "DATE_NOT_IN_EVIDENCE",
            CheckError::BadDateSource(_) => "BAD_DATE_SOURCE",
            CheckError::BadSubject(_) => "BAD_SUBJECT",
            CheckError::BadDescription(_) => "BAD_DESCRIPTION",
            CheckError::TooLong(_, _) => "TOO_LONG",
        }
    }
}

/// Who wrote the fields being checked. A 0.6B model and the office worker in
/// the review pane get the same safety rules (illegal characters, PII, sentence
/// shape) but not the same style rules: the word count and the forward-date
/// ceiling exist to catch a model guessing, and applying them to a human turns
/// the correction pane into a dead end for someone who must never open a
/// terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Model,
    Human,
}

// --- date bounds ---------------------------------------------------------
// The floor catches OCR garbage and truncated years, not genuinely old paper.
// The ceiling is wide because forward-dated leases, policy renewals and hearing
// notices are routine and appear verbatim in the document; anything past
// today+30 still earns a soft flag so the index can be audited.
const DATE_FLOOR_YEAR: i32 = 1800;
const FUTURE_HARD_DAYS: i64 = 400;
const FUTURE_SOFT_DAYS: i64 = 30;
/// Beyond this byte offset a date is body text, not letterhead or a date line.
const HEAD_REGION_BYTES: usize = 1500;

/// How many times `Ledger::reserve_name` may disambiguate one composed name with
/// a `" (n)"` suffix before it gives up and the document is flagged.
///
/// Lives here, next to the length budget that has to hold room for the suffix,
/// because the two are one decision: raising the cap widens the longest possible
/// suffix, and a `FILENAME_TAIL_RESERVE` that no longer covers it would silently
/// produce names past `max_filename_len`. A test below asserts they agree.
///
/// 500 was the previous value and became reachable when the undated-document
/// fallback started naming a whole day's backfill from one mtime: several hundred
/// copies of one templated form filed on one fallback day share a composed name
/// exactly. 2,000 is generous for the thousand-file batches this is built for,
/// while staying far away from where the upward probe's cost matters — the search
/// is O(k) per document and so O(k²) across k identical names, which at 2,000 is
/// about two million indexed point lookups spread over two thousand calls, and at
/// 10,000 would be fifty million.
pub const MAX_NAME_COLLISIONS: u32 = 2000;

/// Characters held back from `max_filename_len` for what gets appended after the
/// composed base name: the widest collision suffix, the dot, and an extension.
///
/// `" (2000)"` is 7, the dot is 1, and the longest extension `RE_TRAILING_EXT`
/// recognises is 4 (`docx`), rounded up to 6 for headroom.
const FILENAME_TAIL_RESERVE: usize = 14;

// --- subject shape -------------------------------------------------------
const SUBJECT_MIN_WORDS: usize = 2;
const SUBJECT_MAX_WORDS: usize = 10;
/// Han/Kana/Thai do not put spaces between words, so a whitespace word count is
/// structurally incapable of passing them. Bound characters instead.
const SUBJECT_MIN_CHARS_UNSPACED: usize = 4;
const SUBJECT_MAX_CHARS_UNSPACED: usize = 40;

// The sidecar schema and review command normally keep these fields small, but
// the checker is the trust boundary and is also callable directly. Check the
// budgets before normalization, regex passes, or error construction so a
// malformed response cannot turn one proposal into several unbounded copies.
const MAX_DATE_INPUT_CHARS: usize = 64;
const MAX_DATE_SOURCE_INPUT_CHARS: usize = 64;
const MAX_SUBJECT_INPUT_CHARS: usize = 4_096;
const MAX_DESCRIPTION_INPUT_CHARS: usize = 4_096;

/// Tokens that carry no information about what a document *is*. A subject whose
/// content words are drawn entirely from this set is a scanner default, not a
/// name — and before this list was consulted ahead of the word count it had
/// never once fired.
static GENERIC_SUBJECT_TOKENS: &[&str] = &[
    "a",
    "attachment",
    "copy",
    "doc",
    "document",
    "documents",
    "file",
    "files",
    "from",
    "image",
    "images",
    "img",
    "letter",
    "microsoft",
    "new",
    "of",
    "pdf",
    "scan",
    "scanned",
    "scanner",
    "scans",
    "the",
    "untitled",
    "word",
];

/// Words that ground nothing: every document has them, so seeing one echoed in
/// the evidence says nothing about whether the subject came from the document.
static UNGROUNDING_TOKENS: &[&str] = &[
    "agreement",
    "and",
    "application",
    "certificate",
    "confirmation",
    "contract",
    "copy",
    "dated",
    "draft",
    "executed",
    "final",
    "for",
    "form",
    "from",
    "invoice",
    "lease",
    "letter",
    "license",
    "memo",
    "memorandum",
    "minutes",
    "notice",
    "notification",
    "order",
    "policy",
    "receipt",
    "regarding",
    "report",
    "request",
    "response",
    "signed",
    "statement",
    "summary",
    "the",
    "with",
];

// Anything SharePoint/Windows dislikes, plus '#' and '%' which break some
// SharePoint URL paths, plus control chars. Replaced with a space.
static RE_ILLEGAL: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"[\\/:*?"<>|#%\x00-\x1f\x7f]"#).unwrap());
// Zero-width, bidi-override and BOM codepoints. These are DELETED rather than
// spaced: a U+202E sits *between* letters, so spacing it would fabricate a word
// boundary. Left in, "Notice of Termination \u{202E}fdp.exe" renders in
// Explorer and SharePoint as "...exe.pdf" — a working phishing primitive built
// out of attacker-influenceable document text, in a file other staff click.
static RE_ZERO_WIDTH: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"[\u{200b}-\u{200f}\u{202a}-\u{202e}\u{2060}\u{2066}-\u{2069}\u{feff}]").unwrap()
});
// NBSP and friends must become plain spaces before the collapse, or they
// survive into the filename and silently break the exact-string comparisons
// dedupe_name and the Power Automate flows rely on.
static RE_UNICODE_SPACE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\p{White_Space}").unwrap());
static RE_MULTISPACE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s{2,}").unwrap());
static RE_SSN: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(\d{3})-(\d{2})-(\d{4})\b").unwrap());
/// A maximal run of digits with optional single-character separators. The
/// Luhn/format decision happens in Rust; a regex cannot do a checksum.
static RE_DIGIT_RUN: Lazy<Regex> = Lazy::new(|| Regex::new(r"\d(?:[\d -]*\d)?").unwrap());
/// Terminal punctuation, including the CJK and Arabic full stops — without them
/// no non-Latin description can ever satisfy the one-sentence rule. Kept in
/// step with `TERMINALS` by `terminal_set_and_regex_agree`.
static RE_SENTENCE_END: Lazy<Regex> = Lazy::new(|| Regex::new(r"[.!?。！？؟]").unwrap());
const TERMINALS: [char; 7] = ['.', '!', '?', '。', '！', '？', '؟'];
/// Scripts that do not separate words with spaces.
static RE_UNSPACED_SCRIPT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[\p{Han}\p{Hiragana}\p{Katakana}\p{Thai}]").unwrap());
/// Trailing serial numbers a scanner appends: "… 001", "… (2)", "… #3".
static RE_TRAILING_SERIAL: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s*[#(]?\d{1,4}\)?$").unwrap());
/// A bare year dangling at the end of a subject ("… - 2026"). Full dates are
/// caught by the harvester in `strip_trailing_dates`; a lone year is below the
/// harvester's bar for a date but still noise in a filename that already
/// starts with one.
static RE_TRAILING_YEAR: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:^|[\s\-_/:,])(?:19|20)\d{2}$").unwrap());
static RE_LEADING_QUALIFIER: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^(?:new|copy of)\s+").unwrap());
/// A file extension echoed back into the subject, which pipeline.rs would then
/// re-append: "Invoice 2024 Q3.pdf" -> "….pdf.pdf".
static RE_TRAILING_EXT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\.(pdf|docx?|xlsx?|pptx?|txt|rtf|odt|ods|odp|csv|msg|eml|jpe?g|png|gif|tiff?|bmp|heic|html?|md|xml|zip)$").unwrap()
});
// Masks for the one-sentence rule. Each turns an internal '.' into '-' so the
// terminal count sees only real sentence ends.
static RE_MASK_ELLIPSIS: Lazy<Regex> = Lazy::new(|| Regex::new(r"\.{2,}").unwrap());
/// The whole numeric run, not one separator: `replace_all` is non-overlapping
/// and `\d\.\d` consumes the digit on both sides, so in "v2.1.3" only "2.1" was
/// masked and the dot in "1.3" was still counted as a sentence end.
static RE_MASK_DECIMAL: Lazy<Regex> = Lazy::new(|| Regex::new(r"\d+(?:\.\d+)+").unwrap());
static RE_MASK_ABBREV: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:inc|corp|co|ltd|llc|plc|no|nos|dept|div|att|attn|ref|est|approx|exh|sched|para|bros|vs|et al|e\.g|i\.e|a\.m|p\.m|mr|mrs|ms|dr|prof|st|ave|rd|blvd|fig|vol|jan|feb|mar|apr|jun|jul|aug|sept?|oct|nov|dec)\.").unwrap()
});
/// Single capital letter + period: "U.S.", "J. Smith", "P.O." — the single most
/// common reason legal and corporate prose blew the raw terminal count.
static RE_MASK_INITIAL: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b[A-Z]\.").unwrap());

pub struct Checker {
    pub max_filename_len: usize,
}

impl Checker {
    pub fn new(max_filename_len: usize) -> Self {
        Self { max_filename_len }
    }

    /// Validate a proposal from the on-device model.
    pub fn check(
        &self,
        out: &SlmOutput,
        harvest: &Harvest,
        file_metadata_dates: &[String], // ISO dates from fs/doc properties
        file_modified_iso: &str,
        ettin_date: Option<&str>, // top DATE span from the Ettin lane, if any
    ) -> Result<Validated, CheckError> {
        self.check_with(
            out,
            harvest,
            file_metadata_dates,
            file_modified_iso,
            ettin_date,
            Source::Model,
        )
    }

    /// Validate a correction typed by the user in the review pane. Same safety
    /// rules; the model-style rules (word count, forward-date ceiling) are the
    /// human's call, because rejecting their answer leaves the file stuck with
    /// no path forward.
    pub fn check_human(
        &self,
        out: &SlmOutput,
        harvest: &Harvest,
        file_metadata_dates: &[String],
        file_modified_iso: &str,
        ettin_date: Option<&str>,
    ) -> Result<Validated, CheckError> {
        self.check_with(
            out,
            harvest,
            file_metadata_dates,
            file_modified_iso,
            ettin_date,
            Source::Human,
        )
    }

    fn check_with(
        &self,
        out: &SlmOutput,
        harvest: &Harvest,
        file_metadata_dates: &[String],
        file_modified_iso: &str,
        ettin_date: Option<&str>,
        source: Source,
    ) -> Result<Validated, CheckError> {
        if exceeds_char_limit(&out.date_source, MAX_DATE_SOURCE_INPUT_CHARS) {
            return Err(CheckError::BadDateSource(format!(
                "input exceeds {MAX_DATE_SOURCE_INPUT_CHARS} characters"
            )));
        }
        if exceeds_char_limit(&out.date, MAX_DATE_INPUT_CHARS) {
            return Err(CheckError::BadDate(format!(
                "input exceeds {MAX_DATE_INPUT_CHARS} characters"
            )));
        }
        if exceeds_char_limit(&out.subject, MAX_SUBJECT_INPUT_CHARS) {
            return Err(CheckError::BadSubject(format!(
                "input exceeds {MAX_SUBJECT_INPUT_CHARS} characters"
            )));
        }
        if exceeds_char_limit(&out.description, MAX_DESCRIPTION_INPUT_CHARS) {
            return Err(CheckError::BadDescription(format!(
                "input exceeds {MAX_DESCRIPTION_INPUT_CHARS} characters"
            )));
        }
        let mut soft_flags = Vec::new();

        // ---- date_source sanity -------------------------------------------
        match out.date_source.as_str() {
            "document" | "metadata" | "none" => {}
            other => return Err(CheckError::BadDateSource(other.to_string())),
        }

        // ---- date ----------------------------------------------------------
        // Only a literal "none" takes the fallback. A model that proposes a
        // real, evidence-backed date but mislabels its provenance must not lose
        // that date — DATE_SOURCE_CORRECTED already records the disagreement.
        // A document with no date evidence anywhere is the case the fallback was
        // written for, and the model's cooperation is not required to reach it.
        //
        // The fallback used to need a literal `"none"` from the model. On real
        // paperwork that is a coin flip at best: a tax page is dense with years
        // ("Tax Year 2022", "Year Acquired"), so a small model offers a
        // plausible date instead of declining, `DateNotInEvidence` correctly
        // refuses it, the ladder re-asks, and the document quarantines as
        // `SLM_FAIL` — having never reached the fallback that existed for it.
        // Measured: 0 of 3 genuinely undated documents were named.
        //
        // "No date evidence" means the *document* has none: `harvest.dates` is
        // empty, which — after `filter.rs` folds the salience and Ettin lanes
        // back into it — covers every date the model was actually shown.
        //
        // It deliberately does **not** also require `file_metadata_dates` to be
        // empty. That reads like the safer condition and is in fact a no-op:
        // `pipeline.rs` always extends that list with the file's own mtime and
        // ctime, so it is never empty for a real file on disk. Gating on it left
        // the fallback exactly as unreachable as it was before — measured at
        // 6 of 18 undated documents named, and those six only because the model
        // happened to guess a date matching the mtime. It is circular besides:
        // the filesystem timestamp cannot be the evidence that forbids falling
        // back to the filesystem timestamp.
        //
        // The central promise survives intact, because this path does not ship
        // the model's date. It discards it and substitutes one that has a real
        // provenance — the file's modified time — recording both the
        // substitution (`DATE_FROM_FILE_MTIME`) and what was thrown away
        // (`DATE_PROPOSAL_DISCARDED`). An unevidenced model date still never
        // reaches a filename. Where the document does contain dates, a
        // mismatched proposal remains a hard rejection: that is what
        // `rejects_hallucinated_date` pins down, and it keeps passing.
        let (date_iso, date_source) = if out.date == "none" {
            Self::mtime_fallback(file_modified_iso, source, &mut soft_flags, None)?
        } else {
            let d = NaiveDate::parse_from_str(&out.date, "%Y-%m-%d")
                .map_err(|_| CheckError::BadDate(out.date.clone()))?;
            Self::range_check(d, &out.date, source, &mut soft_flags)?;
            // Anti-hallucination tripwire: the date must exist somewhere we
            // can point to.
            let evidence: Vec<&harvest::FoundDate> =
                harvest.dates.iter().filter(|f| f.iso == out.date).collect();
            let in_doc = !evidence.is_empty();
            let in_meta = file_metadata_dates.iter().any(|m| m == &out.date);
            if !in_doc && !in_meta {
                // The proposal is unsupported. Whether that is a hallucination or
                // simply an undated document turns on whether the document had
                // any date to be wrong about — so the test is on the *document*,
                // and it belongs here, after the per-date evidence check rather
                // than before it. Testing earlier also discarded dates that
                // metadata genuinely supports, which is what five of these tests
                // caught when this sat above the tripwire.
                if harvest.dates.is_empty() && source == Source::Model {
                    Self::mtime_fallback(
                        file_modified_iso,
                        source,
                        &mut soft_flags,
                        Some(&out.date),
                    )?
                } else {
                    return Err(CheckError::DateNotInEvidence(out.date.clone()));
                }
            } else if let Some(preferred) = (!in_doc && source == Source::Model)
                .then(|| Self::date_printed_on_the_page(harvest))
                .flatten()
            {
                // The date printed on the document outranks the document's
                // embedded properties.
                //
                // A model handed both a dated page and a `created` property will
                // often take the property — measured on a stratified corpus, 14 of
                // 16 documents with a date on page one were named from metadata
                // instead, several of them claiming `date_source: "document"`
                // while proposing a date that appears nowhere in the text. The
                // result is a filename stamped with the day the file was made
                // rather than the day the document is *about*, which is the one
                // thing the name exists to tell you.
                //
                // This is deterministic and it is not the model's answer being
                // trusted: `harvest.dates` is regex evidence read from the
                // document text, so the substituted date provably appears on the
                // page — strictly better provenance than what it replaces. Only
                // unambiguous dates in the head region qualify, because that is
                // where a letterhead or date line lives; a date deep in the body
                // is far likelier to be a reference to some *other* document's
                // date, which is exactly what `DATE_FROM_BODY` exists to warn
                // about.
                let d = NaiveDate::parse_from_str(&preferred.iso, "%Y-%m-%d")
                    .map_err(|_| CheckError::BadDate(preferred.iso.clone()))?;
                Self::range_check(d, &preferred.iso, source, &mut soft_flags)?;
                soft_flags.push(format!("DATE_PREFERRED_FROM_DOCUMENT:{}", out.date));
                (preferred.iso.clone(), "document".to_string())
            } else {
                let src = if in_doc { "document" } else { "metadata" };
                if src != out.date_source {
                    soft_flags.push(format!(
                        "DATE_SOURCE_CORRECTED:{}->{}",
                        out.date_source, src
                    ));
                }
                if in_doc {
                    // A date whose only support is a day-first re-reading of an
                    // ambiguous numeric form is a coin flip; say so rather than
                    // shipping `date_source: "document"` with full confidence.
                    if evidence.iter().all(|f| f.ambiguous) {
                        soft_flags.push("DATE_AMBIGUOUS_FORMAT".into());
                    }
                    // Letterhead and date lines live at the top. A date found
                    // only deep in the body is far likelier to be a reference to
                    // some other document's date.
                    let first = evidence.iter().map(|f| f.offset).min().unwrap_or(0);
                    if first > HEAD_REGION_BYTES {
                        soft_flags.push("DATE_FROM_BODY".into());
                    }
                }
                (out.date.clone(), src.to_string())
            }
        };

        // Ettin/SLM consistency (soft; the retry ladder handles the hard path).
        if let Some(ed) = ettin_date {
            if date_source == "document" && ed != date_iso {
                soft_flags.push(format!("SPAN_MISMATCH:ettin={ed}"));
            }
        }

        // ---- subject -------------------------------------------------------
        let (subject, subject_flags) = self.sanitize_subject_inner(&out.subject, source)?;
        soft_flags.extend(subject_flags);
        if source == Source::Model && subject_grounded(&subject, harvest) == Some(false) {
            soft_flags.push("SUBJECT_UNGROUNDED".into());
        }

        // ---- description ---------------------------------------------------
        // Model prose habitually opens with "The document is a …"; the index
        // wants register style, so strip that deterministically rather than
        // hoping the prompt sticks. Human-typed descriptions pass untouched.
        let described = if source == Source::Model {
            strip_document_preamble(&out.description)
        } else {
            out.description.clone()
        };
        let description = Self::validate_description(&described, &subject, &mut soft_flags)?;

        // ---- compose -------------------------------------------------------
        let base_name = format!("{date_iso} {subject}");
        if base_name.chars().count() + FILENAME_TAIL_RESERVE > self.max_filename_len {
            return Err(CheckError::TooLong(
                base_name.chars().count(),
                self.max_filename_len,
            ));
        }

        Ok(Validated {
            date_iso,
            date_source,
            subject,
            description,
            base_name,
            soft_flags,
        })
    }

    /// The earliest unambiguous date the harvest found in the head region, if any.
    ///
    /// "Head region" is where a letterhead, a date line or a filing stamp sits.
    /// Restricting to it — and to unambiguous forms, so a coin-flip reading of
    /// `04/05/2023` never wins this way — is what keeps this a conservative
    /// preference rather than a licence to pick any number off the page.
    fn date_printed_on_the_page(harvest: &Harvest) -> Option<&harvest::FoundDate> {
        harvest
            .dates
            .iter()
            .filter(|f| !f.ambiguous && f.offset <= HEAD_REGION_BYTES)
            .min_by_key(|f| f.offset)
    }

    /// Name the document from its own modified time, and say so.
    ///
    /// Reached two ways: the model declined with `"none"`, or it proposed a date
    /// that nothing in a dateless document could support. Both are the same
    /// answer — there is no date to read, so use the one fact about the file that
    /// is not a guess — and both get the same parse and range check as any other
    /// date, because this is the one path where a string becomes a filename
    /// without the model's involvement.
    ///
    /// `discarded` carries what the model proposed, when it proposed anything.
    /// `date_source: "metadata"` already means two different things — "the
    /// model's date matched a recorded metadata date" and "we had nothing and
    /// used the mtime" — and this path makes the second common, so recording the
    /// discarded value is what keeps the two distinguishable in the index and
    /// keeps how often the model still invents dates measurable. A bare ISO date
    /// is safe to persist; a subject or description would not be.
    fn mtime_fallback(
        file_modified_iso: &str,
        source: Source,
        soft_flags: &mut Vec<String>,
        discarded: Option<&str>,
    ) -> Result<(String, String), CheckError> {
        let d = NaiveDate::parse_from_str(file_modified_iso, "%Y-%m-%d")
            .map_err(|_| CheckError::BadDate(file_modified_iso.to_string()))?;
        Self::range_check(d, file_modified_iso, source, soft_flags)?;
        soft_flags.push("DATE_FROM_FILE_MTIME".into());
        if let Some(proposed) = discarded {
            soft_flags.push(format!("DATE_PROPOSAL_DISCARDED:{proposed}"));
        }
        Ok((d.format("%Y-%m-%d").to_string(), "metadata".to_string()))
    }

    fn range_check(
        d: NaiveDate,
        raw: &str,
        source: Source,
        soft_flags: &mut Vec<String>,
    ) -> Result<(), CheckError> {
        let today = Utc::now().date_naive();
        let min = NaiveDate::from_ymd_opt(DATE_FLOOR_YEAR, 1, 1).unwrap();
        let hard_max = today + Duration::days(FUTURE_HARD_DAYS);
        if d < min || (source == Source::Model && d > hard_max) {
            return Err(CheckError::DateOutOfRange(raw.to_string()));
        }
        if d > today + Duration::days(FUTURE_SOFT_DAYS) {
            soft_flags.push("DATE_IN_FUTURE".into());
        }
        Ok(())
    }

    pub fn sanitize_subject(&self, raw: &str) -> Result<String, CheckError> {
        self.sanitize_subject_inner(raw, Source::Model)
            .map(|(s, _)| s)
    }

    /// Sanitize a subject the user typed. Illegal characters, PII and the
    /// generic-scanner-default check still apply; the word count does not.
    pub fn sanitize_subject_human(&self, raw: &str) -> Result<String, CheckError> {
        self.sanitize_subject_inner(raw, Source::Human)
            .map(|(s, _)| s)
    }

    fn sanitize_subject_inner(
        &self,
        raw: &str,
        source: Source,
    ) -> Result<(String, Vec<String>), CheckError> {
        let mut flags = Vec::new();

        // NFC first so a decomposed "é" is one char for every rule below and
        // one char in the filename SharePoint stores.
        let mut s: String = raw.nfc().collect();
        s = RE_ZERO_WIDTH.replace_all(&s, "").to_string();
        s = RE_ILLEGAL.replace_all(&s, " ").to_string();
        s = RE_UNICODE_SPACE.replace_all(&s, " ").to_string();
        s = RE_MULTISPACE.replace_all(&s, " ").trim().to_string();
        s = s.trim_matches(['.', ' ']).to_string();

        if s.is_empty() {
            return Err(CheckError::BadSubject("empty after sanitization".into()));
        }

        // The model loves to echo the date it just proposed, which composes
        // into "2026-07-20 2026-07-20 Termination Notice".
        if let Some(stripped) = strip_leading_date(&s) {
            if stripped.is_empty() {
                return Err(CheckError::BadSubject(format!(
                    "subject is only a date: '{s}'"
                )));
            }
            s = stripped;
            flags.push("SUBJECT_DATE_STRIPPED".into());
        }
        // …and to echo the source filename, which composes into "….pdf.pdf".
        if let Some(m) = RE_TRAILING_EXT.find(&s) {
            let stripped = s[..m.start()].trim().to_string();
            if !stripped.is_empty() {
                s = stripped;
                flags.push("SUBJECT_EXT_STRIPPED".into());
            }
        }

        // A dangling separator is stripped from every model subject, not only
        // from one this function trimmed itself.
        //
        // `truncate_to_words` has always cleaned its own cut, but a subject can
        // arrive already ending in one: the JSON schema's `maxLength` stops
        // generation at a fixed character count, so
        // `"Tax Return - Supplemental Income and Loss (Rental Real Estate) -"`
        // shipped exactly like that, with the party it was about to name lost and
        // the separator left pointing at nothing. Widening that cap makes this
        // rarer, not impossible — the cap still exists — so the cleanup belongs
        // here, where every subject passes, rather than only on the trim path.
        //
        // Unflagged on purpose: this drops punctuation, never a word, so there is
        // nothing a reviewer would need to check. `SUBJECT_TRUNCATED` still marks
        // the case where words were actually dropped.
        if source == Source::Model {
            let tidied = trim_dangling_tail(&s);
            if !tidied.is_empty() {
                s = tidied;
            }
            // The model also echoes its date at the END — observed shipping
            // as "… - 2026-08-05 - 2026" — and the schema's character cap
            // leaves clause fragments like "… - Effective" or "… shall
            // recover". Both are model-only habits; a human subject is taken
            // as written.
            if let Some(stripped) = strip_trailing_dates(&s) {
                s = stripped;
                flags.push("SUBJECT_TRAILING_DATE_STRIPPED".into());
            }
            if let Some(stripped) = strip_dangling_words(&s, SUBJECT_MIN_WORDS) {
                s = stripped;
                flags.push("SUBJECT_DANGLING_TAIL_STRIPPED".into());
            }
        }

        // Generic check runs BEFORE the size gate. Behind it, every entry in
        // the list is short enough to die at the word count first, which is why
        // "Scanned Document 001" and "New Microsoft Word Document" sailed
        // through to SharePoint for the entire life of this rule.
        if is_generic_subject(&s) {
            return Err(CheckError::BadSubject(format!("generic subject '{s}'")));
        }

        // Proportion, not presence: one Japanese company name inside an English
        // title is still an English title, and switching it to the character
        // bound rejected "Invoice from 株式会社 Acme Trading Company Limited
        // Group Holdings" at 53 characters. The second clause covers the real
        // target — a subject with no whitespace word structure to count at all.
        if unspaced_script_majority(&s)
            || (RE_UNSPACED_SCRIPT.is_match(&s) && subject_words(&s).len() < 2)
        {
            let n = s.chars().filter(|c| !c.is_whitespace()).count();
            if !(SUBJECT_MIN_CHARS_UNSPACED..=SUBJECT_MAX_CHARS_UNSPACED).contains(&n) {
                return Err(CheckError::BadSubject(format!(
                    "{n} characters (need {SUBJECT_MIN_CHARS_UNSPACED}-{SUBJECT_MAX_CHARS_UNSPACED}): '{s}'"
                )));
            }
        } else {
            let mut words = subject_words(&s);
            if words.is_empty() {
                return Err(CheckError::BadSubject(format!("no word characters: '{s}'")));
            }
            // Too many words is a fixable overshoot; too few is not.
            //
            // A subject's leading words are the informative ones — the form
            // number and the party — so keeping the first `SUBJECT_MAX_WORDS` of
            // an over-long answer preserves what a filename is for. Quarantining
            // a document because the model listed one entity too many spends a
            // human's attention on nothing. Too *few* words cannot be repaired by
            // trimming, so that stays a rejection.
            //
            // Only for `Source::Model`: a human who types a long subject in the
            // review pane means it.
            if source == Source::Model && words.len() > SUBJECT_MAX_WORDS {
                let trimmed = truncate_to_words(&s, SUBJECT_MAX_WORDS);
                let retained = subject_words(&trimmed);
                if retained.len() >= SUBJECT_MIN_WORDS {
                    flags.push("SUBJECT_TRUNCATED".into());
                    s = trimmed;
                    // The word-boundary cut can itself mint a fresh clause
                    // fragment ("… Globex Corporation shall"); clean it the
                    // same way an arriving fragment is cleaned above.
                    if let Some(stripped) = strip_dangling_words(&s, SUBJECT_MIN_WORDS) {
                        s = stripped;
                        flags.push("SUBJECT_DANGLING_TAIL_STRIPPED".into());
                    }
                    words = subject_words(&s);
                }
            }
            if source == Source::Model
                && !(SUBJECT_MIN_WORDS..=SUBJECT_MAX_WORDS).contains(&words.len())
            {
                return Err(CheckError::BadSubject(format!(
                    "{} words (need {SUBJECT_MIN_WORDS}-{SUBJECT_MAX_WORDS}): '{s}'",
                    words.len()
                )));
            }
        }

        if contains_ssn(&s) || contains_card_number(&s) {
            return Err(CheckError::BadSubject(
                "subject contains an identifier pattern (SSN/card-like)".into(),
            ));
        }
        Ok((s, flags))
    }

    /// The input up to and including its first sentence-ending mark, or `None`
    /// when there is no complete sentence in it at all.
    ///
    /// Operates on the same masked form `validate_description` counts with, so
    /// an abbreviation's full stop (`Inc.`, `e.g.`) is not mistaken for the end
    /// of the sentence. The mask is byte-length preserving, so an offset found in
    /// it indexes the original unchanged.
    fn first_sentence(d: &str) -> Option<String> {
        let masked = mask_non_terminals(d);
        let end = RE_SENTENCE_END.find(&masked)?.end();
        Some(d[..end].to_string())
    }

    fn validate_description(
        raw: &str,
        subject: &str,
        flags: &mut Vec<String>,
    ) -> Result<String, CheckError> {
        let d = raw.trim().replace(['\n', '\r'], " ");
        let d = RE_MULTISPACE.replace_all(&d, " ").to_string();

        // Keep the first whole sentence when the model wrote past one.
        //
        // A model that runs on into a second sentence, or that is cut off
        // mid-sentence by the generation cap, has still said something true and
        // useful in its first sentence. Rejecting the whole answer for that
        // quarantines a document over punctuation. Trimming is deterministic,
        // recorded, and re-validated below like any other input — it cannot
        // invent content, only drop the tail.
        //
        // Measured cause: the JSON schema capped `description` at exactly the
        // checker's own 200-character limit, so llama.cpp's grammar stopped
        // generation mid-word and produced a trailing fragment
        // (`"...supporting worksheets. The return was "`). The schema now leaves
        // headroom so the model finishes its sentences; this handles what is
        // left, including a genuine two-sentence answer.
        let d = match Self::first_sentence(&d) {
            Some(trimmed) if trimmed != d => {
                flags.push("DESCRIPTION_TRIMMED_TO_ONE_SENTENCE".into());
                trimmed
            }
            _ => d,
        };

        let n = d.chars().count();
        // An unspaced script says as much in 8 characters as English does in 15.
        let min = if unspaced_script_majority(&d) { 8 } else { 15 };
        if !(min..=200).contains(&n) {
            return Err(CheckError::BadDescription(format!(
                "{n} chars (need {min}-200)"
            )));
        }
        // Count terminals only after masking abbreviations, initials, decimals
        // and ellipses. The old raw count tolerated two terminals, so "U.S."
        // alone consumed both slots and rejected exactly the legal/corporate
        // prose this corpus is made of — while the rambling two-sentence
        // description the rule exists to prevent still got through.
        let masked = mask_non_terminals(&d);
        let terminals = RE_SENTENCE_END.find_iter(&masked).count();
        let ends_ok = RE_SENTENCE_END
            .find_iter(&masked)
            .last()
            .is_some_and(|m| m.end() == masked.len());
        if terminals != 1 || !ends_ok {
            return Err(CheckError::BadDescription(
                "must be exactly one sentence ending in terminal punctuation".into(),
            ));
        }
        let dl = d.to_lowercase();
        let sl = subject.to_lowercase();
        if dl == sl || dl.trim_end_matches(['.', '。']) == sl {
            return Err(CheckError::BadDescription(
                "description merely restates the filename subject".into(),
            ));
        }
        if contains_ssn(&d) || contains_card_number(&d) {
            return Err(CheckError::BadDescription(
                "description contains an identifier pattern".into(),
            ));
        }
        Ok(d)
    }
}

/// Return whether `s` contains more than `max` Unicode scalar values without
/// counting or allocating the whole input. The checker's error paths must stay
/// bounded even when a caller supplies an oversized field.
fn exceeds_char_limit(s: &str, max: usize) -> bool {
    s.chars().nth(max).is_some()
}

/// True when most of the letters are from a script that does not separate words
/// with spaces, which is when a whitespace word count is structurally incapable
/// of judging the text. A single Han/Kana/Thai character is not enough.
fn unspaced_script_majority(s: &str) -> bool {
    let mut buf = [0u8; 4];
    let mut letters = 0usize;
    let mut unspaced = 0usize;
    for c in s.chars().filter(|c| c.is_alphanumeric()) {
        letters += 1;
        if RE_UNSPACED_SCRIPT.is_match(c.encode_utf8(&mut buf)) {
            unspaced += 1;
        }
    }
    letters > 0 && unspaced * 2 > letters
}

/// Split on whitespace and on the joiners a filename-derived title uses, so
/// "2026-07-20_Termination_Notice_Smith" is not counted as a single word.
/// Keep at most `max` words of `s`, cutting at a word boundary in the original
/// text and dropping whatever separator or punctuation the cut left dangling.
///
/// Splits on the same characters as [`subject_words`], so the count this produces
/// agrees with the count that is checked. Returns the input unchanged when it
/// already has `max` words or fewer.
fn truncate_to_words(s: &str, max: usize) -> String {
    let is_sep = |c: char| c.is_whitespace() || c == '-' || c == '_' || c == '/';
    let mut words = 0usize;
    let mut in_word = false;
    let mut cut = s.len();
    for (offset, ch) in s.char_indices() {
        if is_sep(ch) {
            in_word = false;
            continue;
        }
        if !in_word {
            in_word = true;
            words += 1;
            if words > max {
                cut = offset;
                break;
            }
        }
    }
    // A cut mid-list leaves things like "Service, " or "Entity / " behind.
    trim_dangling_tail(&s[..cut])
}

/// Strip trailing punctuation that only makes sense if something followed it.
///
/// Shared by the word trim and by `sanitize_subject_inner`, because a subject can
/// end this way for two unrelated reasons — this function's own cut, or the JSON
/// schema stopping generation at a character count — and both leave a filename
/// ending in a separator that points at nothing.
fn trim_dangling_tail(s: &str) -> String {
    let is_sep = |c: char| c.is_whitespace() || c == '-' || c == '_' || c == '/';
    s.trim_end_matches(|c: char| is_sep(c) || matches!(c, ',' | ';' | ':' | '.' | '&' | '('))
        .to_string()
}

fn subject_words(s: &str) -> Vec<&str> {
    s.split(|c: char| c.is_whitespace() || c == '-' || c == '_' || c == '/')
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .collect()
}

/// A leading date the model echoed from its own `date` field, plus whatever
/// separator followed it. Returns `None` when the subject does not start with a
/// date.
fn strip_leading_date(s: &str) -> Option<String> {
    // '_' is a regex word character, so "2026-07-20_Notice" hides its own date
    // from the harvester's word boundaries. Probe a copy with the joiner
    // spaced out; both are one byte, so offsets and lengths still line up.
    let probe = s.replace('_', " ");
    let d = harvest::extract_dates(&probe)
        .into_iter()
        .find(|d| d.offset == 0)?;
    let rest = &s[d.raw.len()..];
    Some(
        rest.trim_start_matches([' ', '-', '_', ':', ',', '.', '–', '—'])
            .trim()
            .to_string(),
    )
}

/// A date (or bare year) the model appended to the end of the subject, plus
/// whatever separator precedes it. Mirrors `strip_leading_date` — the model
/// echoes its own `date` field at either end — and loops, because the observed
/// failure shape is stacked: `"… - 2026-08-05 - 2026"`. The filename already
/// begins with the validated date; a second copy in the subject is only noise.
fn strip_trailing_dates(s: &str) -> Option<String> {
    let mut out = s.to_string();
    let mut stripped_any = false;
    loop {
        let probe = out.replace('_', " ");
        let tail_date = harvest::extract_dates(&probe)
            .into_iter()
            .find(|d| d.offset + d.raw.len() == probe.len() && d.offset > 0);
        if let Some(d) = tail_date {
            out = trim_dangling_tail(out[..d.offset].trim_end());
            stripped_any = true;
            continue;
        }
        if let Some(m) = RE_TRAILING_YEAR.find(&out) {
            if m.start() > 0 {
                out = trim_dangling_tail(out[..m.start()].trim_end());
                stripped_any = true;
                continue;
            }
        }
        break;
    }
    (stripped_any && !out.is_empty()).then_some(out)
}

/// Function words that cannot end a subject: a truncation cut mid-clause, or
/// the schema's character cap stopping generation, leaves tails like
/// "… - Effective" or "… shall recover". Trailing-position only — every one of
/// these words is legitimate mid-subject.
const DANGLING_TAIL_WORDS: [&str; 26] = [
    "a",
    "an",
    "and",
    "at",
    "but",
    "by",
    "dated",
    "effective",
    "for",
    "from",
    "in",
    "is",
    "of",
    "on",
    "or",
    "per",
    "re",
    "regarding",
    "shall",
    "the",
    "to",
    "was",
    "which",
    "will",
    "with",
    "would",
];

/// Auxiliaries that orphan the single word after them: "shall recover" is a
/// clause fragment even though "recover" alone would survive the tail list.
const DANGLING_PAIR_HEADS: [&str; 9] = [
    "are", "be", "been", "is", "shall", "to", "was", "were", "will",
];

/// Drop clause fragments off the end of a model subject, keeping at least
/// `min_words`. Returns `None` when nothing was stripped.
fn strip_dangling_words(s: &str, min_words: usize) -> Option<String> {
    let mut out = s.to_string();
    let mut stripped_any = false;
    loop {
        let words = subject_words(&out);
        if words.len() <= min_words {
            break;
        }
        let last = words[words.len() - 1].to_lowercase();
        let penult = words[words.len() - 2].to_lowercase();
        let drop = if DANGLING_TAIL_WORDS.contains(&last.as_str()) {
            1
        } else if words.len() > min_words + 1 && DANGLING_PAIR_HEADS.contains(&penult.as_str()) {
            2
        } else {
            break;
        };
        let trimmed = truncate_to_words(&out, words.len() - drop);
        if trimmed.is_empty() || trimmed == out {
            break;
        }
        out = trimmed;
        stripped_any = true;
    }
    (stripped_any && !out.is_empty()).then_some(out)
}

/// True when every content token is a scanner default. Serial suffixes and
/// "New "/"Copy of " prefixes are stripped first because "Scanned Document 001"
/// and the bare "Scanned Document" are the same failure.
fn is_generic_subject(s: &str) -> bool {
    let core = RE_TRAILING_SERIAL.replace(s, "");
    let core = RE_LEADING_QUALIFIER.replace(&core, "");
    let mut any = false;
    for tok in core.split(|c: char| !c.is_alphanumeric()) {
        if tok.is_empty() {
            continue;
        }
        any = true;
        if !GENERIC_SUBJECT_TOKENS.contains(&tok.to_lowercase().as_str()) {
            return false;
        }
    }
    any
}

/// Every scrap of text the harvest could point at, lowercased.
fn evidence_text(h: &Harvest) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for v in [&h.subject_lines, &h.headings, &h.caption_lines] {
        parts.extend(v.iter().map(String::as_str));
    }
    parts.push(&h.head_excerpt);
    parts.push(&h.signature_block);
    parts.join("\n").to_lowercase()
}

/// Does the proposed subject actually come from the document?
///
/// `None` when the question cannot be answered — no evidence text at all, or a
/// subject made entirely of doc-type words — because a flag we cannot justify
/// is worse than no flag.
///
/// Matching is by whole token and a majority of the subject's content tokens
/// must be found. The first cut was a substring search that accepted on any one
/// token: against a document headed "TERMINATION OF EMPLOYMENT - JOHN SMITH" it
/// grounded "Termination Notice for Jane Doe" and "…for Maria Alvarez" alike,
/// so it never fired for the invented-name failure it exists to catch, and
/// "law" grounded on "unlawful". It stays a soft flag, and it stays imperfect:
/// a half-invented "…for Jane Smith" still clears the majority.
pub fn subject_grounded(subject: &str, harvest: &Harvest) -> Option<bool> {
    let text = evidence_text(harvest);
    let evidence: std::collections::HashSet<&str> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    if evidence.is_empty() {
        return None;
    }
    let tokens: Vec<String> = subject
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|t| t.chars().count() >= 3)
        .filter(|t| !t.chars().all(|c| c.is_ascii_digit()))
        .filter(|t| !UNGROUNDING_TOKENS.contains(&t.as_str()))
        .collect();
    if tokens.is_empty() {
        return None;
    }
    let found = tokens
        .iter()
        .filter(|t| evidence.contains(t.as_str()))
        .count();
    Some(found * 2 >= tokens.len())
}

/// Replace the periods that are not sentence ends with '-', preserving length
/// so the "terminal is the final character" test stays meaningful.
///
/// The description's own last character is held back from the mask pass. A
/// trailing abbreviation ("…issued to Beta Holdings Inc.", "…at 4 Elm St.")
/// otherwise has its full stop rewritten, leaving zero terminals and rejecting
/// ordinary output — the same dead end as the raw terminal count, reached from
/// the other side.
fn mask_non_terminals(d: &str) -> String {
    let (body, tail) = match d.chars().next_back() {
        Some(c) if TERMINALS.contains(&c) => d.split_at(d.len() - c.len_utf8()),
        _ => (d, ""),
    };
    let mut m = body.to_string();
    for re in [
        &*RE_MASK_ELLIPSIS,
        &*RE_MASK_DECIMAL,
        &*RE_MASK_ABBREV,
        &*RE_MASK_INITIAL,
    ] {
        m = re
            .replace_all(&m, |c: &regex::Captures| c[0].replace('.', "-"))
            .to_string();
    }
    m.push_str(tail);
    m
}

/// The PAN lengths that go with a run's issuer prefix, empty when no real
/// issuer starts that way.
///
/// Luhn alone accepts one arbitrary digit string in ten, so a checksum with a
/// free starting position is not a filter at all: sliding 13..=19 over a
/// 20-digit run tries 35 windows and rejected 197 of 200 pseudo-random runs
/// (`luhn_alone_is_not_a_filter` measures it). The issuer prefix is what makes
/// the rule mean something.
fn issuer_lengths(digits: &[u8]) -> &'static [usize] {
    let d = |i: usize| digits.get(i).map_or(u32::MAX, |v| u32::from(*v));
    let n2 = d(0) * 10 + d(1);
    let n3 = n2 * 10 + d(2);
    let n4 = n3 * 10 + d(3);
    if d(0) == 4 {
        &[13, 16, 19] // Visa
    } else if (51..=55).contains(&n2) || (2221..=2720).contains(&n4) {
        &[16] // Mastercard
    } else if n2 == 34 || n2 == 37 {
        &[15] // American Express
    } else if n4 == 6011 || n2 == 65 {
        &[16] // Discover
    } else if (300..=305).contains(&n3) || n4 == 3095 || n2 == 36 || n2 == 38 || n2 == 39 {
        &[14] // Diners Club
    } else {
        &[]
    }
}

/// True when a run of digits really is a payment card number, or begins with
/// one.
///
/// Only offset 0 is ever tried, and only at the lengths that issuer's cards are
/// actually printed at, so "Tracking 12345678901234567890 received" and
/// "Meter 000000000000000000 unit" — the latter Luhn-valid, being all zeros —
/// are not identifiers. A PAN buried in the *middle* of a longer run is
/// deliberately not detected: no anchor is left, and the false-positive rate
/// that buys costs the office worker three model attempts and a Needs Review
/// entry on an invoice reference number.
fn pan_at_start(digits: &[u8]) -> bool {
    let lengths = issuer_lengths(digits);
    if lengths.is_empty() {
        return false;
    }
    if (13..=19).contains(&digits.len()) && luhn_ok(digits) {
        return true;
    }
    lengths
        .iter()
        .any(|&n| n <= digits.len() && luhn_ok(&digits[..n]))
}

fn luhn_ok(digits: &[u8]) -> bool {
    let mut sum = 0u32;
    for (i, d) in digits.iter().rev().enumerate() {
        let mut v = u32::from(*d);
        if i % 2 == 1 {
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
    }
    sum.is_multiple_of(10)
}

/// True when the text contains something that really is a payment card number.
///
/// The old rule was `\b(?:\d[ -]?){13,19}\b` with no checksum, which hard-
/// rejected "Invoice covering 2026-07-20 2026-07-21 period", "Meter readings
/// 1234567 8901234 recorded" and "ISBN 978-0-13-235088-4 copy" — invoices and
/// timesheets, i.e. exactly the high-volume document types this appliance
/// exists for — while telling the user their file "contains an identifier
/// pattern".
fn contains_card_number(s: &str) -> bool {
    for m in RE_DIGIT_RUN.find_iter(s) {
        let run = m.as_str();
        // A card is written 4111111111111111, "4111 1111 1111 1111" or
        // "4111-1111-1111-1111" — never with a mix. A mixed run is a date
        // range or a docket string.
        let seps: Vec<char> = run.chars().filter(|c| !c.is_ascii_digit()).collect();
        let sep = match seps.first() {
            None => None,
            Some(&c) if seps.iter().all(|&x| x == c) => Some(c),
            _ => continue,
        };
        if let Some(sep) = sep {
            // Card groupings are 4-4-4-4 or 4-6-5. An ISBN's 3-1-2-6-1 and a
            // pair of 7-digit meter readings are not cards.
            if run.split(sep).any(|g| g.len() < 3 || g.len() > 6) {
                continue;
            }
        }
        let digits: Vec<u8> = run
            .chars()
            .filter_map(|c| c.to_digit(10).map(|d| d as u8))
            .collect();
        if digits.len() < 13 {
            continue;
        }
        // A separated run is the whole number by construction; an unseparated
        // one may be a PAN with trailing digits glued on, which the old
        // trailing \b meant was not examined at all. Either way the number has
        // to start where the run starts.
        if pan_at_start(&digits) {
            return true;
        }
    }
    false
}

/// True when the text contains something that really is an SSN. A trailing
/// group that is a plausible year, or a match the date harvester reads as a
/// date, is a date — and "contains an identifier pattern" is then both false
/// and unactionable.
fn contains_ssn(s: &str) -> bool {
    RE_SSN.captures_iter(s).any(|c| {
        let tail: i32 = c[3].parse().unwrap_or(0);
        if (1900..=2100).contains(&tail) {
            return false;
        }
        harvest::extract_dates(c.get(0).unwrap().as_str()).is_empty()
    })
}

/// ISO dates derivable from filesystem metadata for the presence check.
pub fn fs_metadata_dates(path: &std::path::Path) -> (Vec<String>, String) {
    let meta = std::fs::metadata(path).ok();
    let modified = meta.as_ref().and_then(|m| m.modified().ok());
    let created = meta.as_ref().and_then(|m| m.created().ok());
    metadata_date_strings(modified, created)
}

/// Split out from `fs_metadata_dates` so the calendar arithmetic is testable
/// without touching a filesystem.
fn metadata_date_strings(
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
) -> (Vec<String>, String) {
    metadata_date_strings_in(modified, created, &Local)
}

/// The zone is a parameter because the whole point of this function is that the
/// calendar day comes from the user's zone rather than UTC — and under the
/// `TZ=UTC` of a CI box the two readings are identical, so a test that reads the
/// ambient zone passes just as well against the UTC-only code this replaced.
fn metadata_date_strings_in<Tz: TimeZone>(
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    tz: &Tz,
) -> (Vec<String>, String) {
    let mut dates = Vec::new();
    // The user's calendar is the local one: a file saved at 17:00 Pacific on
    // 2026-07-20 has a UTC mtime of 2026-07-21, and the undated fallback would
    // put that wrong day straight into the filename while Explorer shows the
    // user 7/20. Both readings go into the candidate list so the presence check
    // still accepts a model that read the UTC value from document properties.
    let mut modified_iso = Utc::now()
        .with_timezone(tz)
        .date_naive()
        .format("%Y-%m-%d")
        .to_string();
    if let Some(m) = modified {
        let (local, utc) = local_and_utc_dates(m, tz);
        modified_iso = local.clone();
        dates.push(local);
        dates.push(utc);
    }
    if let Some(c) = created {
        let (local, utc) = local_and_utc_dates(c, tz);
        dates.push(local);
        dates.push(utc);
    }
    (dedup_dates(dates), modified_iso)
}

fn local_and_utc_dates<Tz: TimeZone>(t: std::time::SystemTime, tz: &Tz) -> (String, String) {
    let utc: chrono::DateTime<Utc> = t.into();
    let iso = |d: chrono::NaiveDate| d.format("%Y-%m-%d").to_string();
    (
        iso(utc.with_timezone(tz).date_naive()),
        iso(utc.date_naive()),
    )
}

/// `Vec::dedup` only collapses *consecutive* duplicates, so created-vs-modified
/// on the same day survived it whenever anything sat between them.
fn dedup_dates(mut dates: Vec<String>) -> Vec<String> {
    dates.sort();
    dates.dedup();
    dates
}

/// Strip the "The document is a …" preamble a small model habitually opens
/// its one-sentence description with, so the index reads in register style —
/// "Shareholder's register transferring 40,000 shares to John Smith." — not
/// "The document is a shareholder's register transferring 40,000 shares…".
///
/// Deliberately narrow: only a leading "The/This document/file", optionally
/// followed by "is/was" and an article, is removed. A mid-sentence mention or
/// "The documentation …" is left alone (the prefixes carry their own trailing
/// space, which is the word boundary). If what remains would fall under the
/// 15-character floor `validate_description` enforces, the original is kept —
/// a stripped-too-short description would be rejected outright, a worse
/// outcome than a wordy preamble.
pub fn strip_document_preamble(description: &str) -> String {
    let lower = description.to_lowercase();
    let Some(mut rest) = ["the document ", "this document ", "the file ", "this file "]
        .iter()
        .find_map(|p| lower.starts_with(p).then(|| &description[p.len()..]))
    else {
        return description.to_string();
    };
    // Optional linking verb, then an optional article: "is a", "was the", …
    let lower_rest = rest.to_lowercase();
    for link in ["is ", "was "] {
        if lower_rest.starts_with(link) {
            rest = &rest[link.len()..];
            let after_link = rest.to_lowercase();
            for article in ["an ", "a ", "the "] {
                if after_link.starts_with(article) {
                    rest = &rest[article.len()..];
                    break;
                }
            }
            break;
        }
    }
    let rest = rest.trim_start();
    if rest.chars().count() < 15 {
        return description.to_string();
    }
    let mut chars = rest.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => description.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harvest;

    fn harvest_with(dates: &[&str]) -> Harvest {
        let text = dates.join(" and ");
        Harvest {
            dates: harvest::extract_dates(&text),
            ..Default::default()
        }
    }

    fn ok_out() -> SlmOutput {
        SlmOutput {
            date: "2026-07-20".into(),
            date_source: "document".into(),
            subject: "Termination Notice for John Smith".into(),
            description: "Letter from Acme Corporation notifying John Smith of employment termination effective July 20, 2026.".into(),
        }
    }

    fn today_plus(days: i64) -> String {
        (Utc::now().date_naive() + Duration::days(days))
            .format("%Y-%m-%d")
            .to_string()
    }

    // ---- date rules ----------------------------------------------------

    #[test]
    fn accepts_valid_output() {
        let c = Checker::new(120);
        let h = harvest_with(&["2026-07-20"]);
        let v = c.check(&ok_out(), &h, &[], "2026-07-21", None).unwrap();
        assert_eq!(v.base_name, "2026-07-20 Termination Notice for John Smith");
        assert!(
            v.soft_flags.is_empty(),
            "unexpected flags: {:?}",
            v.soft_flags
        );
    }

    #[test]
    fn rejects_hallucinated_date() {
        let c = Checker::new(120);
        let h = harvest_with(&["2026-01-05"]);
        let e = c.check(&ok_out(), &h, &[], "2026-07-21", None).unwrap_err();
        // Was a bare `matches!(...)` expression statement: it asserted nothing.
        assert!(matches!(e, CheckError::DateNotInEvidence(_)), "got {e:?}");
        assert_eq!(e.code(), "DATE_NOT_IN_EVIDENCE");
    }

    #[test]
    fn oversized_date_fields_fail_without_echoing_the_payload() {
        let c = Checker::new(120);
        let h = harvest_with(&["2026-07-20"]);

        let mut source = ok_out();
        source.date_source = format!("UNTRUSTED_SOURCE:{}", "x".repeat(128));
        let source_error = c.check(&source, &h, &[], "2026-07-21", None).unwrap_err();
        assert_eq!(source_error.code(), "BAD_DATE_SOURCE");
        assert!(!source_error.to_string().contains("UNTRUSTED_SOURCE"));
        assert!(source_error.to_string().len() < 200);

        let mut date = ok_out();
        date.date = format!("UNTRUSTED_DATE:{}", "x".repeat(128));
        let date_error = c.check(&date, &h, &[], "2026-07-21", None).unwrap_err();
        assert_eq!(date_error.code(), "BAD_DATE");
        assert!(!date_error.to_string().contains("UNTRUSTED_DATE"));
        assert!(date_error.to_string().len() < 200);
    }

    #[test]
    fn oversized_subjects_are_rejected_before_word_trimming() {
        let c = Checker::new(120);
        let h = harvest_with(&["2026-07-20"]);
        let mut out = ok_out();
        out.subject = format!("Invoice {}", "Acme ".repeat(1_500));

        let error = c.check(&out, &h, &[], "2026-07-21", None).unwrap_err();
        assert_eq!(error.code(), "BAD_SUBJECT");
    }

    #[test]
    fn oversized_descriptions_are_rejected_before_sentence_trimming() {
        let c = Checker::new(120);
        let h = harvest_with(&["2026-07-20"]);
        let mut out = ok_out();
        out.description = format!(
            "Notice from Acme Corporation about the filing. {}{}",
            "UNTRUSTED_DESCRIPTION ",
            "tail ".repeat(1_500)
        );

        let error = c.check(&out, &h, &[], "2026-07-21", None).unwrap_err();
        assert_eq!(error.code(), "BAD_DESCRIPTION");
    }

    /// The boundary the suite was missing: an unevidenced proposal on a document
    /// that has **no date evidence at all** takes the mtime fallback instead of
    /// quarantining, while the same proposal against *any* real evidence stays a
    /// hard rejection.
    ///
    /// This is the whole fix. README promises undated documents fall back to the
    /// file modified date, but the fallback used to require the model to say
    /// `"none"`; on date-dense tax pages it proposes a date instead and the
    /// document ended as `SLM_FAIL` in quarantine. Measured before this: 0 of 3
    /// genuinely undated documents were named.
    #[test]
    fn an_unevidenced_date_falls_back_only_when_there_is_no_evidence_at_all() {
        let c = Checker::new(120);

        // No harvested dates, no metadata dates: nothing to check against, so the
        // proposal is discarded and the file's own mtime names the document.
        let v = c
            .check(&ok_out(), &Harvest::default(), &[], "2026-07-21", None)
            .expect("a dateless document must be nameable");
        assert_eq!(v.date_iso, "2026-07-21");
        assert_eq!(v.date_source, "metadata");
        assert!(
            v.soft_flags.contains(&"DATE_FROM_FILE_MTIME".to_string()),
            "the mtime provenance flag is load-bearing: {:?}",
            v.soft_flags
        );
        assert!(
            v.soft_flags
                .contains(&"DATE_PROPOSAL_DISCARDED:2026-07-20".to_string()),
            "what the model proposed must stay visible: {:?}",
            v.soft_flags
        );

        // One real date in the body is evidence. A proposal that does not match
        // it is a hallucination and must still be refused — not quietly replaced
        // by the mtime, which would ship a wrong date instead of flagging it.
        let e = c
            .check(
                &ok_out(),
                &harvest_with(&["2026-01-05"]),
                &[],
                "2026-07-21",
                None,
            )
            .unwrap_err();
        assert_eq!(e.code(), "DATE_NOT_IN_EVIDENCE");

        // Metadata dates do **not** block the fallback, and must not: in
        // production `pipeline.rs` always puts the file's own mtime and ctime in
        // this list, so requiring it to be empty made the fallback unreachable —
        // which was the original bug. The model's date is still not shipped; it
        // is discarded in favour of the mtime.
        let v = c
            .check(
                &ok_out(),
                &Harvest::default(),
                &["2026-01-05".to_string()],
                "2026-07-21",
                None,
            )
            .expect("filesystem timestamps must not veto the fallback they provide");
        assert_eq!(v.date_iso, "2026-07-21");
        assert!(v
            .soft_flags
            .contains(&"DATE_PROPOSAL_DISCARDED:2026-07-20".to_string()));

        // But a proposal that *matches* a metadata date is evidenced, so it is
        // kept as the document's date rather than discarded.
        let mut matches_meta = ok_out();
        matches_meta.date = "2026-01-05".into();
        let v = c
            .check(
                &matches_meta,
                &Harvest::default(),
                &["2026-01-05".to_string()],
                "2026-07-21",
                None,
            )
            .expect("a metadata-evidenced date is valid");
        assert_eq!(v.date_iso, "2026-01-05");
        assert_eq!(v.date_source, "metadata");
        assert!(
            !v.soft_flags
                .iter()
                .any(|f| f.starts_with("DATE_PROPOSAL_DISCARDED")),
            "an evidenced date must not be discarded: {:?}",
            v.soft_flags
        );
    }

    /// A human's date is trusted by design (`check_human` injects it as
    /// metadata), so the discard path must never apply to one — a person who
    /// types a date the document does not contain is exercising judgment, and
    /// silently replacing it with the file's mtime would overrule them.
    #[test]
    fn a_human_date_is_never_discarded_for_lack_of_evidence() {
        let c = Checker::new(120);
        let mut out = ok_out();
        out.date = "2019-03-04".into();
        let v = c
            .check_human(
                &out,
                &Harvest::default(),
                &["2019-03-04".to_string()],
                "2026-07-21",
                None,
            )
            .expect("a human override must be honored");
        assert_eq!(v.date_iso, "2019-03-04");
        assert!(
            !v.soft_flags
                .iter()
                .any(|f| f.starts_with("DATE_PROPOSAL_DISCARDED")),
            "a human date must not be recorded as discarded: {:?}",
            v.soft_flags
        );
        assert!(
            !v.soft_flags.contains(&"DATE_FROM_FILE_MTIME".to_string()),
            "a human date must not be replaced by the mtime: {:?}",
            v.soft_flags
        );
    }

    /// The fallback shares `range_check` with every other path, so a corrupted
    /// or absurd mtime cannot become a filename. Previously only reachable via a
    /// literal `"none"`; now that a discarded proposal reaches it too, the
    /// combination is worth pinning.
    #[test]
    fn a_discarded_proposal_still_validates_the_mtime_it_falls_back_to() {
        let c = Checker::new(120);
        for bad in ["", "not-a-date", "2026-13-45", "20260721"] {
            let e = c
                .check(&ok_out(), &Harvest::default(), &[], bad, None)
                .unwrap_err();
            assert_eq!(e.code(), "BAD_DATE", "for mtime {bad:?}");
        }
    }

    #[test]
    fn date_range_boundaries() {
        let c = Checker::new(120);
        // (date, expected error code or None for accept)
        let cases: Vec<(String, Option<&str>)> = vec![
            ("1799-12-31".into(), Some("DATE_OUT_OF_RANGE")),
            ("1800-01-01".into(), None),
            ("2026-07-20".into(), None),
            (today_plus(60), None),  // routine forward-dated lease
            (today_plus(399), None), // still inside the widened ceiling
            (today_plus(401), Some("DATE_OUT_OF_RANGE")),
            ("2026-02-30".into(), Some("BAD_DATE")),
            ("July 20 2026".into(), Some("BAD_DATE")),
        ];
        for (date, want) in cases {
            let mut o = ok_out();
            o.date = date.clone();
            // Feed the same date as metadata so only the range rule is in play.
            let r = c.check(
                &o,
                &Harvest::default(),
                std::slice::from_ref(&date),
                "2026-07-21",
                None,
            );
            match want {
                None => assert!(r.is_ok(), "{date} should be accepted, got {:?}", r.err()),
                Some(code) => assert_eq!(r.unwrap_err().code(), code, "for {date}"),
            }
        }
    }

    #[test]
    fn forward_dated_beyond_thirty_days_is_soft_flagged() {
        let c = Checker::new(120);
        let mut o = ok_out();
        o.date = today_plus(60);
        o.date_source = "metadata".into();
        let v = c
            .check(
                &o,
                &Harvest::default(),
                &[o.date.clone()],
                "2026-07-21",
                None,
            )
            .unwrap();
        assert!(
            v.soft_flags.iter().any(|f| f == "DATE_IN_FUTURE"),
            "{:?}",
            v.soft_flags
        );
    }

    #[test]
    fn human_date_beyond_the_ceiling_is_accepted() {
        // The review pane must never be a dead end: a human who knows the date
        // is authoritative for the range, and still gets the soft flag.
        let c = Checker::new(120);
        let mut o = ok_out();
        o.date = today_plus(900);
        o.date_source = "metadata".into();
        assert!(c
            .check(
                &o,
                &Harvest::default(),
                &[o.date.clone()],
                "2026-07-21",
                None
            )
            .is_err());
        let v = c
            .check_human(
                &o,
                &Harvest::default(),
                &[o.date.clone()],
                "2026-07-21",
                None,
            )
            .unwrap();
        assert!(v.soft_flags.iter().any(|f| f == "DATE_IN_FUTURE"));
    }

    #[test]
    fn metadata_evidence_path_corrects_the_source() {
        // Every pre-existing test passed &[] here, so this whole branch was
        // dead in the suite.
        let c = Checker::new(120);
        let mut o = ok_out();
        o.date_source = "document".into();
        let v = c
            .check(
                &o,
                &Harvest::default(),
                &["2026-07-20".into()],
                "2026-07-21",
                None,
            )
            .unwrap();
        assert_eq!(v.date_source, "metadata");
        assert!(
            v.soft_flags
                .contains(&"DATE_SOURCE_CORRECTED:document->metadata".to_string()),
            "{:?}",
            v.soft_flags
        );
    }

    #[test]
    fn document_evidence_corrects_a_metadata_claim() {
        let c = Checker::new(120);
        let mut o = ok_out();
        o.date_source = "metadata".into();
        let v = c
            .check(&o, &harvest_with(&["2026-07-20"]), &[], "2026-07-21", None)
            .unwrap();
        assert_eq!(v.date_source, "document");
        assert!(v
            .soft_flags
            .contains(&"DATE_SOURCE_CORRECTED:metadata->document".to_string()));
    }

    #[test]
    fn a_real_date_survives_a_wrong_source_token() {
        // The old `out.date == "none" || out.date_source == "none"` threw away
        // a correct, evidence-backed date over one wrong enum token and filed
        // the document under the day it happened to be processed.
        let c = Checker::new(120);
        let mut o = ok_out();
        o.date_source = "none".into();
        let v = c
            .check(&o, &harvest_with(&["2026-07-20"]), &[], "2026-07-21", None)
            .unwrap();
        assert_eq!(v.date_iso, "2026-07-20");
        assert_eq!(v.date_source, "document");
    }

    #[test]
    fn bad_date_source_token_is_rejected() {
        let c = Checker::new(120);
        let mut o = ok_out();
        o.date_source = "guess".into();
        let e = c
            .check(&o, &harvest_with(&["2026-07-20"]), &[], "2026-07-21", None)
            .unwrap_err();
        assert_eq!(e.code(), "BAD_DATE_SOURCE");
    }

    #[test]
    fn undated_falls_back_to_metadata() {
        let c = Checker::new(120);
        let mut o = ok_out();
        o.date = "none".into();
        o.date_source = "none".into();
        let v = c
            .check(&o, &Harvest::default(), &[], "2026-07-21", None)
            .unwrap();
        assert_eq!(v.date_iso, "2026-07-21");
        assert_eq!(v.date_source, "metadata");
        assert!(v.soft_flags.contains(&"DATE_FROM_FILE_MTIME".to_string()));
    }

    #[test]
    fn undated_fallback_validates_the_mtime_string() {
        // The one branch where a string used to reach a filename with no parse,
        // no range bound and no sanitization at all.
        let c = Checker::new(120);
        let mut o = ok_out();
        o.date = "none".into();
        o.date_source = "none".into();
        for bad in ["", "not-a-date", "2026-13-45", "20260721"] {
            let e = c
                .check(&o, &Harvest::default(), &[], bad, None)
                .unwrap_err();
            assert_eq!(e.code(), "BAD_DATE", "for {bad:?}");
        }
    }

    #[test]
    fn ambiguous_only_evidence_is_flagged() {
        let c = Checker::new(120);
        let mut o = ok_out();
        // 03/04/2026: US reading 2026-03-04, day-first alternate 2026-04-03.
        o.date = "2026-04-03".into();
        o.description = "Letter from Acme Corporation about the April schedule change.".into();
        let h = Harvest {
            dates: harvest::extract_dates("effective 03/04/2026"),
            ..Default::default()
        };
        let v = c.check(&o, &h, &[], "2026-07-21", None).unwrap();
        assert!(
            v.soft_flags.contains(&"DATE_AMBIGUOUS_FORMAT".to_string()),
            "{:?}",
            v.soft_flags
        );

        o.date = "2026-03-04".into();
        let v = c.check(&o, &h, &[], "2026-07-21", None).unwrap();
        assert!(!v.soft_flags.contains(&"DATE_AMBIGUOUS_FORMAT".to_string()));
    }

    #[test]
    fn a_confident_tail_duplicate_prevents_an_ambiguous_date_flag() {
        let c = Checker::new(120);
        let markdown = format!(
            "effective 04/03/2026\n{}March 4, 2026",
            "filler ".repeat(1_100)
        );
        let h = harvest::harvest(&markdown);
        let mut out = ok_out();
        out.date = "2026-03-04".into();

        let v = c.check(&out, &h, &[], "2026-07-21", None).unwrap();
        assert!(
            !v.soft_flags.contains(&"DATE_AMBIGUOUS_FORMAT".to_string()),
            "the unambiguous tail evidence must clear the ambiguity flag: {:?}",
            v.soft_flags
        );
    }

    #[test]
    fn a_date_only_in_the_body_is_flagged() {
        let c = Checker::new(120);
        let text = format!(
            "{} the date 2026-07-20 appears here",
            "filler word ".repeat(200)
        );
        let h = Harvest {
            dates: harvest::extract_dates(&text),
            ..Default::default()
        };
        let v = c.check(&ok_out(), &h, &[], "2026-07-21", None).unwrap();
        assert!(
            v.soft_flags.contains(&"DATE_FROM_BODY".to_string()),
            "{:?}",
            v.soft_flags
        );
    }

    #[test]
    fn span_mismatch_is_soft() {
        let c = Checker::new(120);
        let h = harvest_with(&["2026-07-20", "2026-06-01"]);
        let v = c
            .check(&ok_out(), &h, &[], "2026-07-21", Some("2026-06-01"))
            .unwrap();
        assert!(v.soft_flags.iter().any(|f| f.starts_with("SPAN_MISMATCH")));
    }

    // ---- subject rules --------------------------------------------------

    #[test]
    fn subject_table() {
        let c = Checker::new(160);
        // (raw subject, Some(expected output) for accept | None for reject)
        let cases: &[(&str, Option<&str>)] = &[
            (
                "Invoice #4411: Acme/Q3 <final>",
                Some("Invoice 4411 Acme Q3 final"),
            ),
            // C1: scanner defaults. Every one of these returned Ok before.
            ("Scanned Document 001", None),
            ("New Microsoft Word Document", None),
            ("Untitled Document 1", None),
            ("Document from Scanner 3", None),
            ("Scanned Document", None),
            ("Document", None),
            ("Copy of Document (2)", None),
            ("### %%% ///", None),
            // …but a real title that merely contains a generic word is fine.
            ("Letter of Intent Acme", Some("Letter of Intent Acme")),
            (
                "Scanner Maintenance Agreement",
                Some("Scanner Maintenance Agreement"),
            ),
            // C4: two-word document types are the commonest office convention.
            ("Lease Agreement", Some("Lease Agreement")),
            ("Employment Agreement", Some("Employment Agreement")),
            ("Termination", None), // 1 word
            (
                "Notice of Proposed Rulemaking on Wage and Hour Compliance",
                Some("Notice of Proposed Rulemaking on Wage and Hour Compliance"),
            ), // 9
            (
                "Notice of Proposed Rulemaking on Wage and Hour Compliance Rules",
                Some("Notice of Proposed Rulemaking on Wage and Hour Compliance Rules"),
            ), // 10
            // 11 words: trimmed to the first 10 rather than refused. The
            // guarantee is the ceiling, not that the model hits it exactly.
            (
                "Notice of Proposed Rulemaking on Wage and Hour Compliance Rules Today",
                Some("Notice of Proposed Rulemaking on Wage and Hour Compliance Rules"),
            ),
            // C14: unspaced scripts can never reach 2 whitespace words, so they
            // are judged by character count instead.
            ("終止僱傭通知書", Some("終止僱傭通知書")),
            ("解雇", None), // 2 chars, under the character floor
            ("2026年7月20日 解雇通知", Some("2026年7月20日 解雇通知")),
            // …but that switch is by proportion, not presence. One Japanese
            // company name inside an English title is still an English title,
            // and was being rejected at "53 characters (need 4-40)".
            (
                "Invoice from 株式会社 Acme Trading Company Limited Group Holdings",
                Some("Invoice from 株式会社 Acme Trading Company Limited Group Holdings"),
            ),
            // Mostly-Latin must not escape the word count via one CJK glyph —
            // it is judged as English and trimmed to ten words like any other.
            (
                "Alpha Beta Gamma Delta Epsilon Zeta Eta Theta Iota Kappa 株",
                Some("Alpha Beta Gamma Delta Epsilon Zeta Eta Theta Iota Kappa"),
            ),
        ];
        for (raw, want) in cases {
            let got = c.sanitize_subject(raw);
            match want {
                Some(expect) => assert_eq!(got.as_deref().ok(), Some(*expect), "for {raw:?}"),
                None => assert!(got.is_err(), "{raw:?} should be rejected, got {got:?}"),
            }
        }
    }

    /// One word is unrepairable and stays a rejection. Too many is an overshoot
    /// the leading words survive, so it is truncated rather than quarantined —
    /// the guarantee is that no filename carries more than `SUBJECT_MAX_WORDS`,
    /// not that the model must hit the ceiling exactly.
    #[test]
    fn word_count_boundaries() {
        let c = Checker::new(200);
        for n in 1..=14usize {
            let subject = vec!["Alpha"; n].join(" ");
            let got = c.sanitize_subject(&subject);
            if n < 2 {
                assert!(got.is_err(), "{n} words must be rejected: {subject}");
                continue;
            }
            let kept = got.unwrap_or_else(|e| panic!("{n} words should be usable: {e}"));
            let words = subject_words(&kept).len();
            assert!(
                words <= 10,
                "{n} words must be trimmed to at most 10, got {words}: {kept:?}"
            );
            if n <= 10 {
                assert_eq!(kept, subject, "{n} words must pass through unchanged");
            } else {
                assert_eq!(words, 10, "an over-long subject keeps the first 10 words");
            }
        }
    }

    /// A date printed on the page beats the file's embedded `created` property.
    ///
    /// This is the case that made most filenames wrong: handed both a dated page
    /// and a `created` property, the model takes the property — measured, 14 of 16
    /// documents with a date on page one were named from metadata instead — and
    /// the file ends up stamped with the day it was *made* rather than the day it
    /// is *about*.
    #[test]
    fn a_date_printed_on_the_page_outranks_embedded_metadata() {
        let c = Checker::new(200);
        let mut out = ok_out();
        // What the model proposed: the file's own creation date, which is real
        // metadata evidence — just not the document's date.
        out.date = "2026-07-29".into();
        out.date_source = "document".into();

        let v = c
            .check(
                &out,
                &harvest_with(&["2021-01-20"]),
                &["2026-07-29".to_string()],
                "2026-07-29",
                None,
            )
            .expect("a document with a date on the page must be nameable");
        assert_eq!(
            v.date_iso, "2021-01-20",
            "the date on the page must win over the created property"
        );
        assert_eq!(v.date_source, "document");
        assert!(
            v.soft_flags
                .contains(&"DATE_PREFERRED_FROM_DOCUMENT:2026-07-29".to_string()),
            "the displaced proposal must be recorded: {:?}",
            v.soft_flags
        );

        // With no date on the page there is nothing to prefer, so the metadata
        // path is unchanged — this is what `metadata_evidence_path_corrects_the_source`
        // covers and it must keep working.
        let v = c
            .check(
                &out,
                &Harvest::default(),
                &["2026-07-29".to_string()],
                "2026-07-29",
                None,
            )
            .expect("metadata-only evidence is still valid");
        assert_eq!(v.date_iso, "2026-07-29");
        assert_eq!(v.date_source, "metadata");

        // A human's date is never displaced.
        let v = c
            .check_human(
                &out,
                &harvest_with(&["2021-01-20"]),
                &["2026-07-29".to_string()],
                "2026-07-29",
                None,
            )
            .expect("a human override must be honored");
        assert_eq!(
            v.date_iso, "2026-07-29",
            "a person who typed a date meant it"
        );
    }

    /// The preference is deliberately narrow: only unambiguous dates, and only
    /// from the head region where a letterhead or date line lives. A date deep in
    /// the body is more likely a reference to another document, and an ambiguous
    /// `04/05/2023` is a coin flip that should not win this way.
    #[test]
    fn the_document_date_preference_ignores_ambiguous_and_deep_dates() {
        // Deep in the body: past the head region, so not preferred.
        let deep = Harvest {
            dates: vec![harvest::FoundDate {
                iso: "2021-01-20".into(),
                raw: "January 20, 2021".into(),
                offset: HEAD_REGION_BYTES + 1,
                ambiguous: false,
            }],
            ..Default::default()
        };
        assert!(Checker::date_printed_on_the_page(&deep).is_none());

        // Ambiguous slash form in the head region: also not preferred.
        let ambiguous = Harvest {
            dates: vec![harvest::FoundDate {
                iso: "2023-04-05".into(),
                raw: "04/05/2023".into(),
                offset: 10,
                ambiguous: true,
            }],
            ..Default::default()
        };
        assert!(Checker::date_printed_on_the_page(&ambiguous).is_none());

        // The earliest qualifying date wins — the letterhead, not a later mention.
        let two = Harvest {
            dates: vec![
                harvest::FoundDate {
                    iso: "2021-03-03".into(),
                    raw: "3 March 2021".into(),
                    offset: 900,
                    ambiguous: false,
                },
                harvest::FoundDate {
                    iso: "2021-01-20".into(),
                    raw: "January 20, 2021".into(),
                    offset: 40,
                    ambiguous: false,
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            Checker::date_printed_on_the_page(&two).map(|f| f.iso.as_str()),
            Some("2021-01-20")
        );
    }

    /// The length budget must cover the widest suffix the ledger can actually
    /// append, and nothing else connects the two numbers.
    ///
    /// Before this existed, the reserve was a bare `12` with a comment saying
    /// `" (99)"` while `reserve_name` could already emit `" (500)"` — six
    /// characters, not five. It fit by one character, by coincidence of the
    /// constants rather than by anything checking. Raising the collision cap
    /// without widening the reserve would have produced names past
    /// `max_filename_len` silently, because the length gate runs on the base name
    /// before any suffix exists and `reserve_name` never re-checks length.
    #[test]
    fn the_filename_reserve_covers_the_widest_collision_suffix() {
        let widest_suffix = format!(" ({MAX_NAME_COLLISIONS})").chars().count();
        // The longest extension RE_TRAILING_EXT recognises.
        let longest_ext = "docx".len();
        assert!(
            FILENAME_TAIL_RESERVE >= widest_suffix + ".".len() + longest_ext,
            "reserve {FILENAME_TAIL_RESERVE} cannot hold ' ({MAX_NAME_COLLISIONS})' \
             ({widest_suffix}) + '.' + '{longest_ext}-char extension'"
        );
    }

    /// The guarantee the description rule makes is that what ships is exactly one
    /// sentence — not that the model must produce exactly one unaided. Where a
    /// complete first sentence exists, keeping it satisfies the guarantee and
    /// saves the document; where none exists there is nothing to keep, so it
    /// stays a rejection. This pins both halves and the flag that records it.
    #[test]
    fn a_multi_sentence_description_is_trimmed_and_flagged() {
        let c = Checker::new(200);
        let h = harvest_with(&["2026-07-20"]);
        let mut out = ok_out();
        out.description = "This document contains the payroll worksheets. The return was ".into();
        let v = c
            .check(&out, &h, &[], "2026-07-21", None)
            .expect("a trailing fragment must not quarantine the document");
        // The "This document" preamble is stripped first, then the trailing
        // fragment is trimmed to the one complete sentence.
        assert_eq!(v.description, "Contains the payroll worksheets.");
        assert!(
            v.soft_flags
                .contains(&"DESCRIPTION_TRIMMED_TO_ONE_SENTENCE".to_string()),
            "the trim must be recorded: {:?}",
            v.soft_flags
        );

        // Nothing to keep: no complete sentence anywhere, so still refused.
        let mut out = ok_out();
        out.description = "there is no terminal punctuation anywhere in this one".into();
        assert_eq!(
            c.check(&out, &h, &[], "2026-07-21", None)
                .unwrap_err()
                .code(),
            "BAD_DESCRIPTION"
        );

        // A first sentence too short to stand alone is not a repair either.
        let mut out = ok_out();
        out.description = "Ok. Then a much longer trailing fragment that got cut".into();
        assert_eq!(
            c.check(&out, &h, &[], "2026-07-21", None)
                .unwrap_err()
                .code(),
            "BAD_DESCRIPTION",
            "trimming must not produce a description under the 15-char floor"
        );
    }

    /// The truncation is recorded, so a shortened name is auditable rather than
    /// quietly different from what the model proposed.
    #[test]
    fn an_over_long_subject_is_truncated_and_flagged() {
        let c = Checker::new(200);
        let mut out = ok_out();
        out.subject =
            "Wage and Tax Statement, Form W-2, Internal Revenue Service, Elena Hutchins, 2023"
                .into();
        let v = c
            .check(
                &out,
                &harvest_with(&["2026-07-20"]),
                &[],
                "2026-07-21",
                None,
            )
            .expect("an over-long subject must not quarantine the document");
        assert!(
            v.soft_flags.contains(&"SUBJECT_TRUNCATED".to_string()),
            "truncation must be recorded: {:?}",
            v.soft_flags
        );
        assert!(subject_words(&v.subject).len() <= 10, "got {:?}", v.subject);
        // The informative head survives, and no dangling separator with it.
        assert!(
            v.subject.starts_with("Wage and Tax Statement"),
            "{:?}",
            v.subject
        );
        assert!(
            !v.subject.ends_with(',') && !v.subject.ends_with('/'),
            "a cut must not leave a dangling separator: {:?}",
            v.subject
        );
    }

    /// The JSON schema's `maxLength` stops generation at a character count, so a
    /// subject can arrive already ending in a separator whose right-hand side was
    /// never emitted. Measured on the 0.4.2 run, one document shipped as
    /// `"Tax Return - Supplemental Income and Loss (Rental Real Estate) -"`.
    /// The word trim cleans its own cut; this covers the one it did not make.
    #[test]
    fn a_subject_that_arrives_ending_in_a_separator_is_tidied() {
        let c = Checker::new(200);
        for raw in [
            "Tax Return - Supplemental Income and Loss (Rental Real Estate) -",
            "Form 1099-MISC - Kessler & Sons Contracting -",
            "Schedule E for Patrick Chen &",
            "Form 4562 Depreciation,",
        ] {
            let mut out = ok_out();
            out.subject = raw.into();
            let v = c
                .check(
                    &out,
                    &harvest_with(&["2026-07-20"]),
                    &[],
                    "2026-07-21",
                    None,
                )
                .unwrap_or_else(|e| panic!("{raw:?} must still be nameable, got {e:?}"));
            let last = v.subject.chars().last().expect("non-empty");
            assert!(
                last.is_alphanumeric() || last == ')',
                "subject must not end pointing at nothing: {:?} (from {raw:?})",
                v.subject
            );
            // A tidy-up drops punctuation, never a word, so it stays unflagged.
            assert!(
                !v.soft_flags.contains(&"SUBJECT_TRUNCATED".to_string()),
                "no words were dropped, so nothing to flag: {:?}",
                v.soft_flags
            );
        }
    }

    /// A person who types a trailing separator in the review pane gets it back
    /// verbatim; the tidy-up is a repair for model output only.
    #[test]
    fn a_human_subject_is_not_tidied() {
        let c = Checker::new(200);
        let mut out = ok_out();
        out.subject = "Estate of A. Whitfield -".into();
        let v = c
            .check_human(
                &out,
                &harvest_with(&["2026-07-20"]),
                &[],
                "2026-07-21",
                None,
            )
            .expect("a human subject is honored");
        assert_eq!(v.subject, "Estate of A. Whitfield -");
    }

    /// The schema cap and the filename budget are one decision: a subject at the
    /// cap must still compose without tripping `TooLong`. `slm.rs` sets 95 from
    /// this arithmetic, so if either side moves, this fails rather than producing
    /// documents that quarantine on length.
    #[test]
    fn the_schema_subject_cap_still_composes_at_the_filename_budget() {
        const SCHEMA_SUBJECT_MAX: usize = 95; // slm.rs naming_schema()
        const DATE_PREFIX: usize = 11; // "YYYY-MM-DD "
        let c = Checker::new(120); // config.rs default max_filename_len
        assert!(
            DATE_PREFIX + SCHEMA_SUBJECT_MAX + FILENAME_TAIL_RESERVE <= c.max_filename_len,
            "a subject at the schema cap ({SCHEMA_SUBJECT_MAX}) plus the date prefix and \
             the collision reserve ({FILENAME_TAIL_RESERVE}) must fit in {}",
            c.max_filename_len
        );
    }

    #[test]
    fn joined_titles_are_counted_by_their_words() {
        let c = Checker::new(160);
        // Counted as one word before, so it was rejected outright.
        let s = c
            .sanitize_subject("2026-07-20_Termination_Notice_Smith")
            .unwrap();
        assert_eq!(s, "Termination_Notice_Smith");
    }

    #[test]
    fn subject_never_carries_zero_width_or_bidi_characters() {
        let c = Checker::new(160);
        // U+202E turns "…fdp.exe" into "…exe.pdf" in Explorer and SharePoint.
        let s = c
            .sanitize_subject("Notice of Termination \u{202e}fdp.exe")
            .unwrap();
        assert!(!s.contains('\u{202e}'), "bidi override survived: {s:?}");
        for raw in [
            "Termination\u{200b} Notice Smith",
            "Termination\u{00a0}Notice Smith",
            "Termination\u{feff} Notice Smith",
            "Termination\u{2066} Notice\u{2069} Smith",
            "Termination \u{200e}Notice Smith",
        ] {
            let s = c
                .sanitize_subject(raw)
                .unwrap_or_else(|e| panic!("{raw:?}: {e}"));
            for bad in [
                '\u{200b}', '\u{200e}', '\u{202e}', '\u{2066}', '\u{2069}', '\u{feff}', '\u{00a0}',
            ] {
                assert!(!s.contains(bad), "{bad:?} survived {raw:?} -> {s:?}");
            }
            assert!(!s.contains("  "), "double space in {s:?}");
        }
    }

    #[test]
    fn nfc_normalizes_decomposed_input() {
        let c = Checker::new(160);
        // "Reveé" written with a combining acute.
        let s = c
            .sanitize_subject("Notice for Reve\u{0301}e Holdings")
            .unwrap();
        assert_eq!(s, "Notice for Revée Holdings");
        assert!(!s.contains('\u{0301}'));
    }

    #[test]
    fn subject_date_and_extension_echoes_are_stripped() {
        let c = Checker::new(160);
        let (s, flags) = c
            .sanitize_subject_inner("2026-07-20 Termination Notice", Source::Model)
            .unwrap();
        assert_eq!(s, "Termination Notice");
        assert!(flags.contains(&"SUBJECT_DATE_STRIPPED".to_string()));

        let (s, flags) = c
            .sanitize_subject_inner("Invoice 2024 Q3.pdf", Source::Model)
            .unwrap();
        assert_eq!(s, "Invoice 2024 Q3");
        assert!(flags.contains(&"SUBJECT_EXT_STRIPPED".to_string()));

        // A subject that is nothing but a date has no name left to give.
        assert!(c.sanitize_subject("2026-07-20").is_err());

        // Sub-clause numbering is not a date, so nothing may be stripped and
        // no flag may claim it was. "1.2.34 Rent Schedule" used to come back as
        // "Rent Schedule" with an untrue SUBJECT_DATE_STRIPPED.
        for raw in ["1.2.34 Rent Schedule", "10.1.20 Notices and Consents"] {
            let (s, flags) = c.sanitize_subject_inner(raw, Source::Model).unwrap();
            assert_eq!(s, raw, "nothing should have been stripped from {raw:?}");
            assert!(
                !flags.contains(&"SUBJECT_DATE_STRIPPED".to_string()),
                "{flags:?}"
            );
        }
    }

    #[test]
    fn composed_name_contains_exactly_one_date() {
        let c = Checker::new(120);
        let mut o = ok_out();
        o.subject = "2026-07-20 Termination Notice".into();
        let v = c
            .check(&o, &harvest_with(&["2026-07-20"]), &[], "2026-07-21", None)
            .unwrap();
        assert_eq!(v.base_name, "2026-07-20 Termination Notice");
        assert_eq!(
            harvest::extract_dates(&v.base_name)
                .iter()
                .filter(|d| !d.ambiguous)
                .count(),
            1
        );
    }

    #[test]
    fn human_subject_skips_the_word_count_but_not_the_safety_rules() {
        let c = Checker::new(160);
        assert!(c.sanitize_subject("Termination").is_err());
        assert_eq!(
            c.sanitize_subject_human("Termination").unwrap(),
            "Termination"
        );
        // Illegal characters and PII are still enforced for a human.
        assert_eq!(
            c.sanitize_subject_human("Q3/Q4 Plan").unwrap(),
            "Q3 Q4 Plan"
        );
        assert!(c.sanitize_subject_human("W2 for 123-45-6789").is_err());
        assert!(c.sanitize_subject_human("").is_err());
    }

    #[test]
    fn subject_grounding_is_a_soft_flag_only() {
        let c = Checker::new(160);
        let mut h = harvest_with(&["2026-07-20"]);
        h.headings = vec!["TERMINATION OF EMPLOYMENT - JOHN SMITH".into()];
        h.head_excerpt = "Dear Mr. Smith,\nThis letter confirms your termination.".into();

        // Grounded: "smith" appears in the evidence.
        let v = c.check(&ok_out(), &h, &[], "2026-07-21", None).unwrap();
        assert!(
            !v.soft_flags.contains(&"SUBJECT_UNGROUNDED".to_string()),
            "{:?}",
            v.soft_flags
        );

        // Ungrounded: not one content token is anywhere in the document.
        let mut o = ok_out();
        o.subject = "Quarterly Bonus Schedule Alvarez".into();
        o.description = "Letter from Acme Corporation about the quarterly bonus schedule.".into();
        let v = c.check(&o, &h, &[], "2026-07-21", None).unwrap();
        assert!(
            v.soft_flags.contains(&"SUBJECT_UNGROUNDED".to_string()),
            "{:?}",
            v.soft_flags
        );

        // The failure the flag exists for: right document type, invented name.
        // The first cut accepted on any single token, so "Termination" alone
        // grounded both of these and the flag never fired.
        for invented in [
            "Termination Notice for Jane Doe",
            "Termination Notice for Maria Alvarez",
        ] {
            assert_eq!(
                subject_grounded(invented, &h),
                Some(false),
                "{invented:?} must not count as grounded"
            );
        }
        assert_eq!(
            subject_grounded("Termination Notice for John Smith", &h),
            Some(true)
        );

        // Whole tokens, not substrings: "law" must not ground on "unlawful".
        let lawful = Harvest {
            headings: vec!["UNLAWFUL DETAINER PROCEEDINGS".into()],
            ..Default::default()
        };
        assert_eq!(
            subject_grounded("Law Society Bulletin", &lawful),
            Some(false)
        );

        // With no evidence text at all the question is unanswerable, so silent.
        assert_eq!(
            subject_grounded("Quarterly Bonus Schedule", &Harvest::default()),
            None
        );
        // A subject made only of doc-type words grounds nothing either way.
        assert_eq!(subject_grounded("Letter Agreement", &h), None);
    }

    // ---- PII rules ------------------------------------------------------

    #[test]
    fn identifier_patterns_table() {
        let c = Checker::new(200);
        // (text, is it really an identifier?)
        let cases: &[(&str, bool)] = &[
            ("W2 for 123-45-6789 John Smith", true),
            ("Card 4111 1111 1111 1111 on file", true),
            ("Card 4111-1111-1111-1111 on file", true),
            ("Card 4111111111111111 on file", true),
            ("Amex 378282246310005 on file", true), // 15 digits, prefix 37
            ("Diners 36227206271667 on file", true), // 14 digits, prefix 36
            ("Card 5555555555554444 on file", true), // Mastercard 51-55
            ("Card 2223003122003222 on file", true), // Mastercard 2-series
            ("Card 6011111111111117 on file", true), // Discover
            // A real PAN with a real issuer prefix at the head of a 20-digit
            // run: the old trailing \b meant this was not examined at all.
            ("Ref 41111111111111119999 attached", true),
            // C6 false positives, all verified against the old rule.
            ("Invoice covering 2026-07-20 2026-07-21 period", false),
            ("Meter readings 1234567 8901234 recorded", false),
            ("ISBN 978-0-13-235088-4 copy", false),
            ("Timesheet 2026-07-20 through 2026-07-26", false),
            ("Docket 555-12-2026 hearing", false),
            // Luhn-valid (all zeros sum to zero) but no issuer starts with 0.
            ("Meter 000000000000000000 unit", false),
            // Same 16 digits, one digit off the checksum.
            ("Card 4111111111111112 on file", false),
        ];
        for (text, is_pii) in cases {
            assert_eq!(
                contains_ssn(text) || contains_card_number(text),
                *is_pii,
                "for {text:?}"
            );
            assert_eq!(
                c.sanitize_subject_human(text).is_err(),
                *is_pii,
                "subject for {text:?}"
            );
        }
    }

    #[test]
    fn luhn_is_actually_checked() {
        assert!(luhn_ok(&[4, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1]));
        assert!(!luhn_ok(&[4, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2]));
    }

    #[test]
    fn long_reference_numbers_are_not_card_numbers() {
        // The bulk of the false-positive risk: 18-22 digit runs, the shape of
        // every tracking, remittance and meter reference this corpus is full
        // of. Half of these begin with a live issuer prefix, so the checksum
        // and the length rule are both doing work here.
        let cases: &[&str] = &[
            "Tracking 12345678901234567890 received",
            "Meter 000000000000000000 unit",
            "USPS 9405511899223197428490 delivered",
            "Remittance 20260720000123456789 posted",
            "FedEx 612345678901234567 in transit",
            "Batch 4001234567890123456 exported",
            "Policy 5512345678901234567 renewed",
            "Account 3412345678901234567 reconciled",
            "EDI 6511223344556677889900 acknowledged",
            "Serial 3612345678901234567890 shipped",
            "Ledger 2223000012345678901 posted",
            "Reference 987654321098765432 attached",
            "Consignment 445566778899001122 booked",
            "Roll 300123456789012345678 counted",
        ];
        for t in cases {
            assert!(
                !contains_card_number(t),
                "{t:?} is not a card number, but the rule says it is"
            );
        }
    }

    #[test]
    fn luhn_alone_is_not_a_filter() {
        // The measurement behind `issuer_lengths`. Sliding Luhn over 13..=19
        // digit windows of a 20-digit run tries 35 windows at roughly 1-in-10
        // each, so it flagged 197 of 200 random runs — "contains an identifier
        // pattern" on essentially any long reference number. Anchored to an
        // issuer prefix and an issuer length, the same corpus stays clean
        // enough that a real invoice is not sent to Needs Review.
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut rand_digit = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % 10) as u8
        };
        let (mut flagged, mut unanchored, trials) = (0usize, 0usize, 2000usize);
        for _ in 0..trials {
            let digits: Vec<u8> = (0..20).map(|_| rand_digit()).collect();
            let run: String = digits.iter().map(|d| char::from(b'0' + d)).collect();
            if contains_card_number(&format!("Ref {run} attached")) {
                flagged += 1;
            }
            if (13..=19).any(|n: usize| digits.windows(n).any(luhn_ok)) {
                unanchored += 1;
            }
        }
        assert!(
            unanchored * 100 / trials > 90,
            "the free window scan should flag nearly everything; got {unanchored}/{trials}"
        );
        assert!(
            flagged * 100 / trials < 8,
            "{flagged}/{trials} random 20-digit runs flagged as cards"
        );
    }

    // ---- description rules ----------------------------------------------

    #[test]
    fn description_table() {
        let c = Checker::new(200);
        let h = harvest_with(&["2026-07-20"]);
        // (description, accepted?)
        let cases: &[(&str, bool)] = &[
            (
                "Notice from the U.S. Department of Labor regarding wage compliance.",
                true,
            ),
            (
                "Filing by Acme Inc. to the U.S. Dept. of Labor on behalf of staff.",
                true,
            ),
            (
                "Memo announcing a 3.5% increase effective Jan. 1, 2026 for all staff.",
                true,
            ),
            (
                "Letter signed by J. Smith confirming receipt of the notice.",
                true,
            ),
            (
                "Notice covering items 1, 2 and 3 as listed... in the schedule.",
                true,
            ),
            // A description whose LAST token is an abbreviation: the mask
            // rewrote its full stop, leaving zero terminals and rejecting
            // ordinary output. Company names and street types end these
            // sentences constantly.
            ("Invoice issued to Beta Holdings Inc.", true),
            (
                "Employment offer letter addressed to Jane Doe from Acme Corp.",
                true,
            ),
            ("Purchase order for parts from Wilson Bros. Ltd.", true),
            ("Rent statement for the property at 4 Elm St.", true),
            (
                "Notice of hearing set for 10 a.m. before Judge Alvarez.",
                true,
            ),
            // Multi-part version and clause references: the decimal mask
            // consumed the digit on both sides, so in "2.1.3" only "2.1" was
            // masked and the dot in "1.3" still counted as a sentence end.
            (
                "Update to the travel policy v2.1.3 effective immediately.",
                true,
            ),
            (
                "Report on the 3.5.2 release of the payroll system for staff.",
                true,
            ),
            // Two sentences are trimmed to the first rather than refused: the
            // rule exists so a filename's description is one sentence, and
            // keeping the first one satisfies that. The dropped tail is recorded
            // as DESCRIPTION_TRIMMED_TO_ONE_SENTENCE.
            (
                "This is one sentence. This is another complete sentence.",
                true,
            ),
            // Trimmed to the first sentence, same as the two-sentence case. Note
            // the first sentence must still clear the 15-character floor on its
            // own — "First sentence." does, barely.
            ("First sentence. Second sentence. Third one here.", true),
            // No terminal anywhere means there is no complete sentence to keep,
            // so there is nothing to repair and this stays a rejection.
            ("no terminal punctuation at all here sadly", false),
            // The shape observed in production: a complete first sentence, then
            // a fragment where generation stopped. The first sentence is kept.
            (
                "Ends mid-sentence. Then trails off with no closing mark",
                true,
            ),
            ("Too short.", false),
            // 15-char floor, exactly.
            ("Acme wage note.", true),
        ];
        for (desc, accept) in cases {
            let mut o = ok_out();
            o.description = (*desc).into();
            let r = c.check(&o, &h, &[], "2026-07-21", None);
            assert_eq!(r.is_ok(), *accept, "for {desc:?} -> {:?}", r.err());
        }
    }

    #[test]
    fn terminal_set_and_regex_agree() {
        // mask_non_terminals decides what to hold back with TERMINALS while
        // validate_description counts with RE_SENTENCE_END; if they drift, a
        // description ending in one of them silently loses its full stop.
        let mut buf = [0u8; 4];
        for c in TERMINALS {
            assert!(RE_SENTENCE_END.is_match(c.encode_utf8(&mut buf)), "{c:?}");
        }
    }

    #[test]
    fn description_length_bounds() {
        let c = Checker::new(400);
        let h = harvest_with(&["2026-07-20"]);
        for (n, accept) in [(14usize, false), (15, true), (200, true), (201, false)] {
            let mut o = ok_out();
            // n chars total, ending in a period.
            o.description = format!("{}.", "a".repeat(n - 1));
            let r = c.check(&o, &h, &[], "2026-07-21", None);
            assert_eq!(r.is_ok(), accept, "{n} chars -> {:?}", r.err());
        }
    }

    #[test]
    fn description_may_not_restate_the_subject() {
        let c = Checker::new(160);
        let h = harvest_with(&["2026-07-20"]);
        let mut o = ok_out();
        o.description = "termination notice for john smith.".into();
        let e = c.check(&o, &h, &[], "2026-07-21", None).unwrap_err();
        assert_eq!(e.code(), "BAD_DESCRIPTION");
    }

    #[test]
    fn cjk_description_terminals_are_recognized() {
        let c = Checker::new(160);
        let mut o = ok_out();
        o.subject = "終止僱傭通知書".into();
        o.description = "安美公司致約翰史密斯的僱傭終止通知。".into();
        let v = c
            .check(&o, &harvest_with(&["2026-07-20"]), &[], "2026-07-21", None)
            .unwrap();
        assert_eq!(v.base_name, "2026-07-20 終止僱傭通知書");
    }

    #[test]
    fn too_long_is_reported_with_the_limit() {
        let c = Checker::new(40);
        let e = c
            .check(
                &ok_out(),
                &harvest_with(&["2026-07-20"]),
                &[],
                "2026-07-21",
                None,
            )
            .unwrap_err();
        assert_eq!(e.code(), "TOO_LONG");
    }

    // ---- filesystem metadata --------------------------------------------

    /// 2026-07-21T00:30:00Z — 17:30 on the 20th in US Pacific.
    fn late_evening_pacific() -> std::time::SystemTime {
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_784_593_800)
    }

    fn pacific() -> chrono::FixedOffset {
        chrono::FixedOffset::east_opt(-7 * 3600).unwrap()
    }

    #[test]
    fn metadata_dates_carry_both_calendar_readings() {
        // Pinned to a fixed -07:00 rather than the ambient zone: this suite runs
        // under TZ=UTC, where the local and UTC readings are identical and every
        // assertion below would hold against the Utc-only code C10 replaced.
        let (dates, modified_iso) = metadata_date_strings_in(
            Some(late_evening_pacific()),
            Some(late_evening_pacific()),
            &pacific(),
        );
        assert_eq!(
            modified_iso, "2026-07-20",
            "the filename must carry the day Explorer shows the user"
        );
        assert!(
            dates.contains(&"2026-07-20".to_string()) && dates.contains(&"2026-07-21".to_string()),
            "both readings must be candidates: {dates:?}"
        );
        assert_eq!(
            dates.len(),
            dates
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        );
    }

    #[test]
    fn the_calendar_day_comes_from_the_users_zone() {
        let (local, utc) = local_and_utc_dates(late_evening_pacific(), &pacific());
        assert_eq!(local, "2026-07-20");
        assert_eq!(utc, "2026-07-21");
        // East of Greenwich the split falls the other way.
        let (local, utc) = local_and_utc_dates(
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_784_591_400), // 23:50Z on the 20th
            &chrono::FixedOffset::east_opt(9 * 3600).unwrap(),
        );
        assert_eq!(local, "2026-07-21");
        assert_eq!(utc, "2026-07-20");
    }

    #[test]
    fn metadata_dates_dedupe_non_adjacent_duplicates() {
        // Vec::dedup only collapses neighbours, so this survived it.
        let got = dedup_dates(vec![
            "2026-07-20".into(),
            "2026-07-21".into(),
            "2026-07-20".into(),
        ]);
        assert_eq!(
            got,
            vec!["2026-07-20".to_string(), "2026-07-21".to_string()]
        );
    }

    #[test]
    fn metadata_dates_with_no_filesystem_answer() {
        let (dates, modified_iso) = metadata_date_strings_in(None, None, &Local);
        assert!(dates.is_empty());
        assert_eq!(
            modified_iso,
            Local::now().date_naive().format("%Y-%m-%d").to_string()
        );
    }

    #[test]
    fn trailing_dates_and_years_are_stripped_from_subjects() {
        // The exact shape observed shipping in the 2026-08 E2E.
        assert_eq!(
            strip_trailing_dates(
                "Form 8829 - Marcus Alvarez - Globex Corporation - 2026-08-05 - 2026"
            )
            .as_deref(),
            Some("Form 8829 - Marcus Alvarez - Globex Corporation")
        );
        assert_eq!(
            strip_trailing_dates("Termination Notice - Northwind - 2021 - 2021 - 2021").as_deref(),
            Some("Termination Notice - Northwind")
        );
        // A year that IS the subject's head stays; only a dangling tail goes.
        assert_eq!(strip_trailing_dates("2026 Annual Report - Acme"), None);
        // No date, no change.
        assert_eq!(strip_trailing_dates("Form 8829 - Marcus Alvarez"), None);
        // A form number is not a year.
        assert_eq!(strip_trailing_dates("Schedule E - Form 1120"), None);
    }

    #[test]
    fn dangling_clause_fragments_are_stripped_from_subjects() {
        assert_eq!(
            strip_dangling_words("Form 8829 - Initech - Globex Corporation shall recover", 2)
                .as_deref(),
            Some("Form 8829 - Initech - Globex Corporation")
        );
        assert_eq!(
            strip_dangling_words(
                "Form 8829 - Acme Industries - Umbrella Holdings - Effective",
                2
            )
            .as_deref(),
            Some("Form 8829 - Acme Industries - Umbrella Holdings")
        );
        // Trailing-position only: these words are legitimate mid-subject.
        assert_eq!(
            strip_dangling_words("Notice of Termination - Smith", 2),
            None
        );
        // Never strip below the minimum word count.
        assert_eq!(strip_dangling_words("Notice of", 2), None);
    }

    #[test]
    fn document_preamble_is_stripped_from_model_descriptions() {
        // The register style: jump straight into what the thing is.
        assert_eq!(
            strip_document_preamble(
                "The document is a shareholder's register transferring 40,000 shares to John Smith."
            ),
            "Shareholder's register transferring 40,000 shares to John Smith."
        );
        // A verb after the preamble survives as the opening word.
        assert_eq!(
            strip_document_preamble(
                "This document confirms termination of employment effective April 11, 2026."
            ),
            "Confirms termination of employment effective April 11, 2026."
        );
        assert_eq!(
            strip_document_preamble("The file was an invoice for services rendered in June."),
            "Invoice for services rendered in June."
        );
        // "the" as article after the linking verb.
        assert_eq!(
            strip_document_preamble("The document is the annual report of Acme Industries."),
            "Annual report of Acme Industries."
        );
    }

    #[test]
    fn preamble_stripping_leaves_everything_else_alone() {
        // Already in register style.
        let clean = "Shareholder's register transferring 40,000 shares to John Smith.";
        assert_eq!(strip_document_preamble(clean), clean);
        // "documentation" is not "document " — word boundary matters.
        let docs = "The documentation covers deployment and rollback procedures.";
        assert_eq!(strip_document_preamble(docs), docs);
        // Stripping below the checker's 15-char floor keeps the original.
        let short = "The document is a note.";
        assert_eq!(strip_document_preamble(short), short);
        // Mid-sentence mentions are untouched.
        let mid = "Annexes to the document list all parties.";
        assert_eq!(strip_document_preamble(mid), mid);
    }
}
