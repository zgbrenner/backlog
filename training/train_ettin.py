#!/usr/bin/env python3
"""
Slice 4: fine-tune jhu-clsp/ettin-encoder-32m as the BackLog span proposer.

Input: train.jsonl / dev.jsonl / labels.json from silver_label.py.
Output: a HF token-classification model directory to point BACKLOG_ETTIN_DIR
(and the app config ettin_model_dir) at.

Ship gate (printed at the end): per-label F1 on the held-out dev split.
Policy per the design doc: DATE F1 >= 0.90 to ship at all; PARTY/SUBJECT
each need >= 0.75 or you ship date-only by editing SHIP_LABELS.

Usage:
  python train_ettin.py --data data/ --base jhu-clsp/ettin-encoder-32m --out ettin-backlog-v1
"""

import argparse
import json
from pathlib import Path

import numpy as np


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", required=True)
    ap.add_argument("--base", default="jhu-clsp/ettin-encoder-32m")
    ap.add_argument("--out", default="ettin-backlog-v1")
    ap.add_argument("--epochs", type=float, default=4)
    ap.add_argument("--lr", type=float, default=5e-5)
    ap.add_argument("--batch", type=int, default=16)
    args = ap.parse_args()

    from datasets import load_dataset
    from transformers import (
        AutoModelForTokenClassification,
        AutoTokenizer,
        DataCollatorForTokenClassification,
        Trainer,
        TrainingArguments,
    )

    data_dir = Path(args.data)
    labels = json.loads((data_dir / "labels.json").read_text())
    id2label = dict(enumerate(labels))
    label2id = {v: k for k, v in id2label.items()}

    ds = load_dataset(
        "json",
        data_files={"train": str(data_dir / "train.jsonl"), "dev": str(data_dir / "dev.jsonl")},
    )

    tokenizer = AutoTokenizer.from_pretrained(args.base)

    def tokenize(batch):
        enc = tokenizer(
            batch["tokens"],
            is_split_into_words=True,
            truncation=True,
            max_length=512,
        )
        all_labels = []
        for i, word_labels in enumerate(batch["ner_tags"]):
            word_ids = enc.word_ids(batch_index=i)
            prev = None
            ids = []
            for wid in word_ids:
                if wid is None:
                    ids.append(-100)
                elif wid != prev:
                    ids.append(word_labels[wid])
                else:
                    # inside a word: keep I- of the same type, never B-
                    lab = labels[word_labels[wid]]
                    ids.append(label2id["I-" + lab[2:]] if lab != "O" else label2id["O"])
                prev = wid
            all_labels.append(ids)
        enc["labels"] = all_labels
        return enc

    ds = ds.map(tokenize, batched=True, remove_columns=ds["train"].column_names)

    model = AutoModelForTokenClassification.from_pretrained(
        args.base, num_labels=len(labels), id2label=id2label, label2id=label2id
    )

    def compute_metrics(pred):
        logits, gold = pred
        preds = np.argmax(logits, axis=-1)
        # entity-type F1 at the token level (span-level is stricter; token
        # level is fine for a ship gate this coarse)
        out = {}
        for ent in ("DATE", "PARTY", "SUBJECT"):
            ent_ids = {label2id[f"B-{ent}"], label2id[f"I-{ent}"]}
            tp = fp = fn = 0
            for p_row, g_row in zip(preds, gold):
                for p, g in zip(p_row, g_row):
                    if g == -100:
                        continue
                    pi, gi = p in ent_ids, g in ent_ids
                    tp += pi and gi
                    fp += pi and not gi
                    fn += gi and not pi
            prec = tp / (tp + fp) if tp + fp else 0.0
            rec = tp / (tp + fn) if tp + fn else 0.0
            out[f"{ent}_f1"] = 2 * prec * rec / (prec + rec) if prec + rec else 0.0
        return out

    targs = TrainingArguments(
        output_dir=args.out + "-ckpt",
        learning_rate=args.lr,
        num_train_epochs=args.epochs,
        per_device_train_batch_size=args.batch,
        per_device_eval_batch_size=args.batch,
        eval_strategy="epoch",
        save_strategy="epoch",
        load_best_model_at_end=True,
        metric_for_best_model="DATE_f1",
        logging_steps=50,
        report_to=[],
    )

    trainer = Trainer(
        model=model,
        args=targs,
        train_dataset=ds["train"],
        eval_dataset=ds["dev"],
        data_collator=DataCollatorForTokenClassification(tokenizer),
        compute_metrics=compute_metrics,
    )
    trainer.train()
    metrics = trainer.evaluate()
    print("\n=== SHIP GATE (held-out dev) ===")
    for k in ("eval_DATE_f1", "eval_PARTY_f1", "eval_SUBJECT_f1"):
        print(f"  {k}: {metrics.get(k, 0.0):.3f}")
    print("Policy: DATE >= 0.90 required. PARTY/SUBJECT >= 0.75 each, else ship date-only.")

    trainer.save_model(args.out)
    tokenizer.save_pretrained(args.out)
    print(f"\nSaved to {args.out}. Point ettin_model_dir (app config) and BACKLOG_ETTIN_DIR at it.")


if __name__ == "__main__":
    main()
