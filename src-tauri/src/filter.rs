//! §5: evidence filter. Assembles the exact, budget-bounded evidence the SLM
//! reads without summarizing or rewriting the source document:
//!   5a deterministic harvest (always)
//!   5b language gate (Lingua, sidecar)
//!   5c document-type classification (optional enhancement)
//!   5d full-document cached-label entity extraction (optional enhancement)
//!   5e semantic paragraph ranking with exact source provenance (optional)
//!   5f independently budgeted evidence lanes plus a compression trace
//!
//! Every model-backed operation is fail-open. Missing assets, an unavailable
//! sidecar, or low-value compression return the pipeline to deterministic exact
//! text rather than failing a file. The checker remains the final authority.

use crate::harvest::{self, Harvest};
use crate::sidecar::{
    EntityExtractionResult, EntityLabel, EntitySpan, EttinSpan, RankedParagraph,
    SemanticRankResult, Sidecar, SourceParagraph,
};
use serde::Serialize;
use std::collections::HashSet;

const TRACE_SCHEMA_VERSION: u8 = 1;
const MAX_PARAGRAPH_CHARS: usize = 1_200;
const SEMANTIC_TOP_K: usize = 12;
const SEMANTIC_MIN_SCORE: f64 = 0.12;
const SEMANTIC_DIVERSITY: f64 = 0.22;
const ENTITY_THRESHOLD: f64 = 0.42;
const ENTITY_MAX_PER_LABEL: usize = 8;
const MIN_COMPRESSION_SAVINGS_RATIO: f64 = 0.10;

#[derive(Debug, Clone, Serialize, Default)]
pub struct CompressionMetrics {
    pub source_chars: usize,
    pub source_tokens_approx: usize,
    pub bundle_chars: usize,
    pub bundle_tokens_approx: usize,
    pub saved_chars: usize,
    pub savings_ratio: f64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct LaneTrace {
    pub name: String,
    pub budget_chars: usize,
    pub emitted_chars: usize,
    pub items_seen: usize,
    pub items_emitted: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvidenceTrace {
    pub schema_version: u8,
    pub routing: String,
    pub bypass_reason: Option<String>,
    pub semantic_model: String,
    pub semantic_available: bool,
    pub semantic_reason: Option<String>,
    pub entity_model: String,
    pub entity_available: bool,
    pub entity_reason: Option<String>,
    pub label_cache_key: String,
    pub label_embeddings_reused: bool,
    pub candidates_considered: usize,
    pub source_paragraphs: usize,
    pub selected_paragraphs: usize,
    pub min_savings_ratio: f64,
    pub ranked_paragraphs: Vec<RankedParagraph>,
    pub entities: Vec<EntitySpan>,
    pub lanes: Vec<LaneTrace>,
    pub compression: CompressionMetrics,
}

impl Default for EvidenceTrace {
    fn default() -> Self {
        Self {
            schema_version: TRACE_SCHEMA_VERSION,
            routing: "not_recorded".into(),
            bypass_reason: None,
            semantic_model: "not_invoked".into(),
            semantic_available: false,
            semantic_reason: None,
            entity_model: "not_invoked".into(),
            entity_available: false,
            entity_reason: None,
            label_cache_key: String::new(),
            label_embeddings_reused: false,
            candidates_considered: 0,
            source_paragraphs: 0,
            selected_paragraphs: 0,
            min_savings_ratio: MIN_COMPRESSION_SAVINGS_RATIO,
            ranked_paragraphs: Vec::new(),
            entities: Vec::new(),
            lanes: Vec::new(),
            compression: CompressionMetrics::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Evidence {
    pub bundle: String,
    pub language: String,
    /// `None` when no classifier actually ran. A fallback label must never be
    /// presented as a classification in the SharePoint system of record.
    pub doc_type: Option<String>,
    pub doc_type_score: f64,
    pub harvest: Harvest,
    /// Dates from the document's own embedded metadata. Kept so retries can
    /// rebuild a wider bundle without another conversion.
    pub meta_dates: Vec<String>,
    /// Compatibility copy of selected exact text. New code should use
    /// `ranked_paragraphs`, which also preserves source locations and scores.
    pub salient: Vec<String>,
    pub ettin_spans: Vec<EttinSpan>,
    pub thin: bool,
    /// Every exact source paragraph, retained only with the cached Markdown and
    /// used to widen an escalation request without re-running the sidecar.
    pub paragraphs: Vec<SourceParagraph>,
    pub ranked_paragraphs: Vec<RankedParagraph>,
    pub entities: Vec<EntitySpan>,
    pub semantic_lane_char_budget: usize,
    pub trace: EvidenceTrace,
}

/// Document-type taxonomy. Editable config, no retraining required.
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

pub fn default_entity_labels() -> Vec<EntityLabel> {
    [
        (
            "PERSON",
            "a human person's full name, employee, signer, deponent, attorney, or individual",
        ),
        (
            "ORGANIZATION",
            "a company, organization, agency, court, employer, vendor, or other legal entity",
        ),
        (
            "PARTY",
            "a party entering, receiving, assigning, or terminating an agreement or notice",
        ),
        (
            "SUBJECT",
            "the main subject, document title, matter, transaction, or purpose",
        ),
        (
            "DOCUMENT_DATE",
            "the date a document, letter, order, invoice, or notice was written, signed, issued, or filed",
        ),
        (
            "EFFECTIVE_DATE",
            "the date on which an agreement, amendment, policy, or obligation becomes effective",
        ),
        (
            "TERMINATION_DATE",
            "the date employment, an agreement, service, or another relationship terminates or ends",
        ),
        (
            "CASE_NUMBER",
            "a court case, claim, docket, cause, or matter number",
        ),
        (
            "INVOICE_NUMBER",
            "an invoice, receipt, purchase order, or billing identifier",
        ),
        (
            "AMOUNT",
            "a monetary amount, price, payment, balance, total, or amount due",
        ),
    ]
    .into_iter()
    .map(|(label, description)| EntityLabel {
        label: label.into(),
        description: description.into(),
    })
    .collect()
}

/// Type-specific probe queries for paragraph ranking.
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
    let mut h = harvest::harvest(markdown);
    let paragraphs = segment_paragraphs(markdown);
    let char_budget = token_budget.saturating_mul(4);

    // 5b: language gate on a head sample.
    let lang_sample: String = markdown.chars().take(1_500).collect();
    let language = sidecar.langid(&lang_sample).unwrap_or_else(|_| "en".into());

    // 5c: classify harvested evidence, not the full document.
    let classify_text = format!(
        "{}\n{}\n{}",
        h.subject_lines.join("\n"),
        h.headings.join("\n"),
        h.head_excerpt.chars().take(1_200).collect::<String>()
    );
    let (doc_type, doc_type_score) = classify(sidecar, &classify_text, &default_labels());
    let thin = h.subject_lines.is_empty() && h.caption_lines.is_empty() && h.headings.len() < 2;
    let probes = probes_for(doc_type.as_deref().unwrap_or(""));

    // 5d: deterministic candidate generation plus cached semantic labels over
    // every source paragraph. A missing model leaves this lane empty and the
    // deterministic harvest remains authoritative.
    let entity_result = sidecar
        .extract_entities(
            &paragraphs,
            &default_entity_labels(),
            ENTITY_THRESHOLD,
            ENTITY_MAX_PER_LABEL,
        )
        .unwrap_or_else(|_| unavailable_entities("sidecar_error"));

    // 5e: exact paragraph routing. Compression is bypassed before inference
    // whenever the body already fits or the maximum useful saving would be
    // below the materiality floor.
    let selection = select_paragraphs(sidecar, &paragraphs, &probes, char_budget);
    let ranked_paragraphs = selection.ranked.clone();
    let salient: Vec<String> = ranked_paragraphs
        .iter()
        .map(|paragraph| paragraph.text.clone())
        .collect();

    // Existing optional Ettin support remains a separate legacy lane. It is no
    // longer the only model-backed extraction path and still never blocks a job.
    let ettin_spans = if ettin_enabled {
        let head: String = markdown.chars().take(8_000).collect();
        sidecar.ettin_spans(&head).unwrap_or_default()
    } else {
        Vec::new()
    };

    // Anything shown to the naming model must also be admitted to the
    // checker's evidence ledger. The selected paragraphs and entity extractor
    // reach the complete document, beyond the deterministic harvester's
    // head/tail windows. Their positions are deliberately marked unknown here:
    // mixing Python Unicode offsets with Rust regex byte offsets would create a
    // false "near the top" preference, which is worse than no position signal.
    const POSITION_UNKNOWN: usize = usize::MAX;
    for paragraph in &ranked_paragraphs {
        for found in harvest::extract_dates(&paragraph.text) {
            if !h.dates.iter().any(|existing| existing.iso == found.iso) {
                h.dates.push(harvest::FoundDate {
                    offset: POSITION_UNKNOWN,
                    ..found
                });
            }
        }
    }
    for span in &entity_result.spans {
        if let Some(iso) = span.iso.as_deref() {
            if !h.dates.iter().any(|existing| existing.iso == iso) {
                h.dates.push(harvest::FoundDate {
                    iso: iso.to_string(),
                    raw: span.text.clone(),
                    offset: POSITION_UNKNOWN,
                    ambiguous: false,
                });
            }
        }
    }
    for span in &ettin_spans {
        if let Some(iso) = span.iso.as_deref() {
            if !h.dates.iter().any(|existing| existing.iso == iso) {
                h.dates.push(harvest::FoundDate {
                    iso: iso.to_string(),
                    raw: span.text.clone(),
                    offset: POSITION_UNKNOWN,
                    ambiguous: false,
                });
            }
        }
    }

    let assembly = assemble_bundle(
        &h,
        &doc_meta_dates,
        &ettin_spans,
        &entity_result.spans,
        &ranked_paragraphs,
        &salient,
        selection.lane_char_budget,
        char_budget,
    );
    let compression = compression_metrics(markdown, &assembly.bundle);
    let trace = EvidenceTrace {
        schema_version: TRACE_SCHEMA_VERSION,
        routing: selection.routing.clone(),
        bypass_reason: selection.bypass_reason.clone(),
        semantic_model: selection.model.clone(),
        semantic_available: selection.available,
        semantic_reason: selection.reason.clone(),
        entity_model: entity_result.model.clone(),
        entity_available: entity_result.available,
        entity_reason: entity_result.reason.clone(),
        label_cache_key: entity_result.label_cache_key.clone(),
        label_embeddings_reused: entity_result.label_embeddings_reused,
        candidates_considered: entity_result.candidates_considered,
        source_paragraphs: paragraphs.len(),
        selected_paragraphs: ranked_paragraphs.len(),
        min_savings_ratio: MIN_COMPRESSION_SAVINGS_RATIO,
        ranked_paragraphs: ranked_paragraphs.clone(),
        entities: entity_result.spans.clone(),
        lanes: assembly.lanes,
        compression,
    };

    Ok(FilterOutcome {
        evidence: Evidence {
            bundle: assembly.bundle,
            language,
            doc_type,
            doc_type_score,
            harvest: h,
            meta_dates: doc_meta_dates.clone(),
            salient,
            ettin_spans,
            thin,
            paragraphs,
            ranked_paragraphs,
            entities: entity_result.spans,
            semantic_lane_char_budget: selection.lane_char_budget,
            trace,
        },
        doc_meta_dates,
    })
}

/// A document type we can actually stand behind, or `None`.
fn classify(sidecar: &Sidecar, text: &str, labels: &[String]) -> (Option<String>, f64) {
    let Ok((label, score)) = sidecar.classify(text, labels) else {
        return (None, 0.0);
    };
    if score > 0.0 {
        (Some(label), score)
    } else {
        (None, 0.0)
    }
}

fn unavailable_entities(reason: &str) -> EntityExtractionResult {
    EntityExtractionResult {
        available: false,
        model: "unknown".into(),
        reason: Some(reason.into()),
        spans: Vec::new(),
        label_cache_key: String::new(),
        label_embeddings_reused: false,
        candidates_considered: 0,
    }
}

#[derive(Debug, Clone)]
struct ParagraphSelection {
    ranked: Vec<RankedParagraph>,
    routing: String,
    bypass_reason: Option<String>,
    model: String,
    available: bool,
    reason: Option<String>,
    lane_char_budget: usize,
}

fn semantic_lane_budget(char_budget: usize) -> usize {
    ((char_budget.saturating_mul(35)) / 100)
        .max(char_budget.min(512))
        .min(char_budget)
}

fn ranked_without_compression(paragraphs: &[SourceParagraph], reason: &str) -> Vec<RankedParagraph> {
    paragraphs
        .iter()
        .enumerate()
        .map(|(rank, paragraph)| RankedParagraph {
            index: paragraph.index,
            text: paragraph.text.clone(),
            start_char: paragraph.start_char,
            end_char: paragraph.end_char,
            score: 1.0,
            probe: reason.into(),
            rank: rank + 1,
        })
        .collect()
}

fn select_paragraphs(
    sidecar: &Sidecar,
    paragraphs: &[SourceParagraph],
    probes: &[String],
    char_budget: usize,
) -> ParagraphSelection {
    let source_chars: usize = paragraphs
        .iter()
        .map(|paragraph| paragraph.text.chars().count())
        .sum();
    if paragraphs.is_empty() {
        return ParagraphSelection {
            ranked: Vec::new(),
            routing: "empty_document".into(),
            bypass_reason: Some("no source paragraphs".into()),
            model: "not_invoked".into(),
            available: false,
            reason: None,
            lane_char_budget: 0,
        };
    }

    let normal_lane_budget = semantic_lane_budget(char_budget);
    let rendered_overhead = paragraphs.len().saturating_mul(96).saturating_add(128);
    let full_lane_budget = source_chars
        .saturating_add(rendered_overhead)
        .min(char_budget);
    if source_chars <= normal_lane_budget {
        return ParagraphSelection {
            ranked: ranked_without_compression(paragraphs, "compression bypassed: source fits"),
            routing: "bypass_source_fits".into(),
            bypass_reason: Some("all exact paragraphs fit the evidence lane".into()),
            model: "not_invoked".into(),
            available: false,
            reason: None,
            lane_char_budget: full_lane_budget,
        };
    }

    // If squeezing to the normal lane would save less than 10%, an embedding
    // round-trip and a lossy selection buy almost nothing. Spend the small
    // amount of additional context and preserve the whole exact body instead.
    let materiality_limit = ((normal_lane_budget as f64)
        / (1.0 - MIN_COMPRESSION_SAVINGS_RATIO))
        .ceil() as usize;
    if source_chars <= materiality_limit && full_lane_budget <= char_budget {
        return ParagraphSelection {
            ranked: ranked_without_compression(
                paragraphs,
                "compression bypassed: savings below materiality floor",
            ),
            routing: "bypass_negligible_savings".into(),
            bypass_reason: Some(format!(
                "projected savings below {:.0}%",
                MIN_COMPRESSION_SAVINGS_RATIO * 100.0
            )),
            model: "not_invoked".into(),
            available: false,
            reason: None,
            lane_char_budget: full_lane_budget,
        };
    }

    let result = sidecar
        .rank_paragraphs(
            paragraphs,
            probes,
            SEMANTIC_TOP_K,
            SEMANTIC_MIN_SCORE,
            SEMANTIC_DIVERSITY,
        )
        .unwrap_or_else(|_| SemanticRankResult {
            available: false,
            model: "unknown".into(),
            reason: Some("sidecar_error".into()),
            results: Vec::new(),
            source_chars,
            selected_chars: 0,
        });

    if result.available && !result.results.is_empty() {
        let actual_savings = if source_chars == 0 {
            0.0
        } else {
            1.0 - (result.selected_chars.min(source_chars) as f64 / source_chars as f64)
        };
        return ParagraphSelection {
            ranked: result.results,
            routing: if actual_savings < MIN_COMPRESSION_SAVINGS_RATIO {
                "semantic_required_low_savings".into()
            } else {
                "semantic_ranked".into()
            },
            bypass_reason: None,
            model: result.model,
            available: true,
            reason: result.reason,
            lane_char_budget: normal_lane_budget,
        };
    }

    ParagraphSelection {
        ranked: deterministic_fallback_rank(paragraphs, probes, SEMANTIC_TOP_K),
        routing: "semantic_unavailable".into(),
        bypass_reason: Some("semantic model unavailable; exact deterministic fallback used".into()),
        model: result.model,
        available: false,
        reason: result.reason,
        lane_char_budget: normal_lane_budget,
    }
}

fn deterministic_fallback_rank(
    paragraphs: &[SourceParagraph],
    probes: &[String],
    top_k: usize,
) -> Vec<RankedParagraph> {
    let probe_terms: Vec<Vec<String>> = probes
        .iter()
        .map(|probe| meaningful_terms(probe))
        .collect();
    let last_index = paragraphs.len().saturating_sub(1);
    let mut scored: Vec<(usize, f64, String)> = paragraphs
        .iter()
        .enumerate()
        .map(|(position, paragraph)| {
            let lower = paragraph.text.to_lowercase();
            let mut best_probe = "document order fallback".to_string();
            let mut best_overlap = 0.0;
            for (probe, terms) in probes.iter().zip(&probe_terms) {
                if terms.is_empty() {
                    continue;
                }
                let hits = terms.iter().filter(|term| lower.contains(term.as_str())).count();
                let overlap = hits as f64 / terms.len() as f64;
                if overlap > best_overlap {
                    best_overlap = overlap;
                    best_probe = probe.clone();
                }
            }
            let mut score = 0.55 * best_overlap;
            if !harvest::extract_dates(&paragraph.text).is_empty() {
                score += 0.25;
            }
            if lower.contains("subject:")
                || lower.contains("effective date")
                || lower.contains("termination date")
                || lower.contains("by and between")
            {
                score += 0.12;
            }
            if position == 0 {
                score += 0.08;
            }
            if position == last_index {
                score += 0.06;
            }
            (position, score.clamp(0.0, 1.0), best_probe)
        })
        .collect();
    scored.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.0.cmp(&right.0))
    });
    scored
        .into_iter()
        .take(top_k.min(paragraphs.len()))
        .enumerate()
        .map(|(rank, (position, score, probe))| {
            let paragraph = &paragraphs[position];
            RankedParagraph {
                index: paragraph.index,
                text: paragraph.text.clone(),
                start_char: paragraph.start_char,
                end_char: paragraph.end_char,
                score,
                probe,
                rank: rank + 1,
            }
        })
        .collect()
}

fn meaningful_terms(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|term| term.chars().count() >= 4)
        .collect()
}

/// Stable exact-source paragraph segmentation. Blank lines define normal
/// blocks; oversized blocks are split at a newline when possible and otherwise
/// at a Unicode character boundary. No source text is rewritten.
pub(crate) fn segment_paragraphs(markdown: &str) -> Vec<SourceParagraph> {
    let chars: Vec<char> = markdown.chars().collect();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut block_start: Option<usize> = None;
    let mut line_start = 0usize;

    for line_end in 0..=chars.len() {
        if line_end < chars.len() && chars[line_end] != '\n' {
            continue;
        }
        let line_is_blank = chars[line_start..line_end]
            .iter()
            .all(|character| character.is_whitespace());
        if line_is_blank {
            if let Some(start) = block_start.take() {
                push_paragraph_ranges(&chars, start, line_start, &mut ranges);
            }
        } else if block_start.is_none() {
            block_start = Some(line_start);
        }
        line_start = line_end.saturating_add(1);
    }
    if let Some(start) = block_start {
        push_paragraph_ranges(&chars, start, chars.len(), &mut ranges);
    }

    ranges
        .into_iter()
        .enumerate()
        .map(|(index, (start_char, end_char))| SourceParagraph {
            index,
            text: chars[start_char..end_char].iter().collect(),
            start_char,
            end_char,
        })
        .collect()
}

fn push_paragraph_ranges(
    chars: &[char],
    raw_start: usize,
    raw_end: usize,
    ranges: &mut Vec<(usize, usize)>,
) {
    let mut start = raw_start;
    let mut end = raw_end.min(chars.len());
    while start < end && chars[start].is_whitespace() {
        start += 1;
    }
    while end > start && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    while start < end {
        let mut split = (start + MAX_PARAGRAPH_CHARS).min(end);
        if split < end {
            if let Some(relative) = chars[start + 200..split]
                .iter()
                .rposition(|character| *character == '\n')
            {
                split = start + 200 + relative;
            }
        }
        let mut chunk_end = split;
        while chunk_end > start && chars[chunk_end - 1].is_whitespace() {
            chunk_end -= 1;
        }
        if chunk_end > start {
            ranges.push((start, chunk_end));
        }
        start = split;
        while start < end && chars[start].is_whitespace() {
            start += 1;
        }
    }
}

#[derive(Debug, Clone)]
struct LaneItem {
    rendered: String,
    dedupe_key: Option<String>,
    allow_truncate: bool,
}

impl LaneItem {
    fn exact(rendered: String, dedupe_key: impl Into<Option<String>>) -> Self {
        Self {
            rendered,
            dedupe_key: dedupe_key.into(),
            allow_truncate: false,
        }
    }

    fn excerpt(rendered: String, dedupe_key: impl Into<Option<String>>) -> Self {
        Self {
            rendered,
            dedupe_key: dedupe_key.into(),
            allow_truncate: true,
        }
    }
}

struct BundleAssembly {
    bundle: String,
    lanes: Vec<LaneTrace>,
}

struct BundleBuilder {
    bundle: String,
    budget_chars: usize,
    used_chars: usize,
    lanes: Vec<LaneTrace>,
    seen: HashSet<String>,
}

impl BundleBuilder {
    fn new(budget_chars: usize) -> Self {
        Self {
            bundle: String::with_capacity(budget_chars.min(16_000)),
            budget_chars,
            used_chars: 0,
            lanes: Vec::new(),
            seen: HashSet::new(),
        }
    }

    fn remaining(&self) -> usize {
        self.budget_chars.saturating_sub(self.used_chars)
    }

    fn append(&mut self, text: &str) {
        self.used_chars += text.chars().count();
        self.bundle.push_str(text);
    }

    fn add_lane(&mut self, name: &str, heading: &str, items: Vec<LaneItem>, lane_budget: usize) {
        if items.is_empty() {
            return;
        }
        let items_seen = items.len();
        let cap = lane_budget.min(self.remaining());
        let header = format!("{heading}:\n");
        let header_chars = header.chars().count();
        if cap <= header_chars {
            self.lanes.push(LaneTrace {
                name: name.into(),
                budget_chars: lane_budget,
                emitted_chars: 0,
                items_seen,
                items_emitted: 0,
                truncated: true,
            });
            return;
        }

        let checkpoint_bytes = self.bundle.len();
        let checkpoint_chars = self.used_chars;
        self.append(&header);
        let mut lane_used = header_chars;
        let mut emitted = 0usize;
        let mut truncated = false;

        for item in items {
            if item
                .dedupe_key
                .as_ref()
                .is_some_and(|key| self.seen.contains(key))
            {
                continue;
            }
            let rendered = if item.rendered.ends_with('\n') {
                item.rendered
            } else {
                format!("{}\n", item.rendered)
            };
            let rendered_chars = rendered.chars().count();
            let available = cap
                .saturating_sub(lane_used)
                .min(self.remaining());
            if rendered_chars <= available {
                self.append(&rendered);
                lane_used += rendered_chars;
                emitted += 1;
                if let Some(key) = item.dedupe_key {
                    self.seen.insert(key);
                }
                continue;
            }
            if item.allow_truncate && available > 2 {
                let prefix: String = rendered.chars().take(available - 1).collect();
                self.append(&prefix);
                self.append("\n");
                lane_used += available;
                emitted += 1;
                truncated = true;
                if let Some(key) = item.dedupe_key {
                    self.seen.insert(key);
                }
                break;
            }
            truncated = true;
        }

        if emitted == 0 {
            self.bundle.truncate(checkpoint_bytes);
            self.used_chars = checkpoint_chars;
        } else if lane_used < cap && self.remaining() > 0 {
            self.append("\n");
        }
        self.lanes.push(LaneTrace {
            name: name.into(),
            budget_chars: lane_budget,
            emitted_chars: self.used_chars.saturating_sub(checkpoint_chars),
            items_seen,
            items_emitted: emitted,
            truncated,
        });
    }

    fn finish(self) -> BundleAssembly {
        BundleAssembly {
            bundle: self.bundle,
            lanes: self.lanes,
        }
    }
}

fn lane_cap(total: usize, percent: usize, floor: usize) -> usize {
    ((total.saturating_mul(percent)) / 100)
        .max(total.min(floor))
        .min(total)
}

#[allow(clippy::too_many_arguments)]
fn assemble_bundle(
    h: &Harvest,
    meta_dates: &[String],
    ettin_spans: &[EttinSpan],
    entities: &[EntitySpan],
    ranked_paragraphs: &[RankedParagraph],
    salient: &[String],
    semantic_lane_char_budget: usize,
    char_budget: usize,
) -> BundleAssembly {
    let mut builder = BundleBuilder::new(char_budget);

    builder.add_lane(
        "semantic_entities",
        "EXTRACTED ENTITIES (exact source spans)",
        entities
            .iter()
            .map(|span| {
                let iso = span
                    .iso
                    .as_deref()
                    .map(|value| format!(", normalized {value}"))
                    .unwrap_or_default();
                LaneItem::exact(
                    format!(
                        "- {}: \"{}\" [paragraph {}, chars {}..{}, score {:.2}{}]",
                        span.label,
                        span.text,
                        span.paragraph_index + 1,
                        span.start_char,
                        span.end_char,
                        span.score,
                        iso
                    ),
                    Some(format!(
                        "entity:{}:{}:{}:{}",
                        span.label, span.paragraph_index, span.start_char, span.end_char
                    )),
                )
            })
            .collect(),
        lane_cap(char_budget, 15, 600),
    );

    builder.add_lane(
        "legacy_ettin_spans",
        "OPTIONAL LEGACY EXTRACTED SPANS",
        ettin_spans
            .iter()
            .map(|span| {
                let iso = span
                    .iso
                    .as_deref()
                    .map(|value| format!(" [normalized {value}]"))
                    .unwrap_or_default();
                LaneItem::exact(
                    format!("- {}: {}{}", span.label, span.text.trim(), iso),
                    Some(format!("ettin:{}:{}", span.label, span.text.trim())),
                )
            })
            .collect(),
        lane_cap(char_budget, 6, 300),
    );

    builder.add_lane(
        "document_dates",
        "DATES FOUND IN DOCUMENT",
        h.dates
            .iter()
            .take(12)
            .map(|date| {
                LaneItem::exact(
                    format!("- {} (\"{}\")", date.iso, date.raw),
                    Some(format!("date:{}:{}", date.iso, date.raw)),
                )
            })
            .collect(),
        lane_cap(char_budget, 10, 400),
    );

    builder.add_lane(
        "metadata_dates",
        "FILE METADATA DATES",
        meta_dates
            .iter()
            .map(|date| LaneItem::exact(format!("- {date}"), Some(format!("meta-date:{date}"))))
            .collect(),
        lane_cap(char_budget, 5, 200),
    );

    builder.add_lane(
        "subject_headers",
        "SUBJECT / HEADER LINES",
        h.subject_lines
            .iter()
            .take(8)
            .map(|line| LaneItem::exact(format!("- {line}"), Some(line.clone())))
            .collect(),
        lane_cap(char_budget, 10, 400),
    );

    builder.add_lane(
        "case_captions",
        "CASE CAPTION LINES",
        h.caption_lines
            .iter()
            .take(6)
            .map(|line| LaneItem::exact(format!("- {line}"), Some(line.clone())))
            .collect(),
        lane_cap(char_budget, 8, 300),
    );

    builder.add_lane(
        "headings",
        "HEADINGS",
        h.headings
            .iter()
            .take(10)
            .map(|line| LaneItem::exact(format!("- {line}"), Some(line.clone())))
            .collect(),
        lane_cap(char_budget, 8, 300),
    );

    if !ranked_paragraphs.is_empty() {
        let mut ordered = ranked_paragraphs.to_vec();
        ordered.sort_by_key(|paragraph| paragraph.index);
        builder.add_lane(
            "ranked_paragraphs",
            "RANKED BODY PARAGRAPHS (exact source text)",
            ordered
                .iter()
                .map(|paragraph| {
                    LaneItem::exact(
                        format!(
                            "- [paragraph {}; rank {}; score {:.2}; probe: {}]\n{}",
                            paragraph.index + 1,
                            paragraph.rank,
                            paragraph.score,
                            paragraph.probe.replace(['\r', '\n'], " "),
                            paragraph.text
                        ),
                        Some(paragraph.text.clone()),
                    )
                })
                .collect(),
            semantic_lane_char_budget.min(char_budget),
        );
    } else {
        // Compatibility path for cached/test Evidence created before structured
        // paragraph provenance existed.
        builder.add_lane(
            "legacy_salience",
            "KEY SENTENCES",
            salient
                .iter()
                .map(|text| LaneItem::exact(format!("- {}", text.trim()), Some(text.clone())))
                .collect(),
            semantic_lane_char_budget.min(char_budget),
        );
    }

    builder.add_lane(
        "document_opening",
        "DOCUMENT OPENING",
        if h.head_excerpt.is_empty() {
            Vec::new()
        } else {
            vec![LaneItem::excerpt(
                h.head_excerpt.clone(),
                Some(h.head_excerpt.clone()),
            )]
        },
        lane_cap(char_budget, 15, 600),
    );

    builder.add_lane(
        "signature_ending",
        "SIGNATURE BLOCK / ENDING",
        if h.signature_block.is_empty() {
            Vec::new()
        } else {
            vec![LaneItem::excerpt(
                h.signature_block.clone(),
                Some(h.signature_block.clone()),
            )]
        },
        lane_cap(char_budget, 10, 400),
    );

    builder.finish()
}

fn compression_metrics(source: &str, bundle: &str) -> CompressionMetrics {
    let source_chars = source.chars().count();
    let bundle_chars = bundle.chars().count();
    let saved_chars = source_chars.saturating_sub(bundle_chars);
    CompressionMetrics {
        source_chars,
        source_tokens_approx: source_chars.div_ceil(4),
        bundle_chars,
        bundle_tokens_approx: bundle_chars.div_ceil(4),
        saved_chars,
        savings_ratio: if source_chars == 0 {
            0.0
        } else {
            saved_chars as f64 / source_chars as f64
        },
    }
}

fn expanded_ranked(ev: &Evidence) -> Vec<RankedParagraph> {
    let mut expanded = ev.ranked_paragraphs.clone();
    let mut seen: HashSet<usize> = expanded.iter().map(|paragraph| paragraph.index).collect();
    let mut next_rank = expanded
        .iter()
        .map(|paragraph| paragraph.rank)
        .max()
        .unwrap_or(0)
        + 1;
    for paragraph in &ev.paragraphs {
        if seen.insert(paragraph.index) {
            expanded.push(RankedParagraph {
                index: paragraph.index,
                text: paragraph.text.clone(),
                start_char: paragraph.start_char,
                end_char: paragraph.end_char,
                score: 0.0,
                probe: "widened exact source context".into(),
                rank: next_rank,
            });
            next_rank += 1;
        }
    }
    expanded
}

/// Widened evidence for the escalation rung. It preserves every lane from the
/// first attempt and appends previously unselected exact source paragraphs, so
/// the larger model receives the same document with more evidence rather than
/// a different lossy summary.
pub fn widened_bundle(ev: &Evidence, token_budget: usize) -> String {
    let char_budget = token_budget.saturating_mul(4);
    let expanded = expanded_ranked(ev);
    let semantic_budget = ev
        .semantic_lane_char_budget
        .max((char_budget.saturating_mul(50)) / 100)
        .min(char_budget);
    assemble_bundle(
        &ev.harvest,
        &ev.meta_dates,
        &ev.ettin_spans,
        &ev.entities,
        &expanded,
        &ev.salient,
        semantic_budget,
        char_budget,
    )
    .bundle
}

/// Trimmed evidence for the second attempt: deterministic lanes only, at half
/// the normal character allowance.
pub fn trimmed_bundle(ev: &Evidence, token_budget: usize) -> String {
    let mut bundle = String::new();
    if !ev.harvest.dates.is_empty() {
        bundle.push_str("DATES FOUND IN DOCUMENT:\n");
        for date in ev.harvest.dates.iter().take(6) {
            bundle.push_str(&format!("- {} (\"{}\")\n", date.iso, date.raw));
        }
        bundle.push('\n');
    }
    for subject in ev.harvest.subject_lines.iter().take(5) {
        bundle.push_str(&format!("SUBJECT: {subject}\n"));
    }
    bundle.push_str("\nDOCUMENT OPENING:\n");
    bundle.push_str(&ev.harvest.head_excerpt);
    truncate_to_chars(&bundle, token_budget.saturating_mul(2))
}

fn truncate_to_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        text.chars().take(max_chars).collect()
    }
}

/// The largest UTF-8 byte boundary at or below `i`. Retained for callers that
/// must cut a byte buffer; evidence assembly itself now budgets Unicode
/// characters directly.
pub(crate) fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraph_segmentation_preserves_exact_unicode_source_and_offsets() {
        let source = "  Subject: Reçu\ncontinued\n\nSecond paragraph with José.\n\n";
        let paragraphs = segment_paragraphs(source);
        assert_eq!(paragraphs.len(), 2);
        assert_eq!(paragraphs[0].text, "Subject: Reçu\ncontinued");
        assert_eq!(paragraphs[1].text, "Second paragraph with José.");
        for paragraph in paragraphs {
            let exact: String = source
                .chars()
                .skip(paragraph.start_char)
                .take(paragraph.end_char - paragraph.start_char)
                .collect();
            assert_eq!(exact, paragraph.text);
        }
    }

    #[test]
    fn oversized_blocks_split_without_rewriting_source() {
        let source = "a".repeat(MAX_PARAGRAPH_CHARS * 2 + 17);
        let paragraphs = segment_paragraphs(&source);
        assert_eq!(paragraphs.len(), 3);
        assert!(paragraphs
            .iter()
            .all(|paragraph| paragraph.text.chars().count() <= MAX_PARAGRAPH_CHARS));
        assert_eq!(
            paragraphs
                .iter()
                .map(|paragraph| paragraph.text.as_str())
                .collect::<String>(),
            source
        );
    }

    #[test]
    fn deterministic_fallback_prefers_dates_and_subject_terms() {
        let paragraphs = segment_paragraphs(
            "Boilerplate only.\n\nThe effective date is March 5, 2024.\n\nOther material.",
        );
        let ranked = deterministic_fallback_rank(
            &paragraphs,
            &["effective date of this agreement".into()],
            2,
        );
        assert_eq!(ranked[0].index, 1);
        assert!(ranked[0].text.contains("March 5, 2024"));
    }

    #[test]
    fn lanes_keep_exact_entities_and_ranked_source_separate() {
        let harvest = harvest::harvest("Subject: Services\n\nAcme LLC and José Doe signed.");
        let entity = EntitySpan {
            label: "PERSON".into(),
            text: "José Doe".into(),
            score: 0.91,
            paragraph_index: 1,
            start_char: 13,
            end_char: 21,
            iso: None,
        };
        let ranked = RankedParagraph {
            index: 1,
            text: "Acme LLC and José Doe signed.".into(),
            start_char: 19,
            end_char: 49,
            score: 0.8,
            probe: "parties".into(),
            rank: 1,
        };
        let assembly = assemble_bundle(
            &harvest,
            &[],
            &[],
            &[entity],
            &[ranked],
            &[],
            800,
            2_000,
        );
        assert!(assembly.bundle.contains("EXTRACTED ENTITIES (exact source spans):"));
        assert!(assembly.bundle.contains("RANKED BODY PARAGRAPHS (exact source text):"));
        assert!(assembly.bundle.contains("José Doe"));
        assert!(assembly
            .lanes
            .iter()
            .any(|lane| lane.name == "semantic_entities" && lane.items_emitted == 1));
    }

    #[test]
    fn compression_metrics_are_measurable_and_bounded() {
        let metrics = compression_metrics(&"x".repeat(1_000), &"y".repeat(250));
        assert_eq!(metrics.saved_chars, 750);
        assert_eq!(metrics.source_tokens_approx, 250);
        assert_eq!(metrics.bundle_tokens_approx, 63);
        assert!((metrics.savings_ratio - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn character_budget_never_splits_unicode() {
        let cut = truncate_to_chars("A€B", 2);
        assert_eq!(cut, "A€");
        assert_eq!(cut.chars().count(), 2);
    }
}
