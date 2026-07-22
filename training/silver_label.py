#!/usr/bin/env python3
"""
Build the Ettin bootstrap dataset (slice 4) from a shadow run.

Reads the BackLog ledger (jobs that reached 'emitted' or 'validated') plus the
cached markdown at <cache_dir>/<sha256>.md, and silver-labels three span types:

  DATE    exact regex matches whose ISO form equals the accepted date
          (regex-anchored: these labels are near-perfect)
  SUBJECT the accepted subject's words located in the source text by fuzzy
          window match (noisier; kept only on strong matches)
  PARTY   capitalized name sequences adjacent to legal anchors
          ("between X and Y", "Dear X", "v. X") that also overlap the
          accepted subject or description (noisiest; threshold accordingly)

Output: JSONL of {"tokens": [...], "ner_tags": [...]} in BIO over
{O, B-DATE, I-DATE, B-PARTY, I-PARTY, B-SUBJECT, I-SUBJECT}, split into
train.jsonl / dev.jsonl (95/5 by document hash so dev is truly held out).

Usage:
  python silver_label.py --ledger ~/.../ledger.db --cache ~/.../cache --out data/
"""

import argparse
import json
import random
import re
import sqlite3
from pathlib import Path

LABELS = ["O", "B-DATE", "I-DATE", "B-PARTY", "I-PARTY", "B-SUBJECT", "I-SUBJECT"]
LABEL2ID = {l: i for i, l in enumerate(LABELS)}

TOKEN_RE = re.compile(r"\S+")

DATE_PATTERNS = [
    (re.compile(r"\b(\d{4})-(\d{2})-(\d{2})\b"), lambda m: f"{m[1]}-{m[2]}-{m[3]}"),
    (re.compile(r"(?i)\b(jan(?:uary)?|feb(?:ruary)?|mar(?:ch)?|apr(?:il)?|may|jun(?:e)?|jul(?:y)?|aug(?:ust)?|sep(?:t(?:ember)?)?|oct(?:ober)?|nov(?:ember)?|dec(?:ember)?)\.?\s+(\d{1,2})(?:st|nd|rd|th)?,?\s+(\d{4})\b"),
     lambda m: _month_iso(m[1], m[2], m[3])),
    (re.compile(r"\b(\d{1,2})/(\d{1,2})/(\d{4})\b"), lambda m: f"{m[3]}-{int(m[1]):02d}-{int(m[2]):02d}"),
]

MONTHS = {m: i + 1 for i, m in enumerate(
    ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"])}


def _month_iso(mon, day, year):
    m = MONTHS.get(mon.lower()[:3])
    if not m:
        return None
    return f"{year}-{m:02d}-{int(day):02d}"


PARTY_ANCHOR = re.compile(
    r"(?:between|by and between|dear|attn:|attention:)\s+((?:[A-Z][\w.&'-]+\s*){1,6})"
    r"|((?:[A-Z][\w.&'-]+\s*){1,6})\s*(?:v\.|vs\.)"
    r"|(?:v\.|vs\.)\s*((?:[A-Z][\w.&'-]+\s*){1,6})"
)


def spans_for_doc(text, accepted_date, accepted_subject, description):
    """Return list of (start, end, label) character spans."""
    spans = []

    if accepted_date:
        for pat, to_iso in DATE_PATTERNS:
            for m in pat.finditer(text):
                try:
                    iso = to_iso(m)
                except Exception:
                    iso = None
                if iso == accepted_date:
                    spans.append((m.start(), m.end(), "DATE"))

    if accepted_subject:
        subj_words = [w for w in re.findall(r"\w+", accepted_subject) if len(w) > 2]
        if len(subj_words) >= 2:
            # fuzzy window: find a region containing most subject words in order
            pat = re.compile(
                r"\b" + r"\W{1,15}".join(re.escape(w) for w in subj_words) + r"\b",
                re.IGNORECASE,
            )
            m = pat.search(text)
            if m:
                spans.append((m.start(), m.end(), "SUBJECT"))

    joined_ref = f"{accepted_subject or ''} {description or ''}".lower()
    for m in PARTY_ANCHOR.finditer(text[:6000]):
        cand = next((g for g in m.groups() if g), "").strip()
        if len(cand) >= 4 and cand.lower().split()[0] in joined_ref:
            s = m.start() + m.group(0).find(cand)
            spans.append((s, s + len(cand), "PARTY"))

    # drop overlaps, prefer DATE > SUBJECT > PARTY
    prio = {"DATE": 0, "SUBJECT": 1, "PARTY": 2}
    spans.sort(key=lambda s: (prio[s[2]], s[0]))
    kept = []
    for s in spans:
        if not any(not (s[1] <= k[0] or s[0] >= k[1]) for k in kept):
            kept.append(s)
    return kept


def to_bio(text, spans, max_tokens=384):
    tokens, tags = [], []
    for m in TOKEN_RE.finditer(text):
        if len(tokens) >= max_tokens:
            break
        tok_s, tok_e = m.start(), m.end()
        label = "O"
        for s, e, name in spans:
            if tok_s >= s and tok_e <= e:
                label = ("B-" if tok_s == s or (tags and not tags[-1].endswith(name)) else "I-") + name
                break
            if tok_s < e and tok_e > s:
                label = "I-" + name
                break
        tokens.append(m.group(0))
        tags.append(label)
    # repair: I- without preceding same-type B-
    for i, t in enumerate(tags):
        if t.startswith("I-") and (i == 0 or tags[i - 1] == "O" or tags[i - 1][2:] != t[2:]):
            tags[i] = "B-" + t[2:]
    return tokens, tags


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ledger", required=True)
    ap.add_argument("--cache", required=True)
    ap.add_argument("--out", default="data")
    ap.add_argument("--head-chars", type=int, default=6000)
    args = ap.parse_args()

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(args.ledger)
    rows = conn.execute(
        "SELECT sha256, proposed_date, proposed_subject, description FROM jobs "
        "WHERE state IN ('emitted','validated') AND proposed_subject IS NOT NULL"
    ).fetchall()

    n_written = {"train": 0, "dev": 0}
    files = {
        "train": open(out / "train.jsonl", "w", encoding="utf-8"),
        "dev": open(out / "dev.jsonl", "w", encoding="utf-8"),
    }
    stats = {"DATE": 0, "PARTY": 0, "SUBJECT": 0}

    for sha, date, subject, desc in rows:
        md = Path(args.cache) / f"{sha}.md"
        if not md.exists():
            continue
        text = md.read_text(encoding="utf-8", errors="replace")[: args.head_chars]
        spans = spans_for_doc(text, date, subject, desc)
        if not spans:
            continue
        for _s, _e, name in spans:
            stats[name] += 1
        tokens, tags = to_bio(text, spans)
        if all(t == "O" for t in tags):
            continue
        split = "dev" if int(sha[:8], 16) % 20 == 0 else "train"
        files[split].write(json.dumps({"tokens": tokens, "ner_tags": [LABEL2ID[t] for t in tags]}) + "\n")
        n_written[split] += 1

    for f in files.values():
        f.close()
    (out / "labels.json").write_text(json.dumps(LABELS))
    print(f"train={n_written['train']} dev={n_written['dev']} span counts={stats}")
    if n_written["train"] < 500:
        print("WARNING: under 500 training docs. Run a bigger shadow batch before training.")


if __name__ == "__main__":
    main()
