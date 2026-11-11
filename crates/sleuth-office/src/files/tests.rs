use super::*;
use crate::testdir::scratch;
use std::time::SystemTime;

const ROOT: &str = r"\\10.0.0.5\project";

fn listing(dir: &str, pass: i64, at: i64, subdirs: &[&str], files: &[(&str, u64, i64)]) -> Listing {
    Listing { dir: dir.into(), pass, listed_at: at, subdirs: subdirs.iter().map(|s| s.to_string()).collect(), files: files.iter().map(|(n, s, m)| (n.to_string(), *s, *m)).collect() }
}

/// Files (path, size, time) and folders (path, listed, pass, listed at).
type Dump = (Vec<(String, u64, i64)>, Vec<(String, bool, i64, i64)>);

/// Everything a catalog holds for the root: files, and folders with their
/// listed state.
fn dump(c: &Catalog) -> Dump {
    let mut files = Vec::new();
    c.for_each_under(ROOT, &mut |dir, name, size, mtime| {
        files.push((format!("{dir}\\{name}"), size, mtime));
        true
    });
    files.sort();
    let mut dirs = Vec::new();
    if let Some(snap) = c.snapshot_root(ROOT) {
        snap.walk(&mut |i, path| {
            let d = &snap.dirs[i];
            dirs.push((path.to_string(), d.listed, d.pass, d.listed_at));
            true
        });
    }
    dirs.sort();
    (files, dirs)
}

/// Apply to the catalog and hand to the writer, as the walking PC does.
fn publish(c: &Catalog, w: &mut SegmentWriter, l: Listing) {
    c.apply(&l);
    w.add(&l);
}

fn names(d: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(d).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

fn age(path: &Path, secs: u64) {
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(secs)).unwrap();
}

#[test]
fn file_names() {
    let e = parse_name("full-7-00000012.cat").unwrap();
    assert_eq!((e.full, e.mark), (true, Mark { pass: 7, seq: 12 }));
    let e = parse_name("seg-1730000000-00000003-DESKTOP-AB12.cat").unwrap();
    assert_eq!((e.full, e.mark, e.pc.as_str()), (false, Mark { pass: 1730000000, seq: 3 }, "DESKTOP-AB12"));
    assert_eq!(parse_name("seg--5-00000003-PC.cat").unwrap().mark, Mark { pass: -5, seq: 3 });
    assert_eq!(parse_name("full--5-00000003.cat").unwrap().mark, Mark { pass: -5, seq: 3 });
    assert_eq!(parse_name("seg-7-123456789-PC.cat").unwrap().mark.seq, 123456789);
    for bad in ["state.json", "full-7-00000012.cat.PC.1.tmp", "seg-7-00000003.cat", "seg-x-1-PC.cat", "full-7.cat", "other.cat", "seg-7-00000003-PC.cat.99.tmp"] {
        assert!(parse_name(bad).is_none(), "{bad}");
    }
    assert!(Mark { pass: 7, seq: 2 } < Mark { pass: 7, seq: 10 } && Mark { pass: 7, seq: 99 } < Mark { pass: 8, seq: 1 });
}

#[test]
fn an_empty_folder_loads_as_nothing() {
    let d = scratch("cat-empty");
    let f = RootFiles::new(d.join("not").join("there"), ROOT);
    let c = Catalog::new();
    assert_eq!(f.load(&c).unwrap(), Mark::default());
    assert_eq!(c.roots(), vec![ROOT]);
    let mut m = Mark::default();
    assert_eq!(f.follow(&c, &mut m).unwrap(), 0);
    assert_eq!(m, Mark::default());
    assert!(f.state().is_none());
    assert!(f.writer("PC-A", 1).unwrap().flush().unwrap().is_none());
    assert!(!d.exists(), "nothing was written");
}

#[test]
fn segments_and_a_full_catalog_round_trip() {
    let d = scratch("cat-round");
    let (a, files) = (Catalog::new(), RootFiles::new(&d, ROOT));
    a.add_root(ROOT);
    let mut w = files.writer("PC-A", 5).unwrap();
    assert_eq!(w.pending(), 0);
    publish(&a, &mut w, listing(ROOT, 5, 100, &["J100", "Empty", "Ünïcode"], &[("readme.txt", 10, -5), ("big.zip", u64::MAX, i64::MAX)]));
    publish(&a, &mut w, listing(&format!(r"{ROOT}\J100"), 5, 101, &["Rev-1"], &[("14-01-15-2632_PSV-2809-01_REV-B.pdf", 123456, 1_700_000_000)]));
    assert_eq!(w.pending(), 2);
    assert_eq!(w.flush().unwrap(), Some(Mark { pass: 5, seq: 1 }));
    assert_eq!((w.pending(), w.flush().unwrap()), (0, None));
    publish(&a, &mut w, listing(&format!(r"{ROOT}\J100\Rev-1\"), 5, 102, &[], &[("a.dwg", 1, 1), ("b.dwg", 2, 2)]));
    publish(&a, &mut w, listing(&format!(r"{ROOT}\Ünïcode"), 5, 103, &[], &[("Обычный.pmvx", 3, 3)]));
    // A folder below one that was never listed, and a listing outside the root (dropped).
    publish(&a, &mut w, listing(&format!(r"{ROOT}\Deep\Er\Still"), 0, 104, &["x"], &[("deep.pdf", 4, 4)]));
    publish(&a, &mut w, listing(r"\\10.0.0.5\other", 5, 104, &[], &[("no.pdf", 1, 1)]));
    assert_eq!(w.flush().unwrap(), Some(Mark { pass: 5, seq: 2 }));
    assert_eq!(names(&d), ["seg-5-00000001-PC-A.cat", "seg-5-00000002-PC-A.cat"]);

    // Another PC: segments only.
    let b = Catalog::new();
    let fb = RootFiles::new(&d, &format!(r"{ROOT}\"));
    assert_eq!(fb.load(&b).unwrap(), Mark { pass: 5, seq: 2 });
    assert_eq!(dump(&b), dump(&a));
    assert_eq!((b.dir_count(), b.file_count()), (5, 7));
    assert!(b.contains_dir(&format!(r"{ROOT}\Empty")) && !b.dir_listed(&format!(r"{ROOT}\Empty")) && b.contains_dir(&format!(r"{ROOT}\Deep\Er\Still\x")));
    assert_eq!(b.listed_in_pass(&format!(r"{ROOT}\J100"), 5), Some(vec![format!(r"{ROOT}\J100\Rev-1")]));

    // The full catalog, then one more segment after it.
    files.write_full(&a, Mark { pass: 5, seq: 2 }).unwrap();
    publish(&a, &mut w, listing(&format!(r"{ROOT}\J100"), 5, 110, &[], &[("only.pdf", 9, 9)]));
    assert_eq!(w.flush().unwrap(), Some(Mark { pass: 5, seq: 3 }));
    assert_eq!(names(&d), ["full-5-00000002.cat", "seg-5-00000001-PC-A.cat", "seg-5-00000002-PC-A.cat", "seg-5-00000003-PC-A.cat"]);
    let c = Catalog::new();
    assert_eq!(RootFiles::new(&d, ROOT).load(&c).unwrap(), Mark { pass: 5, seq: 3 });
    assert_eq!(dump(&c), dump(&a));
    assert_eq!(c.get(&format!(r"{ROOT}\big.zip")).map(|f| (f.size, f.mtime)), Some((u64::MAX, i64::MAX)));
    assert!(!c.contains_dir(&format!(r"{ROOT}\J100\Rev-1")));

    // The PC that loaded segments only follows to the same place without a reload.
    let mut mark = Mark { pass: 5, seq: 2 };
    let g = b.generation();
    assert_eq!(fb.follow(&b, &mut mark).unwrap(), 1);
    assert_eq!(mark, Mark { pass: 5, seq: 3 });
    assert_eq!(dump(&b), dump(&a));
    assert!(b.generation() > g);
    assert_eq!(fb.follow(&b, &mut mark).unwrap(), 0);

    // The header is one line of JSON anyone can read.
    let mut text = Vec::new();
    GzDecoder::new(std::fs::File::open(d.join("full-5-00000002.cat")).unwrap()).read_to_end(&mut text).unwrap();
    let line = &text[..text.iter().position(|&b| b == b'\n').unwrap()];
    let v: serde_json::Value = serde_json::from_slice(line).unwrap();
    assert_eq!((v["format"].as_u64(), v["root"].as_str(), v["pass"].as_i64(), v["seq"].as_u64(), v["dirs"].as_u64(), v["files"].as_u64()), (Some(1), Some(ROOT), Some(5), Some(2), Some(5), Some(7)));
    assert!(v["created"].as_i64().unwrap() > 0 && v["by"].is_string());
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn a_newer_full_catalog_is_reloaded() {
    let d = scratch("cat-follow");
    let (a, files) = (Catalog::new(), RootFiles::new(&d, ROOT));
    a.add_root(ROOT);
    let mut w = files.writer("PC-A", 1).unwrap();
    publish(&a, &mut w, listing(ROOT, 1, 100, &["A", "B"], &[("root.pdf", 1, 1)]));
    publish(&a, &mut w, listing(&format!(r"{ROOT}\A"), 1, 100, &[], &[("a.pdf", 1, 1)]));
    w.flush().unwrap();

    let (b, fb) = (Catalog::new(), RootFiles::new(&d, ROOT));
    let mut mark = fb.load(&b).unwrap();
    assert_eq!(mark, Mark { pass: 1, seq: 1 });
    // The follower has something of its own that the shared catalog does not.
    b.apply(&listing(&format!(r"{ROOT}\B"), 0, 500, &[], &[("mine.pdf", 1, 1)]));

    // The walker goes on; its next pass's segments are lost to the follower, the full catalog is not.
    a.apply(&listing(ROOT, 2, 200, &["A"], &[("root.pdf", 2, 2)]));
    a.apply(&listing(&format!(r"{ROOT}\A"), 2, 200, &[], &[("a.pdf", 1, 1), ("a2.pdf", 1, 1)]));
    files.write_full(&a, Mark { pass: 2, seq: 40 }).unwrap();
    let mut w = files.writer("PC-A", 2).unwrap();
    publish(&a, &mut w, listing(&format!(r"{ROOT}\A"), 2, 210, &[], &[("a2.pdf", 1, 1)]));
    assert_eq!(w.flush().unwrap(), Some(Mark { pass: 2, seq: 41 }), "numbering continues after the full catalog");

    let g = b.generation();
    assert_eq!(fb.follow(&b, &mut mark).unwrap(), 3, "two folders of the full catalog and one of the segment");
    assert_eq!(mark, Mark { pass: 2, seq: 41 });
    assert_eq!(dump(&b), dump(&a));
    assert!(!b.contains_dir(&format!(r"{ROOT}\B")) && b.get(&format!(r"{ROOT}\B\mine.pdf")).is_none(), "the root was replaced, not merged");
    assert!(b.generation() > g);
    assert_eq!(fb.follow(&b, &mut mark).unwrap(), 0);

    // An older full catalog than the mark is of no interest.
    files.write_full(&a, Mark { pass: 2, seq: 41 }).unwrap();
    let g = b.generation();
    assert_eq!(fb.follow(&b, &mut mark).unwrap(), 0);
    assert_eq!(b.generation(), g);
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn damaged_files_are_skipped() {
    let d = scratch("cat-damaged");
    let (a, files) = (Catalog::new(), RootFiles::new(&d, ROOT));
    a.add_root(ROOT);
    let mut w = files.writer("PC-A", 3).unwrap();
    for (i, name) in ["one", "two", "three", "four", "five", "six"].iter().enumerate() {
        publish(&a, &mut w, listing(&format!(r"{ROOT}\{name}"), 3, 100 + i as i64, &[], &[(&format!("{name}.pdf"), 1, 1)]));
        w.flush().unwrap();
    }
    let seg = |n: u32| d.join(format!("seg-3-{n:08}-PC-A.cat"));
    // 2: cut short. 3: not gzip. 4: a format from the future. 5: another root's.
    let whole = std::fs::read(seg(2)).unwrap();
    std::fs::write(seg(2), &whole[..whole.len() - 6]).unwrap();
    std::fs::write(seg(3), b"this is not a catalog").unwrap();
    let rewrite = |n: u32, head: Header| {
        let mut gz = create(&seg(n), &head).unwrap();
        let mut body = Vec::new();
        begin_record(&mut body, "x", 3, 1);
        put_varint(&mut body, 0);
        put_varint(&mut body, 0);
        put_record(&mut gz, &body).unwrap();
        finish(gz).unwrap();
    };
    rewrite(4, Header { format: 2, root: ROOT.into(), pass: 3, seq: 4, dirs: 1, ..Default::default() });
    rewrite(5, Header { format: FORMAT, root: r"\\10.0.0.5\other".into(), pass: 3, seq: 5, dirs: 1, ..Default::default() });
    // 7: says more than it holds. 8: bytes flipped inside.
    rewrite(7, Header { format: FORMAT, root: ROOT.into(), pass: 3, seq: 7, dirs: 2, ..Default::default() });
    let mut bad = std::fs::read(seg(6)).unwrap();
    let mid = bad.len() / 2;
    bad[mid] ^= 0x55;
    bad[mid + 1] ^= 0x55;
    std::fs::write(seg(8), &bad).unwrap();
    std::fs::write(d.join("seg-3-00000009-PC-A.cat"), b"").unwrap();

    let b = Catalog::new();
    let fb = RootFiles::new(&d, ROOT);
    let mark = fb.load(&b).unwrap();
    assert_eq!(mark, Mark { pass: 3, seq: 9 }, "the bad ones are passed, not waited for");
    let have: Vec<String> = dump(&b).0.into_iter().map(|f| f.0).collect();
    assert_eq!(have, [format!(r"{ROOT}\one\one.pdf"), format!(r"{ROOT}\six\six.pdf")]);
    assert!(!b.contains_dir(&format!(r"{ROOT}\x")));

    // A damaged full catalog: the one before it is used, with the segments after that.
    files.write_full(&a, Mark { pass: 3, seq: 6 }).unwrap();
    a.apply(&listing(&format!(r"{ROOT}\seven"), 3, 200, &[], &[("seven.pdf", 1, 1)]));
    files.write_full(&a, Mark { pass: 3, seq: 20 }).unwrap();
    let full = d.join("full-3-00000020.cat");
    let whole = std::fs::read(&full).unwrap();
    std::fs::write(&full, &whole[..whole.len() / 2]).unwrap();
    let c = Catalog::new();
    assert_eq!(RootFiles::new(&d, ROOT).load(&c).unwrap(), Mark { pass: 3, seq: 9 });
    assert_eq!(dump(&c).0.len(), 6);
    assert!(c.get(&format!(r"{ROOT}\seven\seven.pdf")).is_none());
    // Following does not fail on it either, and what was loaded stays.
    let mut m = mark;
    assert_eq!(fb.follow(&b, &mut m).unwrap(), 0);
    assert_eq!(dump(&b).0.len(), 2);
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn a_file_that_cannot_be_read_is_not_skipped() {
    use std::io::{Error, ErrorKind};
    assert!(is_damage(&anyhow!("damaged record")));
    assert!(is_damage(&anyhow::Error::from(Error::new(ErrorKind::UnexpectedEof, "cut short")).context("reading")));
    assert!(is_damage(&anyhow::Error::from(Error::new(ErrorKind::InvalidInput, "invalid gzip header"))));
    assert!(!is_damage(&anyhow::Error::from(Error::new(ErrorKind::TimedOut, "the share is away")).context("reading")));
    assert!(!is_damage(&anyhow::Error::from(Error::new(ErrorKind::PermissionDenied, "no"))));

    let d = scratch("cat-away");
    let (a, files) = (Catalog::new(), RootFiles::new(&d, ROOT));
    a.add_root(ROOT);
    let mut w = files.writer("PC-A", 3).unwrap();
    publish(&a, &mut w, listing(&format!(r"{ROOT}\one"), 3, 100, &[], &[("one.pdf", 1, 1)]));
    w.flush().unwrap();
    // The second segment cannot be opened (a folder stands in for a share that fails).
    let blocked = d.join("seg-3-00000002-PC-A.cat");
    std::fs::create_dir(&blocked).unwrap();
    let (b, fb) = (Catalog::new(), RootFiles::new(&d, ROOT));
    assert!(fb.load(&b).is_err(), "the caller hears of it");
    let mut mark = Mark::default();
    assert!(fb.follow(&b, &mut mark).is_err());
    assert_eq!(mark, Mark { pass: 3, seq: 1 }, "what could be read was applied");
    assert!(b.get(&format!(r"{ROOT}\one\one.pdf")).is_some());
    // Readable again: followed from where it stopped, nothing lost.
    std::fs::remove_dir(&blocked).unwrap();
    publish(&a, &mut w, listing(&format!(r"{ROOT}\two"), 3, 101, &[], &[("two.pdf", 1, 1)]));
    assert_eq!(w.flush().unwrap(), Some(Mark { pass: 3, seq: 2 }));
    assert_eq!(fb.follow(&b, &mut mark).unwrap(), 1);
    assert_eq!(mark, Mark { pass: 3, seq: 2 });
    assert_eq!(dump(&b), dump(&a));
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn two_writers_interleave() {
    let d = scratch("cat-two");
    let files = RootFiles::new(&d, ROOT);
    let (a, follower) = (Catalog::new(), Catalog::new());
    a.add_root(ROOT);
    let fb = RootFiles::new(&d, ROOT);
    let mut mark = fb.load(&follower).unwrap();

    // PC-A walks, loses the lease to PC-B, and both publish for a while.
    let mut wa = files.writer("PC-A", 9).unwrap();
    let mut wb = files.writer("PC-B", 9).unwrap();
    publish(&a, &mut wa, listing(ROOT, 9, 100, &["x", "y"], &[("r.pdf", 1, 1)]));
    assert_eq!(wa.flush().unwrap(), Some(Mark { pass: 9, seq: 1 }));
    publish(&a, &mut wb, listing(&format!(r"{ROOT}\x"), 9, 101, &[], &[("x1.pdf", 1, 1)]));
    assert_eq!(wb.flush().unwrap(), Some(Mark { pass: 9, seq: 2 }), "never a number already taken");
    publish(&a, &mut wa, listing(&format!(r"{ROOT}\y"), 9, 102, &[], &[("y1.pdf", 1, 1)]));
    assert_eq!(wa.flush().unwrap(), Some(Mark { pass: 9, seq: 3 }));
    assert_eq!(names(&d), ["seg-9-00000001-PC-A.cat", "seg-9-00000002-PC-B.cat", "seg-9-00000003-PC-A.cat"]);
    assert_eq!(fb.follow(&follower, &mut mark).unwrap(), 3);
    assert_eq!(mark, Mark { pass: 9, seq: 3 });
    assert_eq!(dump(&follower), dump(&a));

    // Both publish the same number at the same moment; one arrives after the follower looked.
    publish(&a, &mut wa, listing(&format!(r"{ROOT}\x"), 9, 110, &[], &[("x1.pdf", 1, 1), ("x2.pdf", 1, 1)]));
    wa.flush().unwrap();
    publish(&a, &mut wb, listing(&format!(r"{ROOT}\y"), 9, 111, &[], &[("y2.pdf", 1, 1)]));
    wb.flush().unwrap();
    let late = d.join("seg-9-00000004-PC-B.cat");
    std::fs::rename(d.join("seg-9-00000005-PC-B.cat"), d.join("held-back")).unwrap();
    assert_eq!(fb.follow(&follower, &mut mark).unwrap(), 1);
    assert_eq!(mark, Mark { pass: 9, seq: 4 });
    // An older listing arriving late does not undo a newer one.
    publish(&a, &mut wa, listing(&format!(r"{ROOT}\y"), 9, 120, &[], &[("y3.pdf", 1, 1)]));
    assert_eq!(wa.flush().unwrap(), Some(Mark { pass: 9, seq: 5 }));
    assert_eq!(fb.follow(&follower, &mut mark).unwrap(), 1);
    std::fs::rename(d.join("held-back"), &late).unwrap();
    assert_eq!(fb.follow(&follower, &mut mark).unwrap(), 1, "the late file is still read");
    assert_eq!(mark, Mark { pass: 9, seq: 5 });
    assert_eq!(dump(&follower), dump(&a));
    assert_eq!(fb.follow(&follower, &mut mark).unwrap(), 0, "and only once");

    // A PC starting now reads them all in order and ends in the same place.
    let fresh = Catalog::new();
    assert_eq!(RootFiles::new(&d, ROOT).load(&fresh).unwrap(), Mark { pass: 9, seq: 5 });
    assert_eq!(dump(&fresh), dump(&a));
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn a_full_catalog_clears_what_it_replaces_after_an_hour() {
    let d = scratch("cat-sweep");
    let (a, files) = (Catalog::new(), RootFiles::new(&d, ROOT));
    a.add_root(ROOT);
    let mut w = files.writer("PC-A", 1).unwrap();
    for i in 0..4 {
        publish(&a, &mut w, listing(ROOT, 1, 100 + i, &[], &[("f.pdf", i as u64, 1)]));
        w.flush().unwrap();
    }
    files.write_full(&a, Mark { pass: 1, seq: 2 }).unwrap();
    files.set_state(&RootState { root: ROOT.into(), ..Default::default() }).unwrap();
    std::fs::write(d.join("full-1-00000004.cat.PC-X.7.tmp"), b"left by a crash").unwrap();
    std::fs::write(d.join("fresh.tmp"), b"being written").unwrap();
    for old in ["seg-1-00000001-PC-A.cat", "seg-1-00000003-PC-A.cat", "seg-1-00000004-PC-A.cat", "full-1-00000002.cat", "full-1-00000004.cat.PC-X.7.tmp", "state.json"] {
        age(&d.join(old), 2 * 3600);
    }
    files.write_full(&a, Mark { pass: 1, seq: 3 }).unwrap();
    // Old and replaced: segments 1 and 3, the earlier full catalog, the crash's file.
    // Kept: segment 2 (replaced but recent), segment 4 (old but not replaced), the state.
    assert_eq!(names(&d), ["fresh.tmp", "full-1-00000003.cat", "seg-1-00000002-PC-A.cat", "seg-1-00000004-PC-A.cat", "state.json"]);
    let c = Catalog::new();
    assert_eq!(RootFiles::new(&d, ROOT).load(&c).unwrap(), Mark { pass: 1, seq: 4 });
    assert_eq!(dump(&c), dump(&a));
    // Writing the same mark again replaces the file.
    files.write_full(&a, Mark { pass: 1, seq: 3 }).unwrap();
    assert_eq!(names(&d).len(), 5);
    assert!(RootFiles::new(&d, r"\\10.0.0.5\elsewhere").write_full(&a, Mark::default()).is_err(), "a root the catalog does not hold");
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn state_round_trips() {
    let d = scratch("cat-state");
    let files = RootFiles::new(&d, ROOT);
    assert!(files.state().is_none());
    let s = RootState { root: ROOT.into(), pass: 12, phase: "walking".into(), by: "PC-A".into(), started: 1, updated: 2, last_ok: 3, folders: 4, files: 5, skipped: 6, note: "Ünïcode \"note\"".into() };
    files.set_state(&s).unwrap();
    assert_eq!(files.state(), Some(s.clone()));
    files.set_state(&RootState { phase: "done".into(), ..s.clone() }).unwrap();
    assert_eq!(files.state().unwrap().phase, "done");
    assert_eq!(names(&d), ["state.json"]);
    // A state written by a version with fewer fields still reads.
    std::fs::write(d.join("state.json"), br#"{"root":"x","pass":3,"phase":"partial"}"#).unwrap();
    assert_eq!(files.state().map(|s| (s.pass, s.phase, s.files)), Some((3, "partial".into(), 0)));
    std::fs::write(d.join("state.json"), b"{").unwrap();
    assert!(files.state().is_none());
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn a_large_root_round_trips() {
    let d = scratch("cat-large");
    let (a, files) = (Catalog::new(), RootFiles::new(&d, ROOT));
    a.add_root(ROOT);
    let subs: Vec<String> = (0..300).map(|i| format!("Job-{i:03}")).collect();
    a.apply(&Listing { dir: ROOT.into(), pass: 4, listed_at: 50, subdirs: subs.clone(), files: vec![] });
    for (i, s) in subs.iter().enumerate() {
        let fl = (0..40).map(|k| (format!("14-01-15-{:04}_PSV-{:04}-01_REV-{}.pdf", i, k, (b'A' + (k % 5) as u8) as char), (i * k) as u64, 1_600_000_000 + (i * 40 + k) as i64)).collect();
        a.apply(&Listing { dir: format!(r"{ROOT}\{s}"), pass: 4, listed_at: 51, subdirs: vec![], files: fl });
    }
    files.write_full(&a, Mark { pass: 4, seq: 1 }).unwrap();
    let b = Catalog::new();
    assert_eq!(files.load(&b).unwrap(), Mark { pass: 4, seq: 1 });
    assert_eq!((b.dir_count(), b.file_count()), (301, 12_000));
    assert_eq!(dump(&b), dump(&a));
    let _ = std::fs::remove_dir_all(d);
}
