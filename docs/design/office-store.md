# Sleuth 1.1: the office store

Working design for 1.1. The approved plan is the "what"; this is the "how",
and the contract between the crates. Keep it current while building.

## Why

1.0 kept a full SQLite index per PC (5.8 GB for 7.4 million files), every PC
walked every share itself, and only finished PDF readings were shared. 1.1
keeps one office index in the shared folder (default `U:\Sleuth-cache`) and
nothing large on a PC.

## The rule that makes a network share safe

No file in the store is ever modified after it is visible. Every file is
written under a temporary name and renamed into place; a folder listing is
the manifest. SQLite files in the store are opened read-only with
`immutable=1`, which takes no locks. There is no live database on the share.

## Layout

```
<store>\
  v2\<hh>\<key>.dhx | .claim     per-document OCR readings by content hash (1.0 format, kept)
  catalog\<rootkey>\             file-name catalog of one share
      full-<pass>-<seq>.cat      whole catalog after segment <seq> of pass <pass>
      seg-<pass>-<seq>-<pc>.cat  folder listings since the previous segment
      state.json                 who walks it, progress, last complete pass
  text\<name>.db                 search parts (SQLite, write-once): read documents
  text\<name>.retired            marker: merged into a newer part
  pcs\<pc>.json                  heartbeat: version, what the PC is doing, speed
  pcs\<pc>\text\<name>.db       parts for files on that PC's own disks (only it opens them)
  leases\<name>.lease            walk-<rootkey>, compact, doc\<hh>\<pathkey>
```

A PC keeps under `%LOCALAPPDATA%\Sleuth` only: `local.db` (history, events,
change-journal checkpoints), `work.db` and `work-local.db` (readings not
yet published, same schema as a part), `catalog\local~<letter>\` (the
catalog of its own fixed drives, same format).

## Paths

Everything stored is a canonical path: `\\<ipv4>\<share>\…` for network
files (host names are resolved to IPv4 once and remembered, so every PC
writes a share the same way), `C:\…` for local files. `rootkey` is the
canonical root made file-name safe (`10.0.0.5~project`, `local~c`).
Comparisons are case-insensitive.

## Crates

| Crate | Holds |
|---|---|
| `sleuth-core` | protocol, settings, analyzer, paths, `text` (FileText), `kinds` (FileKind, Category) |
| `sleuth-office` | `lease` (leases on a share), `catalog` (in-memory names + on-disk format) |
| `sleuth-extract` | text from Excel, Word, PowerPoint, email, text, DWG, DGN; Windows IFilter |
| `sleuth-lanes` | reader processes (job object, idle priority, timeouts) and the governor |
| `sleuth-engine` | store (SQLite schema), parts pool, readers, search, the service |

## `sleuth-office::lease` (contract)

```rust
pub struct Identity { pub host: String, pub user: String, pub pid: u32 }
impl Identity { pub fn current() -> Identity }

pub struct Leases { /* dir, me */ }
pub struct Lease { /* held; Drop releases it if still ours */ }
pub struct Holder { pub host: String, pub user: String, pub pid: u32, pub until: i64 }
pub enum Acquire { Mine(Lease), HeldBy(Holder), Unavailable(String) }

impl Leases {
    pub fn new(dir: impl Into<PathBuf>, me: Identity) -> Leases;
    /// Take `<dir>\<name>.lease` for `ttl`.
    pub fn acquire(&self, name: &str, ttl: Duration) -> Acquire;
    /// Who holds it now, if anyone (expired holders are not reported).
    pub fn holder(&self, name: &str) -> Option<Holder>;
}
impl Lease {
    /// Push the expiry out by the original ttl. False when the lease was lost
    /// (expired and taken over): the caller must stop the work it guards.
    pub fn renew(&mut self) -> bool;
    pub fn release(self);
    pub fn name(&self) -> &str;
}
```

Rules:
- Created with `create_new`; content is JSON `{host,user,pid,token,until}`.
- Renew = write a temp file, then rename over the lease, only after reading
  the lease back and seeing our own token.
- Take-over of an expired lease = rename it to `<name>.<token>.dead` (only
  one PC can win the rename), delete that, then `create_new`.
- A leftover of this same host+user with another pid is ours to take (one
  worker runs per user per PC).
- Expiry is judged on the file server's clock, not each PC's: the lease
  file's modified time against the modified time of a file this PC has just
  written to the same folder. PC clocks may differ by minutes.
- Any I/O error other than "exists" is `Unavailable`; nothing panics.

## `sleuth-office::catalog` (contract)

One `Catalog` holds every root this PC can see; all methods take `&self`
(interior `RwLock`), and it is shared as `Arc<Catalog>`.

```rust
pub struct FileInfo { pub path: String, pub name: String, pub size: u64, pub mtime: i64 }

/// One folder as listed. `dir` is the full canonical folder path.
pub struct Listing {
    pub dir: String,
    /// Walk pass that listed it (resumption); 0 for a one-off relist.
    pub pass: i64,
    pub listed_at: i64,
    pub subdirs: Vec<String>,            // names
    pub files: Vec<(String, u64, i64)>,  // name, size, mtime
}

pub struct Change { pub files_added: u32, pub files_changed: u32, pub files_removed: u32, pub dirs_removed: u32 }

pub struct NameScope { pub roots: Vec<String>, pub ext: Option<String> }

impl Catalog {
    pub fn new() -> Catalog;
    /// Register a root (`\\10.0.0.5\project` or `C:\`); listings and lookups
    /// outside every root are ignored.
    pub fn add_root(&self, root: &str);
    pub fn remove_root(&self, root: &str);
    pub fn roots(&self) -> Vec<String>;
    /// Replace one folder's contents. Files and subfolders no longer listed
    /// go (subfolders with everything under them). A listing older than what
    /// the folder already holds (`listed_at`) is ignored.
    pub fn apply(&self, l: &Listing) -> Change;
    pub fn file_count(&self) -> u64;
    pub fn dir_count(&self) -> u64;
    pub fn root_counts(&self, root: &str) -> (u64, u64); // dirs, files
    /// Bumped on every change that altered anything.
    pub fn generation(&self) -> u64;
    pub fn get(&self, path: &str) -> Option<FileInfo>;
    pub fn contains_dir(&self, dir: &str) -> bool;
    /// Subfolder paths when `dir` was listed in `pass` (walk resumption).
    pub fn listed_in_pass(&self, dir: &str, pass: i64) -> Option<Vec<String>>;
    /// Name search, as `namemem` did: whole names first, then the text as
    /// typed, then other separators / patterns; newer first within each.
    pub fn candidates(&self, term: &Term, scope: &NameScope, cap: usize) -> Vec<FileInfo>;
    pub fn candidates_all(&self, terms: &[Term], scope: &NameScope, cap: usize) -> Vec<FileInfo>;
    /// Every file under `folder` (a root or any folder below one), in no
    /// promised order. `f` returns false to stop.
    pub fn for_each_under(&self, folder: &str, f: &mut dyn FnMut(&str /*dir*/, &str /*name*/, u64, i64) -> bool);
    /// Files with the same name stem in the same folder and in sibling
    /// folders (children of the same parent), the file itself excluded.
    pub fn twins(&self, path: &str) -> Vec<FileInfo>;
}
```

On disk (`RootFiles`, one folder per root):

```rust
pub struct Mark { pub pass: i64, pub seq: u32 }
pub struct RootState { pub root: String, pub pass: i64, pub phase: String /* walking|done|partial|offline */,
    pub by: String, pub started: i64, pub updated: i64, pub last_ok: i64,
    pub folders: u64, pub files: u64, pub skipped: u64, pub note: String }

impl RootFiles {
    pub fn new(dir: impl Into<PathBuf>, root: &str) -> RootFiles;
    /// Load the newest full catalog and every later segment into `cat`
    /// (the root is added). Returns the mark reached; `Mark{0,0}` when empty.
    pub fn load(&self, cat: &Catalog) -> Result<Mark>;
    /// Apply what other PCs published since `mark`. Returns listings applied.
    pub fn follow(&self, cat: &Catalog, mark: &mut Mark) -> Result<usize>;
    /// Start writing segments for `pass`, continuing after the last one.
    pub fn writer(&self, pc: &str, pass: i64) -> Result<SegmentWriter>;
    /// Write the whole root from memory as a full catalog at `mark` and
    /// delete the files it replaces (older than one hour).
    pub fn write_full(&self, cat: &Catalog, mark: Mark) -> Result<()>;
    pub fn state(&self) -> Option<RootState>;
    pub fn set_state(&self, s: &RootState) -> Result<()>;
}
impl SegmentWriter {
    pub fn add(&mut self, l: &Listing);
    pub fn pending(&self) -> usize;
    /// Write what was added as one segment file (temp + rename). Returns its mark.
    pub fn flush(&mut self) -> Result<Option<Mark>>;
}
```

- Files are gzip; inside, a one-line JSON header then length-prefixed binary
  listing records with paths relative to the root. A truncated or damaged
  file is skipped with a warning, never a panic; a reader that finds a newer
  full catalog than its mark reloads from it.
- Budget: 7.5 million files in memory in under 60 bytes per file on
  average; `candidates` over all of them in under one second on 8 threads;
  loading a 2-million-file root from its full catalog in under 10 seconds
  from a local disk.

## `sleuth-extract` (contract)

```rust
pub struct Tools { pub dwgread: Option<PathBuf> }           // bundled LibreDWG
pub struct Limits { pub max_bytes: u64, pub max_lines: usize, pub timeout: Duration }
pub struct Abilities { pub office_filter: bool, pub dgn_filter: bool, pub dwg: bool }
pub enum Extracted {
    Text(FileText),
    /// This PC lacks what the file needs (no Office filter, no LibreDWG):
    /// leave it for a PC that has it. The string says what is missing.
    NotHere(String),
}
pub fn abilities(tools: &Tools) -> Abilities;
/// PDF and Image are not handled here (pdfium and OCR live in the engine).
pub fn extract(path: &Path, kind: FileKind, tools: &Tools, limits: &Limits) -> Result<Extracted>;
```

- One `PageText` per sheet / slide / document / model; `PageInfo.class` is
  `sheet`, `slide`, `document`, `email`, `text` or `model`, and `note` holds
  the sheet or slide name.
- A spreadsheet row is one line (cells joined with ` | `), so a tag and its
  description stay together; lines are capped at 1,000 characters.
- Lines come from the file itself: `conf` 1.0, `source` `TextLayer`.
  CAD text keeps its place (`bbox`) normalised to the design extents where
  the format gives one.
- Errors are plain: password protected, damaged, too large, timed out.
  Nothing panics on a damaged file; third-party code (IFilter, LibreDWG)
  runs with a timeout.

## Engine (as built)

Files in `crates/sleuth-engine/src`: `service.rs` (state, settings, status,
search, previews), `indexing.rs` (stage 1), `reading.rs` (stage 2),
`readjob.rs` (what a reader process does), `pool.rs` (parts), `views.rs`
(tags, revisions, corrections, monitor), `store.rs` (the SQLite schema).

- **Catalog** (`indexing.rs`). The holder of the lease `walk-<rootkey>`
  walks a share with `crawl.rs`, applies each folder listing to the
  in-memory catalog and publishes segments every minute; at the end it
  writes a full catalog and `state.json`. Every PC follows the segments
  (`follow_loop`, every minute). A PC's own disks use the same code with
  the catalog folder on that PC and no lease. The office server lists only
  its projects' folders: it publishes segments (pass 0) without a lease and
  without claiming the share is listed.
- **Parts pool** (`pool.rs`). `work.db` and `work-local.db` (this PC's
  unpublished readings of share files and of its own disks) plus every
  `text\*.db` and `pcs\<pc>\text\*.db`, opened read-only and immutable.
  There is no manifest file and no `.cov` file: the folder listing is the
  manifest, and what a part covers is read from its `files` table once
  when it is opened. In each part a `TEMP VIEW files` hides readings a
  newer part replaced, readings of files that are gone, and
  waiting-for-OCR markers, so every query works unchanged.
- **Coverage**. `path -> (part, row, size, mtime, state, read_at)`, newest
  reading wins. States: `READ`, `FAILED`, `NEEDS_OCR` (opened, has scanned
  pages; any PC's OCR lane may take it without opening it again).
- **Queue** (`reading.rs::scan`, on the stats thread). The catalog under
  the reading folders, minus coverage, minus files deferred on this PC.
  With several PCs on, each takes first the folders whose key falls to it
  (`key % PCs`), newest files first.
- **One reader per document**. Before a share file is handed to a reader
  the worker takes the lease `doc\<hh>\<pathkey>`; it keeps it until the
  reading is published and three more minutes (every PC looks for new
  parts each minute), then releases it. OCR results are also still shared
  by content hash in `v2\` (`.dhx`, `.claim`), which 1.0 PCs understand.
- **Readers** (`readjob.rs`, crate `sleuth-lanes`). Child processes
  `sleuth-worker --reader fast|ocr` at idle priority in one job object.
  Fast lane: text-layer PDFs and every extractor. OCR lane: scanned pages
  and images. The governor (`sleuth-lanes::Governor`) sets how many of each
  run, every pass of the loop.
- **Publishing**. `VACUUM INTO` a temporary name in the text folder, check
  (`quick_check`, counts), rename, open in the pool, then empty `work.db`:
  every 500 documents, or ten minutes after the oldest unpublished one.
- **Merging**. The holder of the lease `compact` merges small parts (tiers
  under 32 MB, 256 MB, 2 GB) into one, writes `.retired` markers, and
  retired parts are deleted a day later. A merge keeps readings of files
  that are gone (hiding them is undone if the file comes back).
- **Corrections**. A published part never changes, so correcting a line
  copies the reading into `work.db`, corrects it there and publishes it as
  the newer reading.
- **Upgrade from 1.0**. `index.db` readings missing from the office index
  are published; then the file is deleted.
- **Ids**. A line, correction or document id that travels to the window or
  the server's clients is `part slot << 40 | row id` (`pool::compose`).
