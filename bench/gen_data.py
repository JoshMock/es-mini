#!/usr/bin/env python3
"""Writes a deterministic bulk body: gen_data.py DOCS > data.ndjson"""
import json, random, sys

rng = random.Random(42)
words = [f"w{i}" for i in range(2000)] + ["fox", "dog", "quick", "brown", "lazy"]
tags = [f"tag{i}" for i in range(50)]
for i in range(int(sys.argv[1])):
    doc = {
        "title": " ".join(rng.choice(words) for _ in range(rng.randint(3, 12))),
        "tag": rng.choice(tags),
        "views": rng.randint(0, 100_000),
        "rating": round(rng.uniform(0, 5), 2),
        "published": rng.random() < 0.7,
        "date": f"2024-{rng.randint(1, 12):02d}-{rng.randint(1, 28):02d}",
    }
    print(json.dumps({"index": {"_id": str(i)}}))
    print(json.dumps(doc))
