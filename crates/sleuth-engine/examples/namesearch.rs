//! Run name and text searches against an office store, the way the worker
//! does: every part of its `text` folder, and the file lists of its
//! `catalog` folder.
//! `cargo run --release -p sleuth-engine --example namesearch -- <store folder> <query>...`
//! Mapped drive `T:` -> `\\10.0.0.54\docs-latest` is assumed for
//! pasted drive-letter paths.

use sleuth_core::analyzer::parse_search;
use sleuth_core::paths::{Drive, DriveKind, DriveMap};
use sleuth_engine::pool::{Pool, Presence};
use sleuth_engine::search::search_index;
use sleuth_office::{Catalog, RootFiles};
use std::path::PathBuf;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let store = PathBuf::from(args.next().expect("office store folder"));
    let t = Instant::now();
    let pool = Pool::new();
    pool.set_dirs(vec![store.join("text")]);
    pool.refresh();
    pool.recover(&|_| Presence::Unknown);
    let catalog = Catalog::new();
    for root in std::fs::read_dir(store.join("catalog"))?.flatten() {
        let Some(state) = RootFiles::new(root.path(), "").state() else { continue };
        RootFiles::new(root.path(), &state.root).load(&catalog)?;
    }
    println!("{} parts, {} files listed, opened in {} ms", pool.stats().parts, catalog.file_count(), t.elapsed().as_millis());
    let drives = DriveMap { drives: vec![Drive { letter: "T:".into(), kind: DriveKind::Network, unc: Some(r"\\10.0.0.54\docs-latest".into()) }] };
    for q in args {
        let t = Instant::now();
        let hits = search_index(&pool, &catalog, &parse_search(&q), &[], 5000, &drives)?;
        println!("== {q:?}: {} hits in {} ms", hits.len(), t.elapsed().as_millis());
        for h in hits.iter().take(6) {
            println!("   {:?} {:?} {} | {}", h.class, h.source, h.name, h.why);
        }
    }
    Ok(())
}
