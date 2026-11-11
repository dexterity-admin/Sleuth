//! Measures the catalog against its budget on a synthetic share:
//! `catalog_bench [files] [folder for the catalog files]`.
//!
//! Builds a root of about `files` files (default 2,000,000) with names like
//! `14-01-15-2632_PSV-2809-01_REV-B.pdf`, about 12 to a folder, folders 4
//! to 6 deep, and reports memory per file, name-search times, and the time
//! and size of writing and loading the full catalog. Run with `--release`.

use sleuth_core::analyzer::Term;
use sleuth_office::catalog::{Catalog, Listing, Mark, NameScope, RootFiles};
use std::path::PathBuf;
use std::time::Instant;

const ROOT: &str = r"\\10.0.0.5\project";

/// Working set and private bytes of this process.
#[cfg(windows)]
fn memory() -> (u64, u64) {
    #[repr(C)]
    #[derive(Default)]
    struct Counters {
        cb: u32,
        page_faults: u32,
        peak_working_set: usize,
        working_set: usize,
        quota_peak_paged: usize,
        quota_paged: usize,
        quota_peak_non_paged: usize,
        quota_non_paged: usize,
        pagefile: usize,
        peak_pagefile: usize,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(process: isize, counters: *mut Counters, cb: u32) -> i32;
    }
    let mut c = Counters { cb: size_of::<Counters>() as u32, ..Default::default() };
    // SAFETY: the structure is the documented PROCESS_MEMORY_COUNTERS and its size is passed.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    (c.working_set as u64, c.pagefile as u64)
}

#[cfg(not(windows))]
fn memory() -> (u64, u64) {
    (0, 0)
}

/// Processor seconds this process has used, all threads together, and how
/// much of that inside Windows (paging memory back in, mostly). On a PC
/// busy with other work the clock says little; this says what the work cost.
#[cfg(windows)]
fn cpu_parts() -> (f64, f64) {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn GetProcessTimes(process: isize, created: *mut u64, exited: *mut u64, kernel: *mut u64, user: *mut u64) -> i32;
    }
    let (mut created, mut exited, mut kernel, mut user) = (0u64, 0u64, 0u64, 0u64);
    // SAFETY: four FILETIMEs (64 bits each, in 100 ns units) are written to the addresses given.
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) };
    ((kernel + user) as f64 / 1e7, kernel as f64 / 1e7)
}

#[cfg(not(windows))]
fn cpu_parts() -> (f64, f64) {
    (0.0, 0.0)
}

fn cpu() -> f64 {
    cpu_parts().0
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick<'a>(&mut self, of: &[&'a str]) -> &'a str {
        of[self.below(of.len() as u64) as usize]
    }
}

const TAGS: &[&str] = &["PSV", "FV", "LT", "PT", "TE", "HV", "FIC", "PIC", "XV", "LG", "P", "E", "V", "T"];
const EXTS: &[&str] = &["pdf", "pdf", "pdf", "pdf", "pdf", "pdf", "dwg", "dwg", "xlsx", "i01", "dgn", "docx", "tif", "msg"];
const DISCIPLINES: &[&str] = &["Piping", "Process", "Instrumentation", "Electrical", "Civil & Structural", "Mechanical"];
const KINDS: &[&str] = &["Isometrics", "P&ID", "GA Drawings", "Data Sheets", "Line List", "Vendor Documents", "Calculations", "Transmittals", "As-Built", "Red-Line Markups", "Support Schedule", "MTO"];

struct Gen<'a> {
    rng: Rng,
    cat: &'a Catalog,
    files: u64,
    dirs: u64,
    name_bytes: u64,
    want: u64,
    zips: u64,
}

impl Gen<'_> {
    /// The files of one folder: runs of sheets and revisions of the same
    /// lines, as a drawing folder has them.
    fn file_names(&mut self, n: u64) -> Vec<(String, u64, i64)> {
        let (unit, area, system) = (self.rng.below(40) + 10, self.rng.below(20) + 1, self.rng.below(60) + 10);
        let mut out = Vec::with_capacity(n as usize);
        let (mut line, mut tag, mut num) = (self.rng.below(9000) + 1000, self.rng.pick(TAGS), self.rng.below(9000) + 100);
        for k in 0..n {
            if self.rng.below(3) == 0 {
                (line, tag, num) = (self.rng.below(9000) + 1000, self.rng.pick(TAGS), self.rng.below(9000) + 100);
            }
            let rev = (b'A' + self.rng.below(6) as u8) as char;
            let ext = if self.rng.below(400) == 0 {
                self.zips += 1;
                "zip"
            } else {
                self.rng.pick(EXTS)
            };
            let name = format!("{unit:02}-{area:02}-{system:02}-{line:04}_{tag}-{num:04}-{:02}_REV-{rev}.{ext}", k % 7 + 1);
            self.name_bytes += name.len() as u64;
            out.push((name, 20_000 + self.rng.below(8_000_000), 1_500_000_000 + self.rng.below(250_000_000) as i64));
        }
        self.files += n;
        out
    }

    fn folder(&mut self, dir: String, subdirs: Vec<String>, files: u64) {
        let files = self.file_names(files);
        self.dirs += 1;
        self.cat.apply(&Listing { dir, pass: 1, listed_at: 1_760_000_000, subdirs, files });
    }

    /// A folder with files, and sometimes revisions or batches below it.
    fn leaf(&mut self, dir: String, depth: u32) {
        let below: Vec<String> = match depth {
            4 if self.rng.below(100) < 35 => (0..self.rng.below(3) + 1).map(|i| format!("Rev-{}", i + 1)).collect(),
            5 if self.rng.below(100) < 30 => (0..self.rng.below(2) + 1).map(|i| format!("Batch-{:02}", i + 1)).collect(),
            _ => Vec::new(),
        };
        let n = 4 + self.rng.below(17);
        self.folder(dir.clone(), below.clone(), n);
        for b in below {
            self.leaf(format!("{dir}\\{b}"), depth + 1);
        }
    }

    fn project(&mut self, p: u64) {
        let dir = format!("{ROOT}\\P{:05}-Project {}", 12000 + p, p + 1);
        self.folder(dir.clone(), DISCIPLINES.iter().map(|d| d.to_string()).collect(), 2);
        for disc in DISCIPLINES {
            let areas: Vec<String> = (0..10).map(|a| format!("Area {:03}", (a + 1) * 10)).collect();
            self.folder(format!("{dir}\\{disc}"), areas.clone(), 1);
            for area in areas {
                let kinds: Vec<String> = (0..10).map(|k| KINDS[(k + p as usize) % KINDS.len()].to_string()).collect();
                self.folder(format!("{dir}\\{disc}\\{area}"), kinds.clone(), 0);
                for kind in kinds {
                    self.leaf(format!("{dir}\\{disc}\\{area}\\{kind}"), 4);
                }
            }
            if self.files >= self.want {
                return;
            }
        }
    }
}

/// Best and median of five runs, in ms, the processor time of the best
/// (all threads together), and the number of results.
fn time(what: &str, mut run: impl FnMut() -> usize) {
    let mut ms: Vec<(f64, f64, f64)> = Vec::new();
    let mut n = 0;
    for _ in 0..5 {
        let (t, c) = (Instant::now(), cpu_parts());
        n = run();
        let after = cpu_parts();
        ms.push((t.elapsed().as_secs_f64() * 1000.0, (after.0 - c.0) * 1000.0, (after.1 - c.1) * 1000.0));
    }
    ms.sort_by(|a, b| a.0.total_cmp(&b.0));
    let least = ms.iter().map(|m| m.1).fold(f64::MAX, f64::min);
    println!("  {what:<42} {n:>7} results   best {:>7.1} ms   median {:>7.1} ms   processor {:>7.1} ms ({:.1} in Windows), least {least:.1}", ms[0].0, ms[2].0, ms[0].1, ms[0].2);
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let want: u64 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(2_000_000);
    let out = args.get(2).map(PathBuf::from).or_else(|| std::env::var_os("SLEUTH_TEST_DIR").map(PathBuf::from)).unwrap_or_else(std::env::temp_dir).join(format!("sl-office-bench-{}", std::process::id()));

    let before = memory();
    let cat = Catalog::new();
    cat.add_root(ROOT);
    let (t, c) = (Instant::now(), cpu());
    let mut g = Gen { rng: Rng(0x9e37_79b9_7f4a_7c15), cat: &cat, files: 0, dirs: 0, name_bytes: 0, want, zips: 0 };
    let mut projects = Vec::new();
    let mut p = 0;
    while g.files < want {
        projects.push(format!("P{:05}-Project {}", 12000 + p, p + 1));
        g.project(p);
        p += 1;
    }
    cat.apply(&Listing { dir: ROOT.into(), pass: 1, listed_at: 1_760_000_001, subdirs: projects, files: vec![] });
    let (built, built_cpu) = (t.elapsed(), cpu() - c);
    cat.shrink_to_fit();
    let after = memory();
    let (files, dirs) = (cat.file_count(), cat.dir_count());
    println!("{files} files in {dirs} folders ({:.1} per folder), built in {:.1} s ({built_cpu:.1} s of processor) on a PC with {} threads", files as f64 / dirs as f64, built.as_secs_f64(), std::thread::available_parallelism().map_or(0, |n| n.get()));
    println!("names: {:.1} bytes on average", g.name_bytes as f64 / files as f64);
    println!("memory: {:.1} bytes per file resident ({:.0} MB), {:.1} bytes per file private ({:.0} MB)", (after.0 - before.0) as f64 / files as f64, (after.0 - before.0) as f64 / 1e6, (after.1 - before.1) as f64 / files as f64, (after.1 - before.1) as f64 / 1e6);

    let all = NameScope::default();
    let term = |q: &str| Term::new(q).expect("a term");
    let cap = 8000;
    println!("name searches (at most {cap} results):");
    let rare = {
        let f = cat.candidates(&term("rev-c"), &all, 1).pop().expect("a file");
        f.name[..f.name.len().min(26)].to_lowercase()
    };
    time(&format!("rare term `{rare}`"), || cat.candidates(&term(&rare), &all, cap).len());
    time("common term `psv`", || cat.candidates(&term("psv"), &all, cap).len());
    time("common term `pdf`", || cat.candidates(&term("pdf"), &all, cap).len());
    time("`*.zip`", || cat.candidates(&term("*.zip"), &all, cap).len());
    time("one letter `e`", || cat.candidates(&term("e"), &all, cap).len());
    time("one letter `q` (in no name)", || cat.candidates(&term("q"), &all, cap).len());
    time("other separators `psv 2809`", || cat.candidates(&term("psv 2809"), &all, cap).len());
    time("`psv` with ext dwg", || cat.candidates(&term("psv"), &NameScope { roots: vec![], ext: Some("dwg".into()) }, cap).len());
    time("`psv` under one project", || cat.candidates(&term("psv"), &NameScope { roots: vec![format!("{ROOT}\\P12003-Project 4")], ext: None }, cap).len());
    time("every word: `isometrics` `psv` `rev-b`", || cat.candidates_all(&[term("isometrics"), term("psv"), term("rev-b")], &all, cap).len());
    time(r"path piece `Area 030\Isometrics\`", || cat.candidates_path(r"Area 030\Isometrics\", &all, cap).len());
    time(r"path piece `Isometrics\Rev-2\14-`", || cat.candidates_path(r"Isometrics\Rev-2\14-", &all, cap).len());
    println!("  ({} .zip files in the root)", g.zips);

    let store = RootFiles::new(&out, ROOT);
    let (t, c) = (Instant::now(), cpu());
    store.write_full(&cat, Mark { pass: 1, seq: 1 })?;
    let (wrote, wrote_cpu) = (t.elapsed(), cpu() - c);
    let path = out.join("full-1-00000001.cat");
    let size = std::fs::metadata(&path)?.len();
    println!("full catalog: written in {:.2} s ({wrote_cpu:.2} s of processor) to {}, {:.1} MB ({:.1} bytes per file)", wrote.as_secs_f64(), path.display(), size as f64 / 1e6, size as f64 / files as f64);
    for round in ["first", "again"] {
        let fresh = Catalog::new();
        let (t, c) = (Instant::now(), cpu());
        let mark = store.load(&fresh)?;
        println!("full catalog: loaded ({round}) in {:.2} s ({:.2} s of processor): {} files in {} folders at {mark:?}", t.elapsed().as_secs_f64(), cpu() - c, fresh.file_count(), fresh.dir_count());
        anyhow::ensure!((fresh.file_count(), fresh.dir_count()) == (files, dirs), "the loaded catalog differs");
    }
    let _ = std::fs::remove_dir_all(&out);
    Ok(())
}
