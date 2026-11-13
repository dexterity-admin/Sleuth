//! Where a search's time goes, on a real office store:
//! `cargo run --release -p sleuth-engine --example searchprof -- <store folder> <query>...`
//!
//! For each query: the file-name answer from the catalog alone, one
//! line query per part (wall time, slowest parts, rows), and the whole
//! search, twice (the second with the parts' pages cached).

use sleuth_core::analyzer::parse_search;
use sleuth_core::paths::DriveMap;
use sleuth_engine::pool::{Pool, Presence};
use sleuth_engine::search::search_index;
use sleuth_engine::store::Scope;
use sleuth_office::{Catalog, RootFiles};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let store = PathBuf::from(args.next().expect("office store folder"));
    let t = Instant::now();
    let pool = Pool::new();
    pool.set_dirs(vec![store.join("text")]);
    pool.refresh();
    pool.recover(&|_| Presence::Unknown);
    println!("{} parts ({} MB) opened in {} ms", pool.stats().parts, pool.stats().bytes >> 20, t.elapsed().as_millis());
    let t = Instant::now();
    let catalog = Catalog::new();
    for root in std::fs::read_dir(store.join("catalog"))?.flatten() {
        let Some(state) = RootFiles::new(root.path(), "").state() else { continue };
        RootFiles::new(root.path(), &state.root).load(&catalog)?;
    }
    println!("{} files listed, loaded in {} ms", catalog.file_count(), t.elapsed().as_millis());
    let drives = DriveMap::default();
    let empty = Pool::new();
    for q in args {
        let sq = parse_search(&q);
        let t = Instant::now();
        let names = search_index(&empty, &catalog, &sq, &[], 5000, &drives)?;
        println!("\n== {q:?}\n names only (catalog): {} rows in {} ms", names.len(), t.elapsed().as_millis());
        for term in sq.all_terms() {
            let per: Mutex<Vec<(u128, String, u64, usize)>> = Mutex::new(Vec::new());
            let t = Instant::now();
            let _ = pool.each(|p, s| {
                let t1 = Instant::now();
                let n = s.search_lines(&term, &Scope::default(), 2000)?.len();
                per.lock().unwrap().push((t1.elapsed().as_millis(), p.name.clone(), p.bytes >> 20, n));
                Ok(())
            });
            let mut per = per.into_inner().unwrap();
            per.sort_by(|a, b| b.0.cmp(&a.0));
            let rows: usize = per.iter().map(|x| x.3).sum();
            let sum: u128 = per.iter().map(|x| x.0).sum();
            println!(" lines {:?}: wall {} ms, sum over parts {} ms, {} rows", term.raw, t.elapsed().as_millis(), sum, rows);
            for (ms, name, mb, n) in per.iter().take(5) {
                println!("    {ms:>6} ms  {name} ({mb} MB) {n} rows");
            }
        }
        for pass in ["cold", "warm"] {
            let t = Instant::now();
            let hits = search_index(&pool, &catalog, &sq, &[], 5000, &drives)?;
            println!(" whole search ({pass}): {} rows in {} ms", hits.len(), t.elapsed().as_millis());
        }
    }
    Ok(())
}
