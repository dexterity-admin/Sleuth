//! Sleuth engine: reading PDFs (text layer and OCR), the local index,
//! drive walking and search. Runs inside `sleuth-worker.exe`.

pub mod cache;
pub mod crawl;
pub mod models;
pub mod ocr;
pub mod pdf;
pub mod reader;
pub mod search;
pub mod service;
pub mod store;
pub mod sys;
pub mod text;
pub mod winocr;
