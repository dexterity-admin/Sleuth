//! Sleuth engine: reading documents (text layers, extractors and OCR), the
//! office index (catalog and parts pool), drive walking and search. Runs
//! inside `sleuth-worker.exe` and `sleuth-server.exe`.

pub mod accel;
pub mod cache;
pub mod crawl;
pub mod derive;
pub mod indexing;
pub mod models;
pub mod ocr;
pub mod pdf;
pub mod pool;
pub mod reader;
pub mod reading;
pub mod readjob;
pub mod remote;
pub mod search;
pub mod service;
pub mod store;
pub mod sys;
pub mod text;
pub mod usn;
pub mod views;
pub mod winocr;
pub mod workflows;
