use super::*;

fn listing(dir: &str, pass: i64, at: i64, subdirs: &[&str], files: &[(&str, u64, i64)]) -> Listing {
    Listing { dir: dir.into(), pass, listed_at: at, subdirs: subdirs.iter().map(|s| s.to_string()).collect(), files: files.iter().map(|(n, s, m)| (n.to_string(), *s, *m)).collect() }
}

fn names(rows: &[FileInfo]) -> Vec<&str> {
    rows.iter().map(|r| r.name.as_str()).collect()
}

fn paths(rows: &[FileInfo]) -> Vec<&str> {
    rows.iter().map(|r| r.path.as_str()).collect()
}

fn t(q: &str) -> Term {
    Term::new(q).unwrap()
}

fn all() -> NameScope {
    NameScope::default()
}

fn under(roots: &[&str]) -> NameScope {
    NameScope { roots: roots.iter().map(|r| r.to_string()).collect(), ext: None }
}

/// Every file under `folder`, sorted.
fn tree(c: &Catalog, folder: &str) -> Vec<(String, u64, i64)> {
    let mut out = Vec::new();
    c.for_each_under(folder, &mut |dir, name, size, mtime| {
        out.push((join(dir, name), size, mtime));
        true
    });
    out.sort();
    out
}

/// The files of `namemem`'s test, as folder listings.
fn sample() -> Catalog {
    let c = Catalog::new();
    for root in [r"\\srv\p", r"C:\", r"\\srv\other\"] {
        c.add_root(root);
    }
    c.apply(&listing(r"\\srv\p", 1, 100, &["Rev-1"], &[("FSK_Batch-1.zip", 1, 2), ("fsk.xlsx", 1, 3), ("plan & notes.txt", 1, 6)]));
    c.apply(&listing(r"\\srv\p\Rev-1", 1, 100, &[], &[("14-01-15-2632_PSV-2809-01_HOLD.pdf", 1, 1)]));
    c.apply(&listing(r"C:\", 1, 100, &[], &[("Обычный.pmvx", 1, 4)]));
    c.apply(&listing(r"\\srv\other", 1, 100, &[], &[("PSV2809.dwg", 1, 5)]));
    c
}

#[test]
fn finds_names_like_everything() {
    let c = sample();
    assert_eq!(names(&c.candidates(&t("fsk"), &all(), 10)), vec!["fsk.xlsx", "FSK_Batch-1.zip"], "whole name first");
    assert_eq!(names(&c.candidates(&t("psv-2809"), &all(), 10)), vec!["14-01-15-2632_PSV-2809-01_HOLD.pdf", "PSV2809.dwg"]);
    assert_eq!(names(&c.candidates(&t("обычный"), &all(), 10)), vec!["Обычный.pmvx"]);
    assert_eq!(names(&c.candidates(&t("*.zip"), &all(), 10)), vec!["FSK_Batch-1.zip"]);
    assert_eq!(names(&c.candidates(&t("&"), &all(), 10)), vec!["plan & notes.txt"]);
    assert_eq!(names(&c.candidates(&t("x"), &NameScope { ext: Some("dwg".into()), ..Default::default() }, 10)), Vec::<&str>::new());
    assert_eq!(names(&c.candidates(&t("psv"), &NameScope { ext: Some(".DWG".into()), ..Default::default() }, 10)), vec!["PSV2809.dwg"]);
    assert_eq!(names(&c.candidates(&t("psv"), &under(&[r"\\srv\other"]), 10)), vec!["PSV2809.dwg"]);
    // Every word in the name or the folder path.
    assert_eq!(names(&c.candidates_all(&[t("rev-1"), t("psv")], &all(), 10)), vec!["14-01-15-2632_PSV-2809-01_HOLD.pdf"]);
    assert_eq!(names(&c.candidates_all(&[t("srv"), t("fsk")], &all(), 10)), vec!["fsk.xlsx", "FSK_Batch-1.zip"]);
    assert!(c.candidates_all(&[t("rev-1"), t("fsk")], &all(), 10).is_empty());
    let row = c.get(r"\\srv\p\FSK_Batch-1.zip").unwrap();
    assert_eq!((row.path.as_str(), row.name.as_str(), row.size, row.mtime), (r"\\srv\p\FSK_Batch-1.zip", "FSK_Batch-1.zip", 1, 2));
    // The file is gone from the next listing.
    c.apply(&listing(r"\\srv\p", 1, 101, &["Rev-1"], &[("fsk.xlsx", 1, 3), ("plan & notes.txt", 1, 6)]));
    assert!(c.get(r"\\srv\p\FSK_Batch-1.zip").is_none());
    assert_eq!(names(&c.candidates(&t("fsk"), &all(), 10)), vec!["fsk.xlsx"]);
    // A rename keeps one entry.
    c.apply(&listing(r"\\srv\p", 1, 102, &["Rev-1"], &[("fsk-old.xlsx", 1, 3), ("plan & notes.txt", 1, 6)]));
    assert_eq!(names(&c.candidates(&t("fsk"), &all(), 10)), vec!["fsk-old.xlsx"]);
    assert_eq!(c.get(r"C:\Обычный.pmvx").unwrap().path, r"C:\Обычный.pmvx");
}

#[test]
fn scopes_are_folder_prefixes() {
    let c = sample();
    c.apply(&listing(r"C:\Work\Rev-10", 1, 100, &[], &[("psv-1.pdf", 1, 9)]));
    c.apply(&listing(r"\\srv\p\Rev-1\Sub", 1, 100, &[], &[("psv-2.pdf", 1, 8)]));
    let psv = |scope: &NameScope| c.candidates(&t("psv"), scope, 10).into_iter().map(|f| f.name).collect::<Vec<_>>();
    assert_eq!(psv(&all()).len(), 4);
    // A drive, written with or without its backslash.
    assert_eq!(psv(&under(&[r"C:\"])), ["psv-1.pdf"]);
    assert_eq!(psv(&under(&["c:"])), ["psv-1.pdf"]);
    assert_eq!(psv(&under(&[r"c:\work"])), ["psv-1.pdf"]);
    assert_eq!(psv(&under(&[r"C:\Wor"])), Vec::<String>::new(), "a folder, not a prefix of its name");
    // A folder below a root, the root itself, and above the roots.
    assert_eq!(psv(&under(&[r"\\SRV\P\rev-1\"])), ["psv-2.pdf", "14-01-15-2632_PSV-2809-01_HOLD.pdf"]);
    assert_eq!(psv(&under(&[r"\\srv\p\Rev-1\Sub"])), ["psv-2.pdf"]);
    assert_eq!(psv(&under(&[r"\\srv"])).len(), 3);
    assert_eq!(psv(&under(&[r"\\srv\p", r"C:\"])).len(), 3);
    assert_eq!(psv(&under(&[r"\\srv\nowhere"])), Vec::<String>::new());
    assert_eq!(psv(&under(&[r"\\srv\p\Rev"])), Vec::<String>::new());
    assert_eq!(c.candidates_path(r"rev-1\", &under(&[r"\\srv\p\Rev-1\Sub"]), 10).len(), 1);
}

#[test]
fn newest_first_and_capped() {
    let c = Catalog::new();
    c.add_root(r"\\srv\p");
    let files: Vec<(String, u64, i64)> = (0..500).map(|i| (format!("ISO-{i:04}.pdf"), i as u64, 1000 + i as i64)).collect();
    c.apply(&Listing { dir: r"\\srv\p".into(), pass: 1, listed_at: 1, subdirs: vec![], files });
    c.apply(&listing(r"\\srv\p\x", 1, 1, &[], &[("iso.pdf", 7, 5), ("ISO.dwg", 8, 6)]));
    let r = c.candidates(&t("iso"), &all(), 5);
    assert_eq!(names(&r), vec!["ISO.dwg", "iso.pdf", "ISO-0499.pdf", "ISO-0498.pdf", "ISO-0497.pdf"]);
    assert_eq!((r[0].size, r[0].mtime, r[2].size, r[2].mtime), (8, 6, 499, 1499));
    assert_eq!(r[4].path, r"\\srv\p\ISO-0497.pdf");
    assert_eq!(c.candidates(&t("iso"), &all(), 0).len(), 502, "0 is no limit");
    assert_eq!(c.candidates(&t("iso"), &all(), 502).len(), 502);
    assert_eq!(c.candidates(&t("iso-01"), &all(), 1000).len(), 100);
}

#[test]
fn a_million_names_scan_quickly() {
    let c = Catalog::new();
    c.add_root(r"\\srv\p");
    for d in 0..5000i64 {
        let files = (0..200).map(|k| d + k * 5000).map(|i| (format!("ISO-{i:07}-01_rev{}.pdf", i % 7), 1, i)).collect();
        c.apply(&Listing { dir: format!(r"\\srv\p\{d}"), pass: 1, listed_at: 1, subdirs: vec![], files });
    }
    assert_eq!(c.file_count(), 1_000_000);
    // What the plainest pass over these names costs on this PC right now, on
    // one thread: the limit below is 3 s as in `namemem`, but a PC busy with
    // other work is not a slow scan.
    let mut every = Vec::with_capacity(1_000_000);
    c.for_each_under(r"\\srv\p", &mut |_, name, _, _| {
        every.push(name.to_string());
        true
    });
    let t0 = std::time::Instant::now();
    let plain = every.iter().map(|n| n.to_lowercase()).filter(|n| n.contains('x') || n.contains("0012345")).count();
    let reference = t0.elapsed().as_millis();
    assert_eq!((every.len(), plain), (1_000_000, 1));
    drop(every);
    // The best of three: other tests run at the same time.
    let mut ms = u128::MAX;
    for _ in 0..3 {
        let t0 = std::time::Instant::now();
        let r = c.candidates(&t("x"), &all(), 8000);
        let r2 = c.candidates(&t("0012345"), &all(), 8000);
        ms = ms.min(t0.elapsed().as_millis());
        assert!(r.is_empty());
        assert_eq!(paths(&r2), vec![r"\\srv\p\2345\ISO-0012345-01_rev4.pdf"]);
    }
    assert!(ms < 3000 || ms < 4 * reference, "two scans of a million names took {ms} ms (a plain pass over them on one thread: {reference} ms)");
    assert_eq!(c.candidates(&t("rev3"), &all(), 8000).len(), 8000);
}

#[test]
fn a_listing_replaces_a_folder() {
    let c = Catalog::new();
    c.add_root(r"\\srv\p\");
    assert_eq!(c.roots(), vec![r"\\srv\p"]);
    assert_eq!((c.dir_count(), c.file_count(), c.generation()), (0, 0, 1));
    assert!(c.contains_dir(r"\\srv\p") && !c.dir_listed(r"\\srv\p"));

    // First listing.
    let ch = c.apply(&listing(r"\\srv\p", 7, 100, &["A", "B"], &[("one.pdf", 10, 1), ("two.pdf", 20, 2)]));
    assert_eq!(ch, Change { files_added: 2, ..Default::default() });
    assert_eq!((c.dir_count(), c.file_count(), c.generation()), (1, 2, 2));
    assert_eq!(c.root_counts(r"\\SRV\P"), (1, 2));
    assert_eq!(c.root_counts(r"\\srv\q"), (0, 0));
    // New subfolder names are known, not listed.
    assert!(c.contains_dir(r"\\srv\p\A") && c.contains_dir(r"\\SRV\p\b\"));
    assert!(c.dir_listed(r"\\srv\p") && !c.dir_listed(r"\\srv\p\A") && !c.dir_listed(r"\\srv\p\nope"));
    assert!(!c.contains_dir(r"\\srv\p\C") && !c.contains_dir(r"\\srv\q"));

    // The same again alters nothing.
    assert!(c.apply(&listing(r"\\srv\p", 7, 100, &["A", "B"], &[("two.pdf", 20, 2), ("one.pdf", 10, 1)])).is_empty());
    assert_eq!(c.generation(), 2);

    // Added, changed, removed.
    let ch = c.apply(&listing(r"\\srv\p", 7, 110, &["A", "B"], &[("one.pdf", 11, 1), ("three.pdf", 30, 3)]));
    assert_eq!(ch, Change { files_added: 1, files_changed: 1, files_removed: 1, dirs_removed: 0 });
    assert_eq!(c.get(r"\\srv\p\one.pdf").unwrap().size, 11);
    assert!(c.get(r"\\srv\p\two.pdf").is_none());
    assert_eq!(c.get(r"\\SRV\P\THREE.PDF").unwrap().path, r"\\srv\p\three.pdf", "found in any case, returned as stored");
    assert_eq!(c.generation(), 3);
    // Only the time changed.
    assert_eq!(c.apply(&listing(r"\\srv\p", 7, 111, &["A", "B"], &[("one.pdf", 11, 5), ("three.pdf", 30, 3)])).files_changed, 1);

    // A stale listing is ignored; one of the same moment is not.
    assert!(c.apply(&listing(r"\\srv\p", 7, 105, &[], &[])).is_empty());
    assert_eq!(c.file_count(), 2);
    assert!(c.contains_dir(r"\\srv\p\A"));
    assert_eq!(c.apply(&listing(r"\\srv\p", 7, 111, &["A", "B"], &[("one.pdf", 11, 5)])).files_removed, 1);

    // Outside every root.
    assert!(c.apply(&listing(r"\\srv\q", 7, 500, &[], &[("x.pdf", 1, 1)])).is_empty());
    assert!(c.apply(&listing(r"\\srv\pp", 7, 500, &[], &[("x.pdf", 1, 1)])).is_empty());
    assert!(c.get(r"\\srv\pp\x.pdf").is_none() && c.get(r"\\srv\p").is_none() && c.get(r"\\srv\p\").is_none());
    assert_eq!(c.file_count(), 1);
}

#[test]
fn a_removed_subfolder_takes_everything_below_along() {
    let c = Catalog::new();
    c.add_root(r"\\srv\p");
    c.apply(&listing(r"\\srv\p", 1, 100, &["A", "B"], &[]));
    c.apply(&listing(r"\\srv\p\A", 1, 100, &["Deep"], &[("a1.pdf", 1, 1), ("a2.pdf", 1, 1)]));
    c.apply(&listing(r"\\srv\p\A\Deep", 1, 100, &["Deeper"], &[("d1.pdf", 1, 1)]));
    c.apply(&listing(r"\\srv\p\A\Deep\Deeper", 1, 100, &[], &[("e1.pdf", 1, 1), ("e2.pdf", 1, 1)]));
    c.apply(&listing(r"\\srv\p\B", 1, 100, &[], &[("b1.pdf", 1, 1)]));
    assert_eq!((c.dir_count(), c.file_count()), (5, 6));

    let ch = c.apply(&listing(r"\\srv\p", 2, 200, &["B"], &[]));
    assert_eq!(ch, Change { files_removed: 5, dirs_removed: 3, ..Default::default() });
    assert_eq!((c.dir_count(), c.file_count()), (2, 1));
    assert!(!c.contains_dir(r"\\srv\p\A") && !c.contains_dir(r"\\srv\p\A\Deep\Deeper"));
    assert!(c.get(r"\\srv\p\A\Deep\Deeper\e1.pdf").is_none());
    assert_eq!(names(&c.candidates(&t("pdf"), &all(), 10)), vec!["b1.pdf"]);
    assert_eq!(tree(&c, r"\\srv\p").len(), 1);

    // A listing of the removed folder from before it went does not bring it back...
    assert!(c.apply(&listing(r"\\srv\p\A\Deep", 1, 150, &[], &[("d1.pdf", 1, 1)])).is_empty());
    assert!(!c.contains_dir(r"\\srv\p\A"));
    // ...the folder coming back does.
    c.apply(&listing(r"\\srv\p", 3, 300, &["A", "B"], &[]));
    assert!(c.contains_dir(r"\\srv\p\A") && !c.dir_listed(r"\\srv\p\A"));
    assert_eq!(c.apply(&listing(r"\\srv\p\A", 3, 300, &[], &[("new.pdf", 1, 1)])).files_added, 1);
    assert_eq!(paths(&c.candidates(&t("new"), &all(), 10)), vec![r"\\srv\p\A\new.pdf"]);
    assert_eq!((c.dir_count(), c.file_count()), (3, 2));
}

#[test]
fn a_folder_not_yet_known_brings_its_parents() {
    let c = Catalog::new();
    c.add_root(r"\\srv\p");
    let ch = c.apply(&listing(r"\\srv\p\a\b\c", 0, 100, &["d"], &[("x.pdf", 1, 1)]));
    assert_eq!(ch.files_added, 1);
    for known in [r"\\srv\p\a", r"\\srv\p\a\b", r"\\srv\p\a\b\c", r"\\srv\p\a\b\c\d"] {
        assert!(c.contains_dir(known), "{known}");
    }
    assert!(c.dir_listed(r"\\srv\p\a\b\c") && !c.dir_listed(r"\\srv\p\a\b") && !c.dir_listed(r"\\srv\p"));
    assert_eq!(c.dir_count(), 1);
    assert_eq!(c.get(r"\\srv\p\A\B\C\X.PDF").unwrap().path, r"\\srv\p\a\b\c\x.pdf");
    // A folder made after its parent was listed.
    c.apply(&listing(r"\\srv\p\a", 1, 200, &["b"], &[]));
    assert_eq!(c.apply(&listing(r"\\srv\p\a\new", 0, 250, &[], &[("y.pdf", 1, 1)])).files_added, 1);
    // The parent listed again without them: gone.
    let ch = c.apply(&listing(r"\\srv\p\a", 2, 300, &[], &[]));
    assert_eq!((ch.dirs_removed, ch.files_removed), (4, 2));
    assert_eq!(c.file_count(), 0);
}

#[test]
fn listed_in_pass_is_for_resuming_a_walk() {
    let c = Catalog::new();
    c.add_root(r"\\srv\Project");
    c.apply(&listing(r"\\srv\Project", 7, 100, &["J100"], &[]));
    // Listing the root records its subfolder, which must not make a resumed walk skip the subfolder's contents.
    assert_eq!(c.listed_in_pass(r"\\srv\Project\J100", 7), None);
    assert_eq!(c.listed_in_pass(r"\\srv\Project", 7), Some(vec![r"\\srv\Project\J100".to_string()]));
    c.apply(&listing(r"\\srv\Project\J100", 7, 101, &[], &[]));
    assert_eq!(c.listed_in_pass(r"\\srv\project\j100\", 7), Some(vec![]));
    assert_eq!(c.listed_in_pass(r"\\srv\Project\J100", 8), None, "another pass lists it again");
    assert_eq!(c.listed_in_pass(r"\\srv\Project\J200", 7), None);
    assert_eq!(c.listed_in_pass(r"\\other\x", 7), None);
    // A one-off relist takes the folder out of the pass.
    c.apply(&listing(r"\\srv\Project", 0, 102, &["J100", "J200"], &[]));
    assert_eq!(c.listed_in_pass(r"\\srv\Project", 7), None);
    let mut kids = c.listed_in_pass(r"\\srv\Project", 0).unwrap();
    kids.sort();
    assert_eq!(kids, vec![r"\\srv\Project\J100", r"\\srv\Project\J200"]);
}

#[test]
fn a_drive_root() {
    let c = Catalog::new();
    c.add_root("c:");
    assert_eq!(c.roots(), vec![r"c:\"]);
    c.apply(&listing(r"C:\", 1, 100, &["Work"], &[("boot.ini", 1, 1)]));
    c.apply(&listing(r"C:\Work", 1, 100, &[], &[("plan.dwg", 2, 2)]));
    assert_eq!(c.get(r"C:\BOOT.INI").unwrap().path, r"c:\boot.ini");
    assert_eq!(c.get(r"c:\work\plan.dwg").unwrap().path, r"c:\Work\plan.dwg");
    assert_eq!(c.listed_in_pass("C:", 1), Some(vec![r"c:\Work".to_string()]));
    assert_eq!(tree(&c, r"C:\").len(), 2);
    assert_eq!(tree(&c, r"C:\Work"), vec![(r"c:\Work\plan.dwg".to_string(), 2, 2)]);
    assert!(c.get(r"D:\boot.ini").is_none());
}

#[test]
fn names_are_stored_by_what_differs() {
    let c = Catalog::new();
    c.add_root(r"\\srv\p");
    // More than one restart block, shared prefixes, a prefix that ends inside a character, odd numbers.
    let mut files: Vec<(String, u64, i64)> = (0..100).map(|i| (format!("14-01-15-{:04}_PSV-2809-01_REV-B.pdf", i * 7), i as u64 * 1_000_003, 1_700_000_000 + i as i64)).collect();
    files.extend([("é.pdf".to_string(), u64::MAX, i64::MIN), ("è.pdf".to_string(), 0, i64::MAX), ("a".repeat(600), 5, -1), (format!("{}b", "a".repeat(600)), 6, 0)]);
    files.push(("ÉCOLE.PDF".into(), 1, 1));
    let l = Listing { dir: r"\\srv\p".into(), pass: 1, listed_at: 1, subdirs: vec![], files: files.clone() };
    assert_eq!(c.apply(&l).files_added, 105);
    for (name, size, mtime) in &files {
        let f = c.get(&format!(r"\\srv\p\{name}")).unwrap_or_else(|| panic!("{name}"));
        assert_eq!((&f.name, f.size, f.mtime), (name, *size, *mtime));
    }
    let mut want: Vec<(String, u64, i64)> = files.iter().map(|(n, s, m)| (format!(r"\\srv\p\{n}"), *s, *m)).collect();
    want.sort();
    assert_eq!(tree(&c, r"\\srv\p"), want);
    // Each hit is read back from the middle of the block.
    let hits = c.candidates(&t("psv-2809"), &all(), 0);
    assert_eq!(hits.len(), 100);
    for (i, h) in hits.iter().rev().enumerate() {
        assert_eq!((h.name.clone(), h.size, h.mtime), files[i]);
    }
    assert_eq!(c.get(r"\\srv\p\école.pdf").unwrap().name, "ÉCOLE.PDF");
    assert_eq!(names(&c.candidates(&t("école"), &all(), 10)), vec!["ÉCOLE.PDF"]);
    // The block (pair bits included) is smaller than the names alone would be with fixed-size numbers.
    let g = c.r();
    let (blob, plain) = (g[0].dirs[0].files.as_ref().unwrap().len(), files.iter().map(|f| f.0.len() + 16).sum::<usize>());
    assert!(blob < plain, "{blob} bytes for {plain} bytes of names and numbers");
}

#[test]
fn many_subfolders_come_and_go() {
    let c = Catalog::new();
    c.add_root(r"\\srv\p");
    let subs: Vec<String> = (0..3000).map(|i| format!("Job-{i:04}")).collect();
    c.apply(&Listing { dir: r"\\srv\p".into(), pass: 1, listed_at: 1, subdirs: subs.clone(), files: vec![] });
    for s in &subs {
        c.apply(&listing(&format!(r"\\srv\p\{s}"), 1, 1, &["in"], &[("f.pdf", 1, 1)]));
    }
    assert_eq!((c.dir_count(), c.file_count()), (3001, 3000));
    // Every other one goes; new ones take the freed places.
    let mut next: Vec<String> = subs.iter().step_by(2).cloned().collect();
    next.extend((0..500).map(|i| format!("New-{i:04}")));
    let ch = c.apply(&Listing { dir: r"\\srv\p".into(), pass: 2, listed_at: 2, subdirs: next, files: vec![] });
    assert_eq!((ch.dirs_removed, ch.files_removed), (3000, 1500));
    assert_eq!((c.dir_count(), c.file_count()), (1501, 1500));
    assert!(c.contains_dir(r"\\srv\p\job-0000\IN") && !c.contains_dir(r"\\srv\p\Job-0001") && c.contains_dir(r"\\srv\p\new-0499"));
    c.apply(&listing(r"\\srv\p\New-0007", 2, 2, &[], &[("n.pdf", 1, 1)]));
    assert_eq!(c.candidates(&t("f.pdf"), &all(), 0).len(), 1500);
    assert_eq!(paths(&c.candidates(&t("n.pdf"), &all(), 0)), vec![r"\\srv\p\New-0007\n.pdf"]);
    assert_eq!(c.listed_in_pass(r"\\srv\p", 2).unwrap().len(), 2000);
    assert_eq!(tree(&c, r"\\srv\p").len(), 1501);
    {
        let g = c.r();
        assert!(g[0].dirs.len() <= 6002, "freed folders are reused, {} places", g[0].dirs.len());
        assert!(g[0].clashes.is_empty());
    }
    // Everything below the root goes, and the names of removed folders with it.
    c.apply(&listing(r"\\srv\p", 3, 3, &[], &[]));
    assert_eq!((c.dir_count(), c.file_count()), (1, 0));
    let g = c.r();
    assert!(g[0].index.is_empty());
    assert_eq!(g[0].free.len() + 1, g[0].dirs.len());
}

#[test]
fn two_folders_with_one_key_are_both_found() {
    let mut tr = Tree::new(r"\\srv\p");
    let a = tr.add_child(0, "a").unwrap();
    // As if "b" hashed where "a" did.
    tr.index.insert(child_key(0, "b"), a);
    let b = tr.add_child(0, "b").unwrap();
    assert_eq!(tr.clashes, vec![b]);
    assert_eq!((tr.find_child(0, "A"), tr.find_child(0, "B"), tr.find_child(0, "c")), (Some(a), Some(b), None));
    let mut ch = Change::default();
    tr.dirs[0].child = a;
    tr.dirs[a as usize].next = NONE;
    tr.drop_subtree(b, &mut ch);
    assert!(tr.clashes.is_empty());
    assert_eq!((tr.find_child(0, "a"), tr.find_child(0, "b")), (Some(a), None));
}

#[test]
fn twins_share_a_stem() {
    let c = Catalog::new();
    c.add_root(r"\\srv\p");
    c.apply(&listing(r"\\srv\p\iso\pdf", 1, 1, &[], &[("ISO-1001_Rev-B.pdf", 1, 1), ("ISO-1002.pdf", 1, 1), ("iso-1001_rev-b.tif", 1, 2)]));
    c.apply(&listing(r"\\srv\p\iso\dwg", 1, 1, &["old"], &[("ISO-1001_Rev-B.dwg", 1, 3), ("ISO-1001_Rev-B.bak.dwg", 1, 1)]));
    c.apply(&listing(r"\\srv\p\iso\dwg\old", 1, 1, &[], &[("ISO-1001_Rev-B.dwg", 1, 1)]));
    c.apply(&listing(r"\\srv\p\other", 1, 1, &[], &[("ISO-1001_Rev-B.dgn", 1, 1)]));
    c.apply(&listing(r"\\srv\p", 1, 1, &["iso", "other"], &[("ISO-1001_Rev-B.xlsx", 1, 1), ("ISO-1001_Rev-B.docx", 1, 1)]));
    let mut tw = c.twins(r"\\srv\p\iso\pdf\iso-1001_rev-b.PDF");
    tw.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(paths(&tw), vec![r"\\srv\p\iso\dwg\ISO-1001_Rev-B.dwg", r"\\srv\p\iso\pdf\iso-1001_rev-b.tif"], "same folder and sibling folders only");
    assert_eq!(tw[0].mtime, 3);
    assert!(c.twins(r"\\srv\p\iso\pdf\ISO-1002.pdf").is_empty());
    // In the root there are no sibling folders.
    assert_eq!(names(&c.twins(r"\\srv\p\ISO-1001_Rev-B.xlsx")), vec!["ISO-1001_Rev-B.docx"]);
    // A file the catalog does not hold still has twins where it would be.
    assert_eq!(c.twins(r"\\srv\p\iso\pdf\ISO-1002.dwg").len(), 1);
    assert!(c.twins(r"\\srv\p\nowhere\ISO-1002.dwg").is_empty() && c.twins(r"\\else\x.pdf").is_empty() && c.twins(r"\\srv\p").is_empty());
}

#[test]
fn pasted_paths_and_pieces_of_them() {
    let c = Catalog::new();
    c.add_root(r"\\10.0.0.5\project");
    c.add_root(r"C:\");
    c.apply(&listing(r"\\10.0.0.5\project\J100\Rev-1", 1, 1, &[], &[("14-01-15-2632_PSV-2809.pdf", 1, 10), ("line list.xlsx", 1, 20)]));
    c.apply(&listing(r"\\10.0.0.5\project\J100\Rev-1\2632", 1, 1, &[], &[("14-01-15-2632_PSV-2809.dwg", 1, 30), ("14-01-15-2632_PSV-2809.dwg.bak", 1, 5)]));
    c.apply(&listing(r"\\10.0.0.5\project\J100\Rev-10", 1, 1, &[], &[("other.pdf", 1, 40)]));
    c.apply(&listing(r"\\10.0.0.5\project\J200\Rev-1", 1, 1, &[], &[("14-01-15-2632_PSV-2809.pdf", 1, 50)]));
    c.apply(&listing(r"C:\Rev-1", 1, 1, &[], &[("local.pdf", 1, 60)]));
    let p = |text: &str| c.candidates_path(text, &all(), 100).into_iter().map(|f| f.path).collect::<Vec<_>>();

    // A whole file path, in any case, quoted or not.
    assert_eq!(p(r"\\10.0.0.5\PROJECT\j100\rev-1\14-01-15-2632_psv-2809.PDF"), [r"\\10.0.0.5\project\J100\Rev-1\14-01-15-2632_PSV-2809.pdf"]);
    assert_eq!(p("\"C:\\Rev-1\\local.pdf\" "), [r"C:\Rev-1\local.pdf"]);
    // The file whose path is the text, before the one whose path only starts with it.
    assert_eq!(p(r"\\10.0.0.5\project\J100\Rev-1\2632\14-01-15-2632_PSV-2809.dwg"), [r"\\10.0.0.5\project\J100\Rev-1\2632\14-01-15-2632_PSV-2809.dwg", r"\\10.0.0.5\project\J100\Rev-1\2632\14-01-15-2632_PSV-2809.dwg.bak"]);

    // A folder path, with or without its backslash: files directly in it (newest first), then below.
    let in_j100_rev1 = [r"\\10.0.0.5\project\J100\Rev-1\line list.xlsx", r"\\10.0.0.5\project\J100\Rev-1\14-01-15-2632_PSV-2809.pdf", r"\\10.0.0.5\project\J100\Rev-1\2632\14-01-15-2632_PSV-2809.dwg", r"\\10.0.0.5\project\J100\Rev-1\2632\14-01-15-2632_PSV-2809.dwg.bak"];
    assert_eq!(p(r"\\10.0.0.5\project\J100\Rev-1\"), in_j100_rev1);
    // Without the backslash the text is also the start of `Rev-10`.
    let mut wider = in_j100_rev1.to_vec();
    wider.insert(2, r"\\10.0.0.5\project\J100\Rev-10\other.pdf");
    assert_eq!(p(r"\\10.0.0.5\project\J100\Rev-1"), wider);

    // A fragment naming folders: every `Rev-1`, not `Rev-10`.
    assert_eq!(p(r"rev-1\"), [r"C:\Rev-1\local.pdf", r"\\10.0.0.5\project\J200\Rev-1\14-01-15-2632_PSV-2809.pdf", r"\\10.0.0.5\project\J100\Rev-1\line list.xlsx", r"\\10.0.0.5\project\J100\Rev-1\14-01-15-2632_PSV-2809.pdf", r"\\10.0.0.5\project\J100\Rev-1\2632\14-01-15-2632_PSV-2809.dwg", r"\\10.0.0.5\project\J100\Rev-1\2632\14-01-15-2632_PSV-2809.dwg.bak"]);
    assert_eq!(p(r"\rev-10"), [r"\\10.0.0.5\project\J100\Rev-10\other.pdf"]);
    assert_eq!(p(r"J100\Rev-1\"), in_j100_rev1);

    // A fragment that runs from a folder into a file name.
    assert_eq!(p(r"2632\14-01-15-2632_PSV"), [r"\\10.0.0.5\project\J100\Rev-1\2632\14-01-15-2632_PSV-2809.dwg", r"\\10.0.0.5\project\J100\Rev-1\2632\14-01-15-2632_PSV-2809.dwg.bak"]);
    // Ending with a whole file name: those files first (newest first), longer names after.
    assert_eq!(p(r"Rev-1\14-01-15-2632_PSV-2809.pdf"), [r"\\10.0.0.5\project\J200\Rev-1\14-01-15-2632_PSV-2809.pdf", r"\\10.0.0.5\project\J100\Rev-1\14-01-15-2632_PSV-2809.pdf"]);
    assert_eq!(p(r"\2632\14-01-15-2632_PSV-2809.dwg"), [r"\\10.0.0.5\project\J100\Rev-1\2632\14-01-15-2632_PSV-2809.dwg", r"\\10.0.0.5\project\J100\Rev-1\2632\14-01-15-2632_PSV-2809.dwg.bak"]);

    assert!(p(r"J300\").is_empty() && p(r"Rev-1\nothing").is_empty() && p("").is_empty() && p(r"Rev-1\\x").is_empty());
    // Scope and cap apply as in a name search.
    assert_eq!(c.candidates_path(r"rev-1\", &under(&[r"C:\"]), 100).len(), 1);
    assert_eq!(c.candidates_path(r"rev-1\", &NameScope { roots: vec![], ext: Some("dwg".into()) }, 100).len(), 1);
    assert_eq!(c.candidates_path(r"rev-1\", &all(), 2).len(), 2);
    // Text without a backslash is looked for in the names.
    assert_eq!(p("local.pdf"), [r"C:\Rev-1\local.pdf"]);
}

#[test]
fn every_file_under_a_folder() {
    let c = sample();
    c.apply(&listing(r"\\srv\p\Rev-1\Sub", 1, 100, &[], &[("deep.pdf", 9, 9)]));
    assert_eq!(tree(&c, r"\\srv\p").len(), 5);
    assert_eq!(tree(&c, r"\\SRV\P\rev-1\"), vec![(r"\\srv\p\Rev-1\14-01-15-2632_PSV-2809-01_HOLD.pdf".to_string(), 1, 1), (r"\\srv\p\Rev-1\Sub\deep.pdf".to_string(), 9, 9)]);
    assert_eq!(tree(&c, r"\\srv").len(), 6, "above the roots: every root below");
    assert_eq!(tree(&c, r"C:\").len(), 1);
    assert!(tree(&c, r"\\srv\p\nowhere").is_empty() && tree(&c, r"\\else").is_empty() && tree(&c, "").is_empty());
    // Stopping, and using the catalog from inside (it holds no lock meanwhile).
    let mut seen = 0;
    c.for_each_under(r"\\srv\p", &mut |dir, name, _, _| {
        seen += 1;
        assert!(c.get(&join(dir, name)).is_some());
        c.apply(&listing(r"\\srv\p\Rev-1\Sub", 1, 200 + seen, &[], &[]));
        seen < 2
    });
    assert_eq!(seen, 2);
}

#[test]
fn roots_come_and_go() {
    let c = sample();
    let g0 = c.generation();
    c.add_root(r"\\SRV\P\");
    assert_eq!((c.roots().len(), c.generation()), (3, g0), "known already");
    c.remove_root(r"\\srv\p\");
    assert_eq!(c.roots(), vec![r"C:\", r"\\srv\other"]);
    assert!(c.generation() > g0);
    assert!(c.get(r"\\srv\p\fsk.xlsx").is_none() && !c.contains_dir(r"\\srv\p"));
    assert_eq!((c.dir_count(), c.file_count()), (2, 2));
    assert_eq!(names(&c.candidates(&t("psv"), &all(), 10)), vec!["PSV2809.dwg"]);
    // Nested roots: the deeper one holds what is under it.
    c.add_root(r"\\srv\other\inner");
    c.apply(&listing(r"\\srv\other\inner", 1, 1, &[], &[("in.pdf", 1, 1)]));
    assert_eq!(c.root_counts(r"\\srv\other\inner"), (1, 1));
    assert_eq!(c.root_counts(r"\\srv\other"), (1, 1));
    c.shrink_to_fit();
    assert_eq!(c.get(r"\\srv\other\inner\in.pdf").unwrap().size, 1);
}

#[test]
fn odd_listings_do_no_harm() {
    let c = Catalog::new();
    c.add_root(r"\\srv\p");
    let ch = c.apply(&listing(r"\\srv\p", 1, 1, &["", r"a\b", "ok", "OK"], &[("", 1, 1), ("dup.pdf", 1, 1), ("dup.pdf", 2, 2), ("Dup.pdf", 3, 3)]));
    assert_eq!(ch.files_added, 2);
    assert_eq!(c.listed_in_pass(r"\\srv\p", 1), Some(vec![r"\\srv\p\ok".to_string()]));
    assert_eq!(c.get(r"\\srv\p\Dup.pdf").unwrap().size, 3);
    assert!(c.get(r"\\srv\p\DUP.PDF").is_some());
    assert!(c.candidates(&t("zzz"), &all(), 10).is_empty());
    assert!(Catalog::new().candidates(&t("zzz"), &all(), 10).is_empty());
    assert_eq!(norm_dir(r"C:"), r"C:\");
    assert_eq!(norm_dir(r"\\srv\p\\"), r"\\srv\p");
}

#[test]
fn the_quick_searches_agree_with_the_plain_ones() {
    for (hay, needle) in [("14-01-15-2632_psv-2809.pdf", "psv"), ("abc", "abc"), ("abc", "abcd"), ("aab", "ab"), ("ababac", "abac"), ("abc", ""), ("", ""), ("", "a"), ("обычный.pmvx", "чный"), ("xxa", "a"), ("xxa", "ax")] {
        assert_eq!(has(hay.as_bytes(), needle.as_bytes()), hay.contains(needle), "{needle:?} in {hay:?}");
        if hay.is_ascii() {
            assert_eq!(has_ci(hay.to_uppercase().as_bytes(), needle.as_bytes()), hay.contains(needle), "{needle:?} in {hay:?}, any case");
            assert_eq!(has_ci(hay.as_bytes(), needle.as_bytes()), hay.contains(needle));
        }
    }
    for (p, text) in [("*.zip*", "a.zip"), ("**.zip*", "a.zip.bak"), ("*iso-??01*", "iso-1001.pdf"), ("*iso-??01*", "iso-101.pdf"), ("a*b*c", "aXbXc"), ("a*b*c", "aXbX"), ("*", ""), ("?", ""), ("a?c", "abc"), ("*a*a*", "banana"), ("*ab", "aab")] {
        assert_eq!(glob_ascii(p.as_bytes(), text.as_bytes()), glob_match(p, text), "{p:?} on {text:?}");
    }
    let mut bufs = NameBufs::default();
    for (name, raw, whole) in [("FSK.xlsx", "fsk", true), ("fsk", "fsk", true), ("FSK_Batch-1.zip", "fsk", false), ("a.b.PDF", "a.b", true), ("a.b.pdf", "a", false), ("fsk.", "fsk", true), ("fs", "fsk", false), ("Обычный.pmvx", "обычный", true), ("Обычный.pmvx", "обычны", false), ("x.pdf", "é", false)] {
        let mut f = Forms::of(name, false, &mut bufs);
        assert_eq!(f.is_whole(raw), whole, "{raw:?} is the whole of {name:?}");
        let lower = lower(name);
        assert_eq!(whole, lower == raw || lower.rsplit_once('.').is_some_and(|(stem, _)| stem == raw));
        assert_eq!(Forms::of(name, false, &mut bufs).holds(raw), lower.contains(raw));
        assert_eq!(Forms::of(name, false, &mut bufs).sep(), sep_into(&lower, &mut Vec::new()));
    }
    let (mut a, mut b) = (Vec::new(), Vec::new());
    assert_eq!(sep_into("14-01_psv 2809.pdf", &mut a), b"1401psv2809pdf");
    assert_eq!(sep_into("обычный-№1.pmvx", &mut b), "обычный1pmvx".as_bytes());
}

/// `namemem`'s test of one name, word for word: what the search must agree with.
fn group_as_namemem(term: &Term, lower: &str) -> Option<u8> {
    if term.has_wildcards() {
        return glob_match(&format!("*{}*", term.raw), lower).then_some(2);
    }
    let raw = term.raw.as_str();
    if lower.contains(raw) {
        let whole = lower == raw || lower.rsplit_once('.').is_some_and(|(stem, _)| stem == raw);
        return Some(if whole { 0 } else { 1 });
    }
    if term.is_literal_only() || term.is_punctuation() || term.sep.is_empty() {
        return None;
    }
    let sep: String = lower.chars().filter(|c| c.is_alphanumeric()).collect();
    sep.contains(term.sep.as_str()).then_some(2)
}

#[test]
fn no_shortcut_loses_a_match() {
    // Folders of every size (so every size of pair-bit set), names with all kinds of separators and letters.
    let stems = ["14-01-15-2632_PSV-2809-01_REV-B", "psv2809", "PSV 2809", "P.S.V-28.09", "Line List (Rev 3)", "FSK_Batch-1", "fsk", "Обычный Документ №7", "ÉCOLE-1001", "plan & notes", "a", "A-1", "x_y", "ISO-1001_Rev-B", "İstanbul-07", "ǅ-12", "日本語-設計図-03", "_", "100%", "r&d"];
    let exts = ["pdf", "DWG", "xlsx", "zip", "i01", ""];
    let c = Catalog::new();
    c.add_root(r"\\srv\p");
    let mut every: Vec<(String, String)> = Vec::new();
    let mut n = 0usize;
    for (d, size) in [1usize, 2, 3, 5, 12, 40, 130, 700].into_iter().enumerate() {
        let dir = format!(r"\\srv\p\Rev-{d}\PSV sheets");
        let mut files = Vec::new();
        for k in 0..size {
            n += 1;
            let (stem, ext) = (stems[n % stems.len()], exts[(n / 3) % exts.len()]);
            let name = if ext.is_empty() { format!("{stem}~{k}") } else if k % 4 == 0 { format!("{stem}.{ext}") } else { format!("{stem}-{k:03}.{ext}") };
            if !files.iter().any(|f: &(String, u64, i64)| f.0 == name) {
                every.push((dir.clone(), name.clone()));
                files.push((name, k as u64, n as i64));
            }
        }
        c.apply(&Listing { dir, pass: 1, listed_at: 1, subdirs: vec![], files });
    }
    let terms = ["psv", "psv-2809", "psv2809", "psv 2809", "2809", "fsk", "rev-b", "rev b", "revb", "-1", "_", "&", "a", "x", "1", "01", "i01", "обычный", "документ7", "école", "1001", "*.zip", "*.dwg", "psv*28?9*", "?sk", "iso-??01", "日本語", "設計図03", "istanbul", "i̇stanbul", "ǆ", "100%", "r&d", "line list", "list(rev", "zzz", "07", "p.s.v", "sheets", "rev-3"];
    let mut found = 0;
    for q in terms {
        let term = t(q);
        let mut want: Vec<String> = every.iter().filter(|(_, name)| group_as_namemem(&term, &lower(name)).is_some()).map(|(dir, name)| join(dir, name)).collect();
        want.sort();
        let got = c.candidates(&term, &all(), 0);
        let groups: Vec<Option<u8>> = got.iter().map(|f| group_as_namemem(&term, &lower(&f.name))).collect();
        assert!(groups.windows(2).all(|w| w[0] <= w[1]), "{q}: groups out of order: {groups:?}");
        let mut have: Vec<String> = got.into_iter().map(|f| f.path).collect();
        have.sort();
        assert_eq!(have, want, "term {q:?}");
        found += want.len();

        // With a second word that is in every folder path, the same files.
        let mut both: Vec<String> = c.candidates_all(&[term.clone(), t("sheets")], &all(), 0).into_iter().map(|f| f.path).collect();
        both.sort();
        let mut want_both: Vec<String> = every
            .iter()
            .filter(|(dir, name)| group_as_namemem(&term, &lower(name)).is_some() || group_as_namemem(&term, &lower(dir)).is_some())
            .map(|(dir, name)| join(dir, name))
            .collect();
        want_both.sort();
        assert_eq!(both, want_both, "terms {q:?} and `sheets`");
        // With the extension limit.
        let mut dwg: Vec<String> = c.candidates(&term, &NameScope { roots: vec![], ext: Some("dwg".into()) }, 0).into_iter().map(|f| f.path).collect();
        dwg.sort();
        want.retain(|p| lower(p).ends_with(".dwg"));
        assert_eq!(dwg, want, "term {q:?} with ext dwg");
    }
    assert!(found > 2000, "the terms found {found} files in all: the test must not be vacuous");
}

#[test]
fn folders_without_the_letters_are_passed_by() {
    let c = Catalog::new();
    c.add_root(r"\\srv\p");
    for d in 0..200 {
        let files = (0..12).map(|k| (format!("14-01-15-{:04}_FV-{:04}-01_REV-A.pdf", d * 7 + k, 1000 + d), 1, 1)).collect();
        c.apply(&Listing { dir: format!(r"\\srv\p\{d}"), pass: 1, listed_at: 1, subdirs: vec![], files });
    }
    c.apply(&listing(r"\\srv\p\x", 1, 1, &[], &[("PSV-2809.pdf", 1, 1)]));
    let g = c.r();
    let want = t("psv-2809");
    let want = Want::new(&want);
    let passed = g[0].dirs.iter().filter_map(|d| d.files.as_deref()).filter(|blob| Grams::of(blob).0.has_all(&want.grams)).count();
    assert!((1..=20).contains(&passed), "{passed} of 201 folders would be read for `psv-2809`");
    drop(g);
    assert_eq!(names(&c.candidates(&t("psv 2809"), &all(), 10)), vec!["PSV-2809.pdf"]);
}
