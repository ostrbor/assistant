#!/usr/bin/env python3
"""Helper for the summarize-lesson-cache skill. Run with `python3 -I`.

  cache_tool.py gaps   <cache.txt>             pages where PDF extraction lost symbols
  cache_tool.py backup <cache.txt>             copy the original to ../.cache-orig/ (never overwrites)
  cache_tool.py verify <original> <new>        check that <new> is still a valid cache file

A cache file is "<header line>\\n<page 1>\\f<page 2>\\f...": the assistant app reads everything after
the first line and takes page numbers from the form feeds (\\f).
"""
import re
import shutil
import sys
from pathlib import Path

# Places where a symbol (conjunction, union, ...) was drawn as graphics and is missing from the text.
GAP_PATTERNS = {
    "empty set of connectives {  ,  ,  }": re.compile(r"\{[\s,]*,[\s,]*\}"),
    "empty quotes “ ”": re.compile(r"[“\"']\s+[”\"']"),
    "wide hole between operands ( p   q )": re.compile(r"[\w)\]]\s{2,}[\w(\[¬]"),
    "hole before an opening brace/paren": re.compile(r"[\w)]\s{2,}[({]"),
}
FORMULA_HINT = re.compile(r"[({\[]")


def read_cache(path: Path):
    text = path.read_text(encoding="utf-8")
    header, _, body = text.partition("\n")
    return header, body.split("\f")


def suspicious_lines(page: str) -> list:
    """Lines of a page that look like a symbol was lost."""
    hits = []
    for line in page.split("\n"):
        # Wide spaces matter only inside formulas, not in layout whitespace.
        for label, rx in GAP_PATTERNS.items():
            if rx.search(line) and (label.startswith(("empty", "hole")) or FORMULA_HINT.search(line)):
                hits.append(line.strip())
                break
    return hits


def cmd_gaps(path: Path) -> int:
    header, pages = read_cache(path)
    print(f"{path.name}: {len(pages)} pages, header: {header!r}")
    total = 0
    for no, page in enumerate(pages, 1):
        hits = suspicious_lines(page)
        if hits:
            total += len(hits)
            print(f"  p.{no}: {len(hits)} suspicious lines, e.g. {hits[0][:90]!r}")
    print(f"total suspicious lines: {total} (pages without a mark may still lack a symbol; the page image is the truth)")
    return 0


def cmd_backup(path: Path) -> int:
    dest_dir = path.parent.parent / ".cache-orig"
    dest = dest_dir / path.name
    if dest.exists():
        print(f"backup exists, kept: {dest}")
        return 0
    dest_dir.mkdir(exist_ok=True)
    shutil.copy2(path, dest)
    print(f"backup written: {dest}")
    return 0


def cmd_verify(orig: Path, new: Path) -> int:
    (h0, p0), (h1, p1) = read_cache(orig), read_cache(new)
    errors, warnings = [], []
    if h0 != h1:
        errors.append(f"header changed: {h0!r} -> {h1!r} (the app would treat the cache as stale)")
    if len(p0) != len(p1):
        errors.append(f"page count changed: {len(p0)} -> {len(p1)} (page numbers in hint sources would be wrong)")
    for no, page in enumerate(p1, 1):
        if not page.strip():
            if no <= len(p0) and p0[no - 1].strip():
                errors.append(f"p.{no} is empty (the report would flag the file); write a one-line note instead")
    bad = new.read_text(encoding="utf-8").count("�")
    if bad:
        errors.append(f"{bad} replacement characters (U+FFFD)")
    w0 = sum(len(p.split()) for p in p0)
    w1 = sum(len(p.split()) for p in p1)
    ratio = w1 / w0 if w0 else 0
    print(f"words: {w0} -> {w1} ({ratio:.0%} of the original)")
    if ratio > 0.8:
        warnings.append("barely shortened")
    if ratio < 0.1:
        warnings.append("very short: check that key terms were not lost")
    for no, page in enumerate(p1, 1):
        if len(page.split()) > 400:
            warnings.append(f"p.{no} is long ({len(page.split())} words)")
    left = [(no, line) for no, page in enumerate(p1, 1) for line in suspicious_lines(page)]
    print(f"lines that still look like a symbol is missing: {len(left)}")
    for no, line in left[:5]:
        print(f"  p.{no}: {line[:90]!r}")
    for w in warnings:
        print("warning:", w)
    for e in errors:
        print("ERROR:", e)
    print("OK" if not errors else "FAILED")
    return 1 if errors else 0


def main() -> int:
    a = sys.argv[1:]
    try:
        if len(a) == 2 and a[0] == "gaps":
            return cmd_gaps(Path(a[1]))
        if len(a) == 2 and a[0] == "backup":
            return cmd_backup(Path(a[1]))
        if len(a) == 3 and a[0] == "verify":
            return cmd_verify(Path(a[1]), Path(a[2]))
    except (OSError, UnicodeDecodeError) as e:
        print(f"error: {e}")
        return 2
    print(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main())
