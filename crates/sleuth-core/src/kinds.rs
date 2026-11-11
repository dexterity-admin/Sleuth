//! File types Sleuth reads inside, decided by the file name alone (a walk
//! never opens a file), and the category each is shown under.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    Pdf,
    /// .xlsx .xlsm .xlsb .xls .ods
    Excel,
    /// .docx .docm .doc
    Word,
    /// .pptx .pptm .ppt
    Slides,
    /// .msg .eml
    Email,
    /// .txt .csv .tsv .log
    Text,
    /// .dwg .dxf
    Dwg,
    /// .dgn and PDS isometric natives (.i01 … .i99)
    Dgn,
    /// .tif .tiff .jpg .jpeg .png .bmp (read by OCR)
    Image,
    /// Found by name only.
    #[default]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Drawing,
    Document,
    Spreadsheet,
    Email,
    Presentation,
    #[default]
    Other,
}

/// Lower-case extension without the dot (`""` when there is none).
pub fn extension(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 8 && !ext.contains(['\\', '/', ' ']) => ext.to_lowercase(),
        _ => String::new(),
    }
}

impl FileKind {
    pub fn of(name: &str) -> FileKind {
        let ext = extension(name);
        match ext.as_str() {
            "pdf" => FileKind::Pdf,
            "xlsx" | "xlsm" | "xlsb" | "xls" | "ods" => FileKind::Excel,
            "docx" | "docm" | "doc" => FileKind::Word,
            "pptx" | "pptm" | "ppt" => FileKind::Slides,
            "msg" | "eml" => FileKind::Email,
            "txt" | "csv" | "tsv" | "log" => FileKind::Text,
            "dwg" | "dxf" => FileKind::Dwg,
            "dgn" => FileKind::Dgn,
            "tif" | "tiff" | "jpg" | "jpeg" | "png" | "bmp" => FileKind::Image,
            // PDS isometric natives: `.i01`, `.i02`, … (MicroStation V7).
            e if e.len() == 3 && e.starts_with('i') && e[1..].bytes().all(|b| b.is_ascii_digit()) => FileKind::Dgn,
            _ => FileKind::Other,
        }
    }

    /// Whether Sleuth reads text inside this type.
    pub fn readable(self) -> bool {
        self != FileKind::Other
    }

    /// Whether reading it always means OCR (the slow lane).
    pub fn needs_ocr(self) -> bool {
        self == FileKind::Image
    }

    pub fn label(self) -> &'static str {
        match self {
            FileKind::Pdf => "pdf",
            FileKind::Excel => "excel",
            FileKind::Word => "word",
            FileKind::Slides => "slides",
            FileKind::Email => "email",
            FileKind::Text => "text",
            FileKind::Dwg => "dwg",
            FileKind::Dgn => "dgn",
            FileKind::Image => "image",
            FileKind::Other => "other",
        }
    }

    /// The category a file of this type is shown under. A PDF is a document
    /// unless its reading says it is a drawing (`is_drawing`: a title block
    /// was found, or a page is A3 or larger).
    pub fn category(self, is_drawing: bool) -> Category {
        match self {
            FileKind::Dwg | FileKind::Dgn => Category::Drawing,
            FileKind::Pdf | FileKind::Image if is_drawing => Category::Drawing,
            FileKind::Pdf | FileKind::Image | FileKind::Word | FileKind::Text => Category::Document,
            FileKind::Excel => Category::Spreadsheet,
            FileKind::Email => Category::Email,
            FileKind::Slides => Category::Presentation,
            FileKind::Other => Category::Other,
        }
    }
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Drawing => "drawing",
            Category::Document => "document",
            Category::Spreadsheet => "spreadsheet",
            Category::Email => "email",
            Category::Presentation => "presentation",
            Category::Other => "other",
        }
    }
}

/// A file name without its extension, lower case, for pairing the same
/// document saved as several types (`ISO-1001.pdf`, `ISO-1001.dwg`).
pub fn stem_key(name: &str) -> String {
    let ext = extension(name);
    let stem = if ext.is_empty() { name } else { &name[..name.len() - ext.len() - 1] };
    stem.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_by_name() {
        assert_eq!(FileKind::of("P1001-ISO.PDF"), FileKind::Pdf);
        assert_eq!(FileKind::of("line list.XLSX"), FileKind::Excel);
        assert_eq!(FileKind::of("14-01-15-2632.i01"), FileKind::Dgn);
        assert_eq!(FileKind::of("14-01-15-2632.I12"), FileKind::Dgn);
        assert_eq!(FileKind::of("notes.ini"), FileKind::Other);
        assert_eq!(FileKind::of("model.dgn"), FileKind::Dgn);
        assert_eq!(FileKind::of("plan.dwg"), FileKind::Dwg);
        assert_eq!(FileKind::of("RE scope.msg"), FileKind::Email);
        assert_eq!(FileKind::of("scan.TIF"), FileKind::Image);
        assert_eq!(FileKind::of("no extension"), FileKind::Other);
        assert_eq!(FileKind::of(".gitignore"), FileKind::Other);
        assert_eq!(FileKind::of("Ölleitung €.pdf"), FileKind::Pdf);
    }

    #[test]
    fn stems_pair_types() {
        assert_eq!(stem_key("ISO-1001_Rev-B.pdf"), stem_key("iso-1001_rev-b.DWG"));
        assert_ne!(stem_key("ISO-1001.pdf"), stem_key("ISO-1002.pdf"));
        assert_eq!(stem_key("README"), "readme");
    }

    #[test]
    fn categories() {
        assert_eq!(FileKind::Pdf.category(true), Category::Drawing);
        assert_eq!(FileKind::Pdf.category(false), Category::Document);
        assert_eq!(FileKind::Dgn.category(false), Category::Drawing);
        assert_eq!(FileKind::Excel.category(false), Category::Spreadsheet);
    }
}
