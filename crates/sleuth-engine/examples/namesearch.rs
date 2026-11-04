//! Run name/text searches against a copy of an index, the way the worker does.
//! `cargo run --release -p sleuth-engine --example namesearch -- <index.db> <query>...`
//! Mapped drive `T:` -> `\\10.0.0.54\docs-latest` is assumed for
//! pasted drive-letter paths.

use sleuth_core::analyzer::parse_search;
use sleuth_core::paths::{Drive, DriveKind, DriveMap};
use sleuth_engine::search::search_index;
use sleuth_engine::store::Store;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let db = args.next().expect("index path");
    let store = Store::open(std::path::Path::new(&db))?;
    let drives = DriveMap { drives: vec![Drive { letter: "T:".into(), kind: DriveKind::Network, unc: Some(r"\\10.0.0.54\docs-latest".into()) }] };
    for q in args {
        let t = Instant::now();
        let hits = search_index(&store, &parse_search(&q), &[], 5000, &drives)?;
        let all = t.elapsed().as_millis();
        let pq = parse_search(&q);
        let t2 = Instant::now();
        let n: usize = pq.groups.iter().flatten().map(|t| store.search_names(t, &Default::default(), 2000).map(|v| v.len()).unwrap_or(0)).sum();
        println!("== {q:?}: {} hits in {all} ms (names alone: {n} in {} ms)", hits.len(), t2.elapsed().as_millis());
        for h in hits.iter().take(6) {
            println!("   {:?} {:?} {} | {}", h.class, h.source, h.name, h.why);
        }
    }
    Ok(())
}
