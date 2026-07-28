//! §5: evidence filter. Assembles the bundle the SLM actually reads:
//!   5a deterministic harvest (always)
//!   5b language gate (Lingua, sidecar)
//!   5c doc-type classification (GLiClass, sidecar) -> subject template + probes
//!   5d salience ranking (granite embeddings, sidecar) -> only when 5a is thin
//!   5e Ettin span proposals (fine-tuned token classifier, sidecar, optional)
//!
//! 5c and 5d are naming ENHANCEMENTS, not requirements: the shipped sidecar
//! is the slim, torch-free profile (no gliclass/transformers/
//! sentence-transformers), so `sidecar.classify`/`sidecar.salience` normally
//! return convertd.py's deterministic fallbacks (a neutral doc_type,
//! document-order sentences) rather than erroring -- see
//! `sidecar/convertd.py`'s `op_classify`/`op_salience`. The `unwrap_or_else`/
//! `if let Ok` below are the second line of defense for the rarer case of a
//! transport-level sidecar error (timeout, crash), so this function still
//! never fails the pipeline over 5c/5d.
//! Budget-bounded: chars/4 as the token approximation.

use crate::harvest::{self, Harvest};
use crate::sidecar::{EttinSpan, Sidecar};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Evidence {
    pub bundle: String,
    pub language: String,
    /// `None` when no classifier actually ran. On the shipped torch-free
    /// profile `_classify_fallback` returns a constant label with
    /// `available: false`; shipping that constant made every row in the
    /// SharePoint DocumentIndex claim the same DocType — a fabricated
    /// classification in a system of record, which is the exact failure mode
    /// the checker exists to prevent.
    pub doc_type: Option<String>,
    pub doc_type_score: f64,
    pub harvest: Harvest,
    /// Dates from the document's own file/embedded metadata. Kept on the
    /// Evidence so a wider retry bundle can be reassembled from it alone.
    pub meta_dates: Vec<String>,
    /// The 5d salience picks, kept for the same reason as `meta_dates` — and
    /// more urgently, because this is the one bundle section that cannot be
    /// recomputed without another sidecar round-trip. 5d fires only when the
    /// deterministic harvest came back thin, i.e. exactly when KEY SENTENCES is
    /// the only substantive section there is; a retry rung that dropped it
    /// would hand a thin document LESS evidence than the rung before it.
    pub salient: Vec<String>,
    pub ettin_spans: Vec<EttinSpan>,
    pub thin: bool,
}

/// Doc-type taxonomy. Editable config, no retraining: GLiClass is zero-shot.
pub fn default_labels() -> Vec<String> {
    [
        "termination notice",
        "offer letter",
        "engagement letter",
        "demand letter",
        "non-disclosure agreement",
        "services agreement",
        "purchase agreement",
        "lease agreement",
        "amendment",
        "invoice",
        "receipt",
        "purchase order",
        "complaint",
        "answer",
        "motion",
        "brief",
        "court order",
        "subpoena",
        "deposition transcript",
        "discovery request",
        "settlement agreement",
        "corporate resolution",
        "board minutes",
        "bylaws",
        "operating agreement",
        "policy document",
        "memorandum",
        "correspondence",
        "email",
        "financial statement",
        "tax document",
        "insurance document",
        "employment agreement",
        "severance agreement",
        "power of attorney",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Type-specific probe queries for salience ranking.
fn probes_for(doc_type: &str) -> Vec<String> {
    let base = vec![
        "date of this document".to_string(),
        "parties to this document".to_string(),
        "subject matter of this document".to_string(),
    ];
    let extra: Vec<&str> = match doc_type {
        t if t.contains("termination") => {
            vec!["effective date of termination", "employee being terminated"]
        }
        t if t.contains("agreement") || t.contains("nda") => vec![
            "effective date of this agreement",
            "parties entering this agreement",
        ],
        t if t.contains("invoice") || t.contains("receipt") || t.contains("order") => vec![
            "invoice number and total amount due",
            "vendor and customer names",
        ],
        t if t.contains("complaint")
            || t.contains("motion")
            || t.contains("brief")
            || t.contains("court") =>
        {
            vec!["case caption plaintiff and defendant", "relief requested"]
        }
        t if t.contains("deposition") => vec!["name of the deponent", "date of the deposition"],
        t if t.contains("minutes") || t.contains("resolution") => {
            vec!["date of the meeting", "resolutions adopted"]
        }
        _ => vec![],
    };
    base.into_iter()
        .chain(extra.into_iter().map(String::from))
        .collect()
}

pub struct FilterOutcome {
    pub evidence: Evidence,
    pub doc_meta_dates: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
pub fn build_evidence(
    sidecar: &Sidecar,
    markdown: &str,
    doc_meta_dates: Vec<String>,
    ettin_enabled: bool,
    token_budget: usize,
) -> anyhow::Result<FilterOutcome> {
    let h = harvest::harvest(markdown);

    // 5b: language gate on a head sample.
    let lang_sample: String = markdown.chars().take(1500).collect();
    let language = sidecar.langid(&lang_sample).unwrap_or_else(|_| "en".into());

    // 5c: classify on the harvested evidence, not the full doc.
    let classify_text = format!(
        "{}\n{}\n{}",
        h.subject_lines.join("\n"),
        h.headings.join("\n"),
        h.head_excerpt.chars().take(1200).collect::<String>()
    );
    let (doc_type, doc_type_score) = classify(sidecar, &classify_text, &default_labels());

    // Is 5a thin? (no subject line, no caption, few headings)
    let thin = h.subject_lines.is_empty() && h.caption_lines.is_empty() && h.headings.len() < 2;

    // 5d: salience only when thin.
    let mut salient: Vec<String> = Vec::new();
    if thin {
        let sentences: Vec<String> = split_sentences(markdown, 400);
        if sentences.len() > 4 {
            let probes = probes_for(doc_type.as_deref().unwrap_or(""));
            if let Ok(mut idx) = sidecar.salience(&sentences, &probes, 12) {
                idx.sort_unstable(); // restore document order
                salient = idx
                    .into_iter()
                    .filter_map(|i| sentences.get(i).cloned())
                    .collect();
            }
        }
    }

    // 5e: Ettin span proposals, pinned to the top of the bundle.
    let ettin_spans = if ettin_enabled {
        let head: String = markdown.chars().take(8000).collect();
        sidecar.ettin_spans(&head).unwrap_or_default()
    } else {
        Vec::new()
    };

    // ---- assemble, budget-bounded (chars/4 ~ tokens) ----------------------
    let bundle = assemble_bundle(
        &h,
        &doc_meta_dates,
        &ettin_spans,
        &salient,
        token_budget * 4,
    );

    Ok(FilterOutcome {
        evidence: Evidence {
            bundle,
            language,
            doc_type,
            doc_type_score,
            harvest: h,
            meta_dates: doc_meta_dates.clone(),
            salient,
            ettin_spans,
            thin,
        },
        doc_meta_dates,
    })
}

/// A doc_type we can actually stand behind, or `None`.
///
/// Every path that did not really classify the document scores it 0.0:
/// `_classify_fallback` (no gliclass on the shipped torch-free profile, or a
/// live model that errored), op_classify's empty-results case, and the
/// transport failure handled below. A zero-confidence best label is not a
/// classification, so the score is the gate — and it is a slightly stricter
/// one than op_classify's `available` flag, which still reports `true` for the
/// empty-results case. (`Sidecar::classify` collapses the wire response to
/// `(label, score)` and drops `available` outright; see the cross-workstream
/// note in this change's report.)
fn classify(sidecar: &Sidecar, text: &str, labels: &[String]) -> (Option<String>, f64) {
    // 5c is a naming enhancement, not a requirement: a transport-level error
    // still must not fail the pipeline.
    let Ok((label, score)) = sidecar.classify(text, labels) else {
        return (None, 0.0);
    };
    if score > 0.0 {
        (Some(label), score)
    } else {
        (None, 0.0)
    }
}

/// The one place the bundle's shape is defined, so a retry that widens the
/// budget produces the same document in more detail rather than a different
/// document.
fn assemble_bundle(
    h: &Harvest,
    meta_dates: &[String],
    ettin_spans: &[EttinSpan],
    salient: &[String],
    char_budget: usize,
) -> String {
    let mut bundle = String::with_capacity(char_budget.min(16_000));

    if !ettin_spans.is_empty() {
        bundle.push_str("EXTRACTED SPANS (high confidence):\n");
        for s in ettin_spans {
            let iso = s
                .iso
                .as_deref()
                .map(|i| format!(" [{i}]"))
                .unwrap_or_default();
            bundle.push_str(&format!("- {}: {}{}\n", s.label, s.text.trim(), iso));
        }
        bundle.push('\n');
    }
    if !h.dates.is_empty() {
        bundle.push_str("DATES FOUND IN DOCUMENT:\n");
        for d in h.dates.iter().take(10) {
            bundle.push_str(&format!("- {} (\"{}\")\n", d.iso, d.raw));
        }
        bundle.push('\n');
    }
    if !meta_dates.is_empty() {
        bundle.push_str(&format!(
            "FILE METADATA DATES: {}\n\n",
            meta_dates.join(", ")
        ));
    }
    if !h.subject_lines.is_empty() {
        bundle.push_str("SUBJECT / HEADER LINES:\n");
        for s in h.subject_lines.iter().take(8) {
            bundle.push_str(&format!("- {s}\n"));
        }
        bundle.push('\n');
    }
    if !h.caption_lines.is_empty() {
        bundle.push_str("CASE CAPTION LINES:\n");
        for s in h.caption_lines.iter().take(4) {
            bundle.push_str(&format!("- {s}\n"));
        }
        bundle.push('\n');
    }
    if !h.headings.is_empty() {
        bundle.push_str("HEADINGS:\n");
        for s in h.headings.iter().take(8) {
            bundle.push_str(&format!("- {s}\n"));
        }
        bundle.push('\n');
    }
    if !salient.is_empty() {
        bundle.push_str("KEY SENTENCES:\n");
        for s in salient {
            bundle.push_str(&format!("- {}\n", s.trim()));
        }
        bundle.push('\n');
    }
    bundle.push_str("DOCUMENT OPENING:\n");
    bundle.push_str(&h.head_excerpt);
    bundle.push_str("\n\nSIGNATURE BLOCK / ENDING:\n");
    bundle.push_str(&h.signature_block);

    if bundle.len() > char_budget {
        bundle.truncate(floor_char_boundary(&bundle, char_budget));
    }
    bundle
}

/// Widened evidence for the "attempt 3" escalation: the same sections, a
/// larger budget. The ladder's contract is that each rung varies the INPUT;
/// rung 3 previously varied only the model, so a rejection caused by evidence
/// that had been truncated away could never be recovered.
///
/// Rebuilt from the harvest rather than from `ev.bundle`, because the bundle
/// is the already-truncated artifact — re-truncating it can only ever remove
/// more. Every section the first bundle had is carried on the `Evidence` for
/// this call, salience included: dropping KEY SENTENCES here would have handed
/// a thin document less material than rung 1 saw, which is the opposite of what
/// the rung is for.
pub fn widened_bundle(ev: &Evidence, token_budget: usize) -> String {
    assemble_bundle(
        &ev.harvest,
        &ev.meta_dates,
        &ev.ettin_spans,
        &ev.salient,
        token_budget * 4,
    )
}

/// Trimmed evidence for the "attempt 2" retry: 5a-only, half budget.
pub fn trimmed_bundle(ev: &Evidence, token_budget: usize) -> String {
    let mut b = String::new();
    if !ev.harvest.dates.is_empty() {
        b.push_str("DATES FOUND IN DOCUMENT:\n");
        for d in ev.harvest.dates.iter().take(6) {
            b.push_str(&format!("- {} (\"{}\")\n", d.iso, d.raw));
        }
        b.push('\n');
    }
    for s in ev.harvest.subject_lines.iter().take(5) {
        b.push_str(&format!("SUBJECT: {s}\n"));
    }
    b.push_str("\nDOCUMENT OPENING:\n");
    b.push_str(&ev.harvest.head_excerpt);
    let cap = token_budget * 2; // half the normal char budget
    if b.len() > cap {
        b.truncate(floor_char_boundary(&b, cap));
    }
    b
}

fn split_sentences(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        for s in para.split_inclusive(['.', '!', '?']) {
            let t = s.trim();
            if t.chars().count() >= 25 && t.chars().count() <= 350 {
                out.push(t.to_string());
                if out.len() >= max {
                    return out;
                }
            }
        }
    }
    out
}

/// The largest char boundary at or below `i`. `String::truncate` panics on a
/// byte index that splits a codepoint, which is a live hazard here: the budget
/// is an arithmetic multiple of a configurable token count and has no relation
/// to where the document's characters happen to fall. `pub(crate)` so the
/// orchestrator's tests can assert the exact cut rather than that the result
/// merely parses.
pub(crate) fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
