"""Run several Sleuth workers as if they were different PCs sharing one office
index, and check what the design promises:

- every readable file under the reading folder is read, by exactly one PC;
- every PC finds the same documents for the same search;
- a worker killed half-way does not lose or duplicate anything.

Nothing here touches the real office index: the store, the files and each
"PC"'s data folder are created under the folders given.

    python scripts/office-sim.py --exe target/release/sleuth-worker.exe \
        --share U:\\some\\scratch\\sim --local .devdata\\sim --pcs 3 --files 300

`--share` must be on a network drive (it holds the store and the files, as
in an office); `--local` holds each PC's own data folder.
"""

import argparse
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import time
import urllib.parse
import zipfile
from pathlib import Path

USER = os.environ.get("USERNAME", "user")


def safe(s):
    return "".join(c if (c.isalnum() or c == "-") else "_" for c in s)


class Pipe:
    """One JSON object per line over the worker's named pipe."""

    def __init__(self, instance):
        self.path = rf"\\.\pipe\Sleuth-{safe(USER + '-' + instance)}"
        self.f = None
        self.next = 1

    def open(self, wait=60):
        end = time.time() + wait
        while True:
            try:
                self.f = open(self.path, "r+b", buffering=0)
                return
            except OSError:
                if time.time() > end:
                    raise
                time.sleep(0.5)

    def call(self, req, until=("ok", "status", "done", "error", "monitor", "settings")):
        if self.f is None:
            self.open()
        rid = self.next
        self.next += 1
        self.f.write((json.dumps({"id": rid, **req}) + "\n").encode("utf-8"))
        out = []
        buf = b""
        while True:
            chunk = self.f.read(65536)
            if not chunk:
                raise OSError("pipe closed")
            buf += chunk
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                ev = json.loads(line.decode("utf-8"))
                if ev.get("id") != rid:
                    continue
                out.append(ev)
                if ev.get("ev") in until:
                    return out

    def close(self):
        if self.f:
            self.f.close()
            self.f = None


def minimal_xlsx(path, rows):
    """A workbook with one sheet of inline strings, written by hand."""
    cells = []
    for r, row in enumerate(rows, 1):
        cs = "".join(
            f'<c r="{chr(65 + c)}{r}" t="inlineStr"><is><t>{v}</t></is></c>' for c, v in enumerate(row)
        )
        cells.append(f'<row r="{r}">{cs}</row>')
    sheet = (
        '<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">'
        f'<sheetData>{"".join(cells)}</sheetData></worksheet>'
    )
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr(
            "[Content_Types].xml",
            '<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
            '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
            '<Default Extension="xml" ContentType="application/xml"/>'
            '<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>'
            '<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>'
            "</Types>",
        )
        z.writestr(
            "_rels/.rels",
            '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
            '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>'
            "</Relationships>",
        )
        z.writestr(
            "xl/workbook.xml",
            '<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" '
            'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets>'
            '<sheet name="Lines" sheetId="1" r:id="rId1"/></sheets></workbook>',
        )
        z.writestr(
            "xl/_rels/workbook.xml.rels",
            '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
            '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>'
            "</Relationships>",
        )
        z.writestr("xl/worksheets/sheet1.xml", sheet)


def minimal_docx(path, paragraphs):
    body = "".join(f"<w:p><w:r><w:t>{p}</w:t></w:r></w:p>" for p in paragraphs)
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr(
            "[Content_Types].xml",
            '<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
            '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
            '<Default Extension="xml" ContentType="application/xml"/>'
            '<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>'
            "</Types>",
        )
        z.writestr(
            "_rels/.rels",
            '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
            '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>'
            "</Relationships>",
        )
        z.writestr(
            "word/document.xml",
            '<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">'
            f"<w:body>{body}</w:body></w:document>",
        )


def make_tree(tree, n, pdf_dir=None):
    """`n` small files of mixed types in a few folders, each carrying its own
    tag (SIM-00042), plus any PDFs copied from `pdf_dir`."""
    kinds = ["txt", "csv", "xlsx", "docx"]
    made = []
    for i in range(n):
        tag = f"SIM-{i:05d}"
        folder = tree / f"J{i % 7:02d}" / f"Lot-{i % 3}"
        folder.mkdir(parents=True, exist_ok=True)
        kind = kinds[i % len(kinds)]
        p = folder / f"doc_{i:05d}.{kind}"
        if kind == "txt":
            p.write_text(f"LINE LIST\n{tag} 4 inch CS line\nPSV-{1000 + i}\n", encoding="utf-8")
        elif kind == "csv":
            p.write_text(f"tag,size,service\n{tag},6,cooling water\nFV-{2000 + i},2,steam\n", encoding="utf-8")
        elif kind == "xlsx":
            minimal_xlsx(p, [["Tag", "Size"], [tag, "8"], [f"LT-{3000 + i}", "1"]])
        else:
            minimal_docx(p, ["Method statement", f"Refer to {tag} and HV-{4000 + i}."])
        made.append((p, tag))
    pdfs = []
    if pdf_dir:
        dest = tree / "PDF"
        dest.mkdir(parents=True, exist_ok=True)
        for src in sorted(Path(pdf_dir).glob("*.pdf")):
            shutil.copy2(src, dest / src.name)
            pdfs.append(dest / src.name)
    # A file no reader handles: found by name only.
    (tree / "notes.zip").write_bytes(b"PK\x05\x06" + b"\0" * 18)
    return made, pdfs


def settings_for(tree, store):
    return {
        "schema_version": 2,
        "content_scopes": [str(tree)],
        "index_roots": [str(tree)],
        "excluded": [],
        "shared_cache_dir": str(store),
        "shared_cache_enabled": True,
        "keep_indexing_when_closed": True,
        "name_rescan_minutes": 15,
        "read_speed": "full",
    }


class Pc:
    def __init__(self, n, args, tree, store):
        self.name = f"SIM-PC-{n}"
        self.instance = f"sim{n}"
        self.data = Path(args.local).resolve() / f"pc{n}"
        self.exe = args.exe
        self.tree, self.store = tree, store
        self.proc = None
        self.pipe = Pipe(self.instance)

    def prepare(self):
        if self.data.exists():
            shutil.rmtree(self.data)
        (self.data / "config").mkdir(parents=True)
        (self.data / "config" / "settings.json").write_text(json.dumps(settings_for(self.tree, self.store)), encoding="utf-8")

    def start(self):
        env = dict(os.environ)
        env.update(
            SLEUTH_DATA_DIR=str(self.data),
            SLEUTH_PC_NAME=self.name,
            SLEUTH_INSTANCE=self.instance,
            SLEUTH_FOLDER_ROOTS="1",
            SLEUTH_LOG="info",
        )
        self.proc = subprocess.Popen([str(self.exe)], env=env)
        self.pipe = Pipe(self.instance)
        self.pipe.open()

    def status(self):
        return self.pipe.call({"req": "status"})[-1]["status"]

    def search(self, q):
        hits = []
        for ev in self.pipe.call({"req": "search", "query": q, "limit": 500}, until=("done", "error")):
            hits += ev.get("hits", [])
        return sorted(h["path"].lower() for h in hits)

    def kill(self):
        self.pipe.close()
        if self.proc:
            subprocess.run(["taskkill", "/F", "/T", "/PID", str(self.proc.pid)], capture_output=True)
            self.proc.wait(timeout=30)
            self.proc = None

    def stop(self):
        try:
            self.pipe.call({"req": "shutdown"})
        except OSError:
            pass
        self.pipe.close()
        if self.proc:
            try:
                self.proc.wait(timeout=60)
            except subprocess.TimeoutExpired:
                self.kill()
            self.proc = None


def readings_in(store):
    """Every reading in every published part: path -> list of (part, state)."""
    seen = {}
    for db in list((store / "text").glob("*.db")) + list((store / "pcs").glob("*/text/*.db")):
        posix = db.as_posix()
        # A network path keeps its two leading slashes after `file:`.
        uri = "file:" + ("//" if posix.startswith("//") else "/") + urllib.parse.quote(posix) + "?mode=ro&immutable=1"
        con = sqlite3.connect(uri, uri=True)
        try:
            for path, st in con.execute("SELECT path, read_state FROM files WHERE read_state IN (2, 3)"):
                seen.setdefault(path.lower(), []).append((db.name, st))
        finally:
            con.close()
    return seen


def wait_until(pcs, done, what, timeout):
    end = time.time() + timeout
    last = ""
    while time.time() < end:
        sts = [p.status() for p in pcs]
        line = " | ".join(
            f"{p.name}: {s['pdfs_read']} read, {s['pdfs_waiting']} waiting, {s['store'].get('unpublished', 0)} unpublished, {s['reader_state']}"
            for p, s in zip(pcs, sts)
        )
        if line != last:
            print("  " + line, flush=True)
            last = line
        if all(done(s) for s in sts):
            return sts
        time.sleep(3)
    raise SystemExit(f"FAILED: {what} did not finish in {timeout} s")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--exe", required=True, type=Path)
    ap.add_argument("--share", required=True, type=Path)
    ap.add_argument("--local", required=True, type=Path)
    ap.add_argument("--pcs", type=int, default=3)
    ap.add_argument("--files", type=int, default=300)
    ap.add_argument("--pdfs", type=Path, help="folder of PDFs to add (e.g. the bench corpus)")
    ap.add_argument("--kill", action="store_true", help="kill one worker half-way and start it again")
    ap.add_argument("--timeout", type=int, default=1800)
    a = ap.parse_args()

    share = a.share.resolve()
    tree, store = share / "tree", share / "store"
    for d in (tree, store):
        if d.exists():
            shutil.rmtree(d)
        d.mkdir(parents=True)
    made, pdfs = make_tree(tree, a.files, a.pdfs)
    readable = len(made) + len(pdfs)
    print(f"{readable} readable files in {tree}")

    pcs = [Pc(i + 1, a, tree, store) for i in range(a.pcs)]
    failures = []
    try:
        t0 = time.time()
        for p in pcs:
            p.prepare()
            p.start()
        if a.kill:
            # Let them get going, then take one down hard and bring it back.
            wait_until(pcs, lambda s: s["pdfs_read"] > readable // 6, "the first readings", a.timeout)
            print(f"killing {pcs[0].name}")
            pcs[0].kill()
            time.sleep(5)
            pcs[0].start()
        sts = wait_until(
            pcs,
            lambda s: s["pdfs_waiting"] == 0 and s["pdfs_read"] + s["pdfs_failed"] >= readable and s["store"].get("unpublished", 0) == 0,
            "reading",
            a.timeout,
        )
        took = time.time() - t0
        print(f"all read after {took:.0f} s")

        # 1. Every readable file read, each exactly once.
        seen = readings_in(store)
        want = {str(p).lower() for p, _ in made} | {str(p).lower() for p in pdfs}
        # Parts store canonical paths (share by address); compare by the tail under the tree.
        tail = lambda s: s.replace("/", "\\").lower().split("\\tree\\", 1)[-1]
        seen_tails = {}
        for path, where in seen.items():
            seen_tails.setdefault(tail(path), []).extend(where)
        missing = sorted(tail(w) for w in want if tail(w) not in seen_tails)
        twice = {k: v for k, v in seen_tails.items() if len(v) > 1}
        if missing:
            failures.append(f"{len(missing)} file(s) never read, e.g. {missing[:3]}")
        if twice:
            failures.append(f"{len(twice)} file(s) read more than once, e.g. {list(twice.items())[:2]}")
        by_pc = {}
        for where in seen_tails.values():
            for part, _ in where:
                pc = part.split("-")[2] if part.startswith("p-") else part
                by_pc[pc] = by_pc.get(pc, 0) + 1
        print("readings by PC:", by_pc)

        # 2. Every PC answers the same.
        for tag in [made[0][1], made[len(made) // 2][1], made[-1][1]]:
            answers = [p.search(tag) for p in pcs]
            if any(x != answers[0] for x in answers) or len(answers[0]) != 1:
                failures.append(f"search {tag}: {[len(x) for x in answers]} hits per PC, expected 1 each and equal")
        names = [p.search("notes.zip") for p in pcs]
        if any(len(x) != 1 for x in names):
            failures.append(f"name search notes.zip: {[len(x) for x in names]} hits per PC, expected 1")

        # 3. Counts agree on every PC.
        for p, s in zip(pcs, sts):
            if s["pdfs_read"] + s["pdfs_failed"] != readable:
                failures.append(f"{p.name} counts {s['pdfs_read']} read + {s['pdfs_failed']} failed, expected {readable}")
            if s["files_indexed"] != readable + 1:
                failures.append(f"{p.name} lists {s['files_indexed']} files, expected {readable + 1}")
    finally:
        for p in pcs:
            p.stop()
    if failures:
        print("FAILED")
        for f in failures:
            print(" -", f)
        sys.exit(1)
    print(f"PASSED: {readable} files, {a.pcs} PCs, each file read once, searches agree")


if __name__ == "__main__":
    main()
