"""Compare two `bench --dump` files line by line: what a candidate OCR
configuration lost, changed or gained against a baseline.

    python bench/compare_dumps.py baseline.json candidate.json [--list lost.json] [--show 40]

Lines are compared in the matcher's own normal form (letters and digits only,
lower case), page by page. A baseline line that the candidate does not have in
the same form is classed by what the candidate read at the same place:

- contained  a candidate line on the same spot holds the whole baseline text
             (two lines read as one, or more of the line read)
- duplicate  the candidate has the same text elsewhere on the page and the
             baseline had it more than once (a second copy from a tile overlap)
- changed    the candidate read something else at the same place
- lost       the candidate read nothing at that place

`gained` is the same seen from the candidate's side. `--list` writes the lost
and changed lines with their boxes, to look at the pixels and decide which
reading was right: there is no ground truth on real drawings.
"""
import argparse
import collections
import json
import sys


def sep(s):
    return "".join(c for c in s if c.isalnum()).lower()


def overlap(a, b):
    """Intersection over the smaller box; boxes are [x, y, w, h]."""
    w = min(a[0] + a[2], b[0] + b[2]) - max(a[0], b[0])
    h = min(a[1] + a[3], b[1] + b[3]) - max(a[1], b[1])
    if w <= 0 or h <= 0:
        return 0.0
    small = min(a[2] * a[3], b[2] * b[3])
    return w * h / small if small > 0 else 0.0


def taglike(s):
    return len(s) >= 4 and any(c.isdigit() for c in s) and any(c.isalpha() for c in s)


def classify(mine, theirs):
    """Class of every line in `mine` against the lines in `theirs`."""
    their_forms = collections.Counter(sep(t["t"]) for t in theirs)
    my_forms = collections.Counter(sep(m["t"]) for m in mine)
    out = []
    for m in mine:
        f = sep(m["t"])
        if not f:
            continue
        if their_forms.get(f):
            # Same text on the page; fewer copies is the overlap duplicate going away.
            if my_forms[f] > their_forms[f] and not any(sep(t["t"]) == f and overlap(m["b"], t["b"]) >= 0.3 for t in theirs):
                out.append(("duplicate", m, None))
            else:
                out.append(("same", m, None))
            continue
        near = [t for t in theirs if overlap(m["b"], t["b"]) >= 0.3]
        holder = [t for t in near if f in sep(t["t"])]
        if holder:
            out.append(("contained", m, holder[0]))
        elif near:
            out.append(("changed", m, max(near, key=lambda t: overlap(m["b"], t["b"]))))
        else:
            out.append(("lost", m, None))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("baseline")
    ap.add_argument("candidate")
    ap.add_argument("--list", help="write lost and changed lines here as JSON")
    ap.add_argument("--show", type=int, default=0, help="print this many lost/changed lines")
    a = ap.parse_args()
    # OCR reads the odd character no Windows console code page can print.
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    base = json.load(open(a.baseline, encoding="utf-8"))
    cand = json.load(open(a.candidate, encoding="utf-8"))
    tot = collections.Counter()
    tag = collections.Counter()
    listed = []
    for key in sorted(base):
        b = [l for l in base[key] if l.get("src", "Ocr") == "Ocr"]
        c = [l for l in cand.get(key, []) if l.get("src", "Ocr") == "Ocr"]
        tot["baseline lines"] += len(b)
        tot["candidate lines"] += len(c)
        for cls, m, other in classify(b, c):
            tot[cls] += 1
            if taglike(sep(m["t"])):
                tag[cls] += 1
            if cls in ("lost", "changed"):
                listed.append({"page": key, "class": cls, "text": m["t"], "conf": m["c"], "alt": m.get("alt", False), "box": m["b"], "candidate": other["t"] if other else None, "candidate_conf": other["c"] if other else None})
        for cls, m, other in classify(c, b):
            if cls in ("lost", "changed", "contained"):
                name = {"lost": "gained", "changed": "changed (candidate side)", "contained": "gained (part of a baseline line)"}[cls]
                tot[name] += 1
                if taglike(sep(m["t"])):
                    tag[name] += 1
                if cls == "lost":
                    listed.append({"page": key, "class": "gained", "text": m["t"], "conf": m["c"], "alt": m.get("alt", False), "box": m["b"], "candidate": None, "candidate_conf": None})
    print(f"pages: {len(base)}")
    for k in ["baseline lines", "candidate lines", "same", "duplicate", "contained", "changed", "lost", "gained", "gained (part of a baseline line)", "changed (candidate side)"]:
        print(f"{k:34s} {tot[k]:6d}   tag-like {tag[k]:6d}" if k not in ("baseline lines", "candidate lines") else f"{k:34s} {tot[k]:6d}")
    if a.list:
        with open(a.list, "w", encoding="utf-8") as f:
            json.dump(listed, f, indent=1, ensure_ascii=False)
    for item in listed[: a.show]:
        print(f"  {item['class']:8s} p{item['page']} conf {item['conf']:.2f} {item['text']!r} -> {item['candidate']!r}")


if __name__ == "__main__":
    main()
