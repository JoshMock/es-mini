#!/usr/bin/env python3
"""Extract Lucene StandardTokenizer expectations into es-index/tests/data/*.jsonl.

Sources are Lucene's own tests at the pinned tag (Apache-2.0):
    gh api 'repos/apache/lucene/contents/<path>?ref=releases/lucene/10.5.1'
Usage: extract_lucene_analysis_tests.py <dir with the .java files>
Each output line is JSON: [input, [expected tokens]]. Tokenizer only, no lowercasing.
"""
import json, re, sys
from pathlib import Path

LIT = r'"(?:[^"\\]|\\.)*"'


def java_str(lit):
    # Java and JSON string escapes coincide for these files (\\uXXXX, \\n, \\", ...).
    return json.loads(lit)


def asserts(src):
    # Skip testMaxTokenLengthNonDefault: it runs with maxTokenLength=5, which the corpus can't express.
    src = re.sub(r"public void testMaxTokenLengthNonDefault\(\).*?\n  }\n", "", src, flags=re.S)
    pat = re.compile(
        r'assertAnalyzesTo\(\s*(?:BaseTokenStreamTestCase\.)?\w+,\s*(' + LIT + r')\s*,\s*'
        r'new String\[\]\s*\{\s*((?:' + LIT + r'\s*,?\s*)*)\}\s*[,)]'
    )
    for m in pat.finditer(src):
        yield java_str(m.group(1)), [java_str(t) for t in re.findall(LIT, m.group(2))]


def emoji(src):
    # TESTS alternates description, sequence; each sequence must be exactly one token.
    body = src[src.index("String[] TESTS"):]
    body = body[: body.index("};")]
    for lit in re.findall(LIT, body)[1::2]:
        s = java_str(lit)
        yield s, [s]


def write(name, cases):
    out = Path(__file__).parent.parent / "es-index/tests/data" / name
    with out.open("w") as f:
        n = 0
        for case in cases:
            line = json.dumps(case, ensure_ascii=True)
            try:
                line.encode("utf-8").decode("utf-8")
                json.dumps(case, ensure_ascii=False).encode("utf-8")
            except UnicodeEncodeError:
                continue  # lone surrogate: not representable in Rust strings
            f.write(line + "\n")
            n += 1
    print(f"{out.name}: {n} cases")


src = Path(sys.argv[1])
read = lambda f: (src / f).read_text()
write("lucene_standard_analyzer.jsonl", asserts(read("TestStandardAnalyzer.java")))
write("lucene_word_break_12_1.jsonl", asserts(read("WordBreakTestUnicode_12_1_0.java")))
write("lucene_emoji_12_1.jsonl", emoji(read("EmojiTokenizationTestUnicode_12_1.java")))
