//! Every format end to end, on files built here from invented content.
//!
//! Scratch files go to `SLEUTH_TEST_DIR` when it is set, else the system
//! temporary folder.

use sleuth_core::kinds::FileKind;
use sleuth_core::protocol::TextSource;
use sleuth_core::text::FileText;
use sleuth_extract::{Extracted, Limits, Tools, extract};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

// ------------------------------------------------------------- helpers

/// A scratch folder for one test, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let base = std::env::var_os("SLEUTH_TEST_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("sleuth-extract-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch folder");
        Scratch(dir)
    }

    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let p = self.0.join(name);
        std::fs::write(&p, bytes).expect("write fixture");
        p
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn read_with(path: &Path, tools: &Tools, limits: &Limits) -> anyhow::Result<Extracted> {
    let kind = FileKind::of(&path.file_name().unwrap().to_string_lossy());
    extract(path, kind, tools, limits)
}

fn text(path: &Path) -> FileText {
    match read_with(path, &Tools::default(), &Limits::default()) {
        Ok(Extracted::Text(t)) => {
            check_shape(&t);
            t
        }
        Ok(Extracted::NotHere(why)) => panic!("not here: {why}"),
        Err(e) => panic!("error: {e:#}"),
    }
}

fn error(path: &Path) -> String {
    match read_with(path, &Tools::default(), &Limits::default()) {
        Err(e) => format!("{e:#}"),
        Ok(_) => panic!("expected an error"),
    }
}

/// What every reading must satisfy, whatever the format.
fn check_shape(t: &FileText) {
    assert_eq!(t.reader_version, sleuth_core::text::READER_VERSION);
    assert!(t.model.is_empty());
    assert_eq!(t.pages.len(), t.page_info.len(), "one page_info entry per page");
    for (i, (p, info)) in t.pages.iter().zip(&t.page_info).enumerate() {
        assert_eq!(p.page, i as u32 + 1);
        assert_eq!(info.page, p.page);
        assert_eq!(info.chars as usize, p.lines.iter().map(|l| l.text.chars().count()).sum::<usize>());
        for l in &p.lines {
            assert!(!l.text.is_empty() && l.text == l.text.trim(), "no empty or padded lines: {:?}", l.text);
            assert!(!l.text.contains("  ") && !l.text.chars().any(char::is_control), "clean text: {:?}", l.text);
            assert!(l.text.chars().count() <= 1000);
            assert_eq!(l.conf, 1.0);
            assert_eq!(l.source, TextSource::TextLayer);
            assert!(!l.alt);
            let b = l.bbox;
            assert!((0.0..=1.0).contains(&b.x) && (0.0..=1.0).contains(&b.y) && b.x + b.w <= 1.0001 && b.y + b.h <= 1.0001);
        }
    }
}

fn lines(t: &FileText, page: usize) -> Vec<&str> {
    t.pages[page].lines.iter().map(|l| l.text.as_str()).collect()
}

fn zip_of(parts: &[(&str, &str)]) -> Vec<u8> {
    let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, content) in parts {
        z.start_file(*name, opts).unwrap();
        z.write_all(content.as_bytes()).unwrap();
    }
    z.finish().unwrap().into_inner()
}

const RELS_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const DOC_REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

fn root_rels(target: &str) -> String {
    format!(r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="{RELS_NS}"><Relationship Id="rId1" Type="{DOC_REL}/officeDocument" Target="{target}"/></Relationships>"#)
}

// ------------------------------------------------------------- limits

#[test]
fn kinds_handled_elsewhere_are_refused() {
    let s = Scratch::new("kinds");
    let p = s.file("drawing.pdf", b"%PDF-1.7");
    for kind in [FileKind::Pdf, FileKind::Image, FileKind::Other] {
        let e = extract(&p, kind, &Tools::default(), &Limits::default()).unwrap_err();
        assert_eq!(e.to_string(), "not handled by sleuth-extract");
    }
}

#[test]
fn limits_are_enforced() {
    let s = Scratch::new("limits");
    let body: String = (0..500).map(|i| format!("row {i} PSV-{}\n", 1000 + i)).collect();
    let p = s.file("list.txt", body.as_bytes());
    let small = Limits { max_bytes: 100, ..Default::default() };
    let e = read_with(&p, &Tools::default(), &small).unwrap_err();
    assert_eq!(e.to_string(), "too large: 1 MB");

    let few = Limits { max_lines: 20, ..Default::default() };
    let Ok(Extracted::Text(t)) = read_with(&p, &Tools::default(), &few) else { panic!("text expected") };
    check_shape(&t);
    assert_eq!(t.pages[0].lines.len(), 20);
    assert_eq!(t.page_info[0].note, "stopped after 20 lines");

    let empty = s.file("empty.txt", b"");
    assert!(error(&empty).contains("empty"));
    assert!(read_with(&s.0.join("missing.txt"), &Tools::default(), &Limits::default()).is_err());
    let d = Limits::default();
    assert_eq!((d.max_bytes, d.max_lines, d.timeout), (256 << 20, 400_000, Duration::from_secs(120)));
}

// ------------------------------------------------------------- text

#[test]
fn text_files_in_every_encoding() {
    let s = Scratch::new("text");
    let content = "ISO-1001-01  rev B\r\n\r\nPSV-1203\tØ50 – 45°\r\n   \r\nlast line";
    let want = ["ISO-1001-01 rev B", "PSV-1203 Ø50 – 45°", "last line"];

    let utf8 = s.file("a.txt", content.as_bytes());
    let mut with_bom = vec![0xEF, 0xBB, 0xBF];
    with_bom.extend_from_slice(content.as_bytes());
    let bom = s.file("b.log", &with_bom);
    let le: Vec<u8> = [0xFF, 0xFE].into_iter().chain(content.encode_utf16().flat_map(|u| u.to_le_bytes())).collect();
    let utf16le = s.file("c.txt", &le);
    let be: Vec<u8> = [0xFE, 0xFF].into_iter().chain(content.encode_utf16().flat_map(|u| u.to_be_bytes())).collect();
    let utf16be = s.file("d.txt", &be);
    let (cp1252, _, _) = encoding_rs::WINDOWS_1252.encode(content);
    let ansi = s.file("e.txt", &cp1252);
    assert!(std::str::from_utf8(&cp1252).is_err(), "the fixture really is not UTF-8");

    for p in [utf8, bom, utf16le, utf16be, ansi] {
        let t = text(&p);
        assert_eq!(lines(&t, 0), want, "{}", p.display());
        assert_eq!(t.page_info[0].class, "text");
        assert_eq!(t.page_info[0].note, "");
    }
}

#[test]
fn delimited_text_keeps_rows_together() {
    let s = Scratch::new("csv");
    let csv = "Tag,Service,,Size\r\nPSV-1203,\"Relief, to flare\",,\"2\"\"\"\r\n,,,\r\n\"P-101A\",\"Feed pump\nstandby\",x,\r\n";
    let t = text(&s.file("valves.csv", csv.as_bytes()));
    assert_eq!(lines(&t, 0), ["Tag | Service | Size", "PSV-1203 | Relief, to flare | 2\"", "P-101A | Feed pump standby | x"]);

    // semicolons, as spreadsheets write CSV in many locales, in Windows-1252
    let t = text(&s.file("semi.csv", b"Tag;Size\r\nFE-2001;\xD850\r\n"));
    assert_eq!(lines(&t, 0), ["Tag | Size", "FE-2001 | Ø50"]);

    let t = text(&s.file("list.tsv", b"ISO-1001-01\t6\"-P-1001-A1A\t\tIFC\nISO-1001-02\t\t\t\n"));
    assert_eq!(lines(&t, 0), ["ISO-1001-01 | 6\"-P-1001-A1A | IFC", "ISO-1001-02"]);
    assert_eq!(t.page_info[0].class, "text");
}

// ------------------------------------------------------------- excel

const SHEET_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";

fn xlsx(sheets: &[(&str, String)]) -> Vec<u8> {
    let mut wb = format!(r#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="{SHEET_NS}" xmlns:r="{DOC_REL}"><sheets>"#);
    let mut rels = format!(r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="{RELS_NS}">"#);
    for (i, (name, _)) in sheets.iter().enumerate() {
        let n = i + 1;
        wb.push_str(&format!(r#"<sheet name="{name}" sheetId="{n}" r:id="rId{n}"/>"#));
        rels.push_str(&format!(r#"<Relationship Id="rId{n}" Type="{DOC_REL}/worksheet" Target="worksheets/sheet{n}.xml"/>"#));
    }
    wb.push_str("</sheets></workbook>");
    rels.push_str(&format!(
        r#"<Relationship Id="rId90" Type="{DOC_REL}/sharedStrings" Target="sharedStrings.xml"/><Relationship Id="rId91" Type="{DOC_REL}/styles" Target="styles.xml"/></Relationships>"#
    ));
    let strings = format!(r#"<?xml version="1.0" encoding="UTF-8"?><sst xmlns="{SHEET_NS}" count="3" uniqueCount="3"><si><t>LINE NO</t></si><si><t xml:space="preserve">  SIZE &amp; RATING </t></si><si><r><t>ISS</t></r><r><t>UED</t></r></si></sst>"#);
    // style 1 is a date (built-in format 14)
    let styles = format!(r#"<?xml version="1.0" encoding="UTF-8"?><styleSheet xmlns="{SHEET_NS}"><cellXfs count="2"><xf numFmtId="0"/><xf numFmtId="14" applyNumberFormat="1"/></cellXfs></styleSheet>"#);
    let content_types = r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/></Types>"#;
    let names: Vec<String> = (1..=sheets.len()).map(|n| format!("xl/worksheets/sheet{n}.xml")).collect();
    let root = root_rels("xl/workbook.xml");
    let mut parts: Vec<(&str, &str)> = vec![
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", &root),
        ("xl/workbook.xml", &wb),
        ("xl/_rels/workbook.xml.rels", &rels),
        ("xl/sharedStrings.xml", &strings),
        ("xl/styles.xml", &styles),
    ];
    for (name, (_, xml)) in names.iter().zip(sheets) {
        parts.push((name, xml));
    }
    zip_of(&parts)
}

fn sheet(rows: &str) -> String {
    format!(r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="{SHEET_NS}"><sheetData>{rows}</sheetData></worksheet>"#)
}

#[test]
fn xlsx_one_page_per_sheet_one_line_per_row() {
    let s = Scratch::new("xlsx");
    let first = sheet(concat!(
        r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c><c r="D1" t="s"><v>2</v></c></row>"#,
        r#"<row r="2"><c r="A2" t="inlineStr"><is><t>6"-P-1001-A1A</t></is></c><c r="B2"><v>150</v></c><c r="C2"><v>12.5</v></c><c r="D2" s="1"><v>46095</v></c></row>"#,
        r#"<row r="3"/>"#,
        r#"<row r="5"><c r="B5" t="b"><v>1</v></c><c r="C5" t="str"><v>P-101A</v></c><c r="D5" t="e"><v>#N/A</v></c><c r="E5"><v>0.30000000000000004</v></c><c r="F5" s="1"><v>46095.4375</v></c></row>"#,
    ));
    // one row far longer than a line may be
    let cells: String = (0..300).map(|i| format!(r#"<c r="{}1" t="inlineStr"><is><t>PSV-{}</t></is></c>"#, column(i), 1000 + i)).collect();
    let second = sheet(&format!(r#"<row r="1">{cells}</row>"#));
    let p = s.file("Line list.xlsx", &xlsx(&[("Line list", first), ("Valves &amp; PSVs", second), ("Empty", sheet(""))]));
    let t = text(&p);
    assert_eq!(t.pages.len(), 3);
    assert_eq!(t.page_info.iter().map(|i| (i.class.as_str(), i.note.as_str())).collect::<Vec<_>>(), [("sheet", "Line list"), ("sheet", "Valves & PSVs"), ("sheet", "Empty")]);
    assert_eq!(lines(&t, 0), ["LINE NO | SIZE & RATING | ISSUED", "6\"-P-1001-A1A | 150 | 12.5 | 2026-03-14", "TRUE | P-101A | 0.3 | 2026-03-14T10:30:00"]);
    // 300 cells of 8 characters joined by " | " are 3,297 characters: four lines
    let long = lines(&t, 1);
    assert_eq!(long.len(), 4);
    assert!(long.iter().all(|l| l.starts_with("PSV-") && l.chars().count() <= 1000), "continued, never cut inside a tag");
    assert_eq!(long.join(" ").matches("PSV-").count(), 300);
    assert!(t.pages[2].lines.is_empty());
    assert_eq!(t.page_info[2].chars, 0);

    // the same workbook under a misleading extension is still read as what it is
    let renamed = s.file("export.xls", &std::fs::read(&p).unwrap());
    assert_eq!(lines(&text(&renamed), 0).len(), 3);
}

/// Spreadsheet column letters: 0 → A, 26 → AA.
fn column(mut i: usize) -> String {
    let mut s = String::new();
    loop {
        s.insert(0, (b'A' + (i % 26) as u8) as char);
        if i < 26 {
            return s;
        }
        i = i / 26 - 1;
    }
}

#[test]
fn workbooks_that_are_something_else() {
    let s = Scratch::new("xls-other");
    let html = "<html><body><table><tr><th>Tag</th><th>Service</th></tr><tr><td>PSV-1203</td><td>Relief &amp; vent</td></tr></table></body></html>";
    let t = text(&s.file("report.xls", html.as_bytes()));
    assert_eq!(lines(&t, 0), ["Tag | Service", "PSV-1203 | Relief & vent"]);
    assert_eq!(t.page_info[0].note, "a web page saved as a workbook");

    let t = text(&s.file("export.xls", b"Tag\tSize\r\nFE-2001\t50\r\n"));
    assert_eq!(lines(&t, 0), ["Tag | Size", "FE-2001 | 50"]);
    assert_eq!(t.page_info[0].note, "delimited text saved as a workbook");

    assert!(error(&s.file("junk.xlsx", &[0x13, 0x00, 0x00, 0x00, 0xFE, 0x00, 0x07, 0x00, 0x00, 0x00])).contains("not a workbook"));
    assert!(error(&s.file("cut.xlsx", b"PK\x03\x04 this is not a whole zip")).contains("damaged"));
}

#[test]
fn password_protected_office_files_say_so() {
    let s = Scratch::new("password");
    // An encrypted Office file is a compound file holding EncryptedPackage.
    let mut cf = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
    cf.create_stream("/EncryptionInfo").unwrap().write_all(&[4, 0, 4, 0]).unwrap();
    cf.create_stream("/EncryptedPackage").unwrap().write_all(&[0u8; 64]).unwrap();
    cf.flush().unwrap();
    let bytes = cf.into_inner().into_inner();
    for name in ["secret.xlsx", "secret.docx", "secret.pptx"] {
        assert_eq!(error(&s.file(name, &bytes)), "password protected", "{name}");
    }
}

// ------------------------------------------------------------- word

const W_NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape" xmlns:v="urn:schemas-microsoft-com:vml""#;

fn para(text: &str) -> String {
    format!("<w:p><w:r><w:t>{text}</w:t></w:r></w:p>")
}

#[test]
fn docx_paragraphs_tables_headers_notes_and_text_boxes() {
    let s = Scratch::new("docx");
    let text_box = format!(
        "<w:p><w:r><mc:AlternateContent><mc:Choice Requires=\"wps\"><wps:txbx><w:txbxContent>{}</w:txbxContent></wps:txbx></mc:Choice>\
         <mc:Fallback><v:textbox><w:txbxContent>{}</w:txbxContent></v:textbox></mc:Fallback></mc:AlternateContent></w:r><w:r><w:t>Figure 1</w:t></w:r></w:p>",
        para("HOLD: confirm PSV-1203 set pressure"),
        para("HOLD: confirm PSV-1203 set pressure")
    );
    let table = format!(
        "<w:tbl><w:tr><w:tc>{}</w:tc><w:tc>{}</w:tc></w:tr><w:tr><w:tc>{}</w:tc><w:tc><w:p/></w:tc><w:tc>{}{}</w:tc></w:tr></w:tbl>",
        para("Tag"),
        para("Service"),
        para("P-101A"),
        para("Feed pump"),
        para("(standby)")
    );
    let body = [
        "<w:p><w:pPr><w:tabs><w:tab w:val=\"left\" w:pos=\"720\"/></w:tabs></w:pPr><w:r><w:t xml:space=\"preserve\">Piping  specification </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>A1A &amp; B2B</w:t></w:r></w:p>".to_string(),
        // a line break inside a paragraph, a tab, a non-breaking hyphen
        "<w:p><w:r><w:t>Line 6\"</w:t><w:noBreakHyphen/><w:t>P-1001</w:t><w:tab/><w:t>to pump</w:t><w:br/><w:t>second line &#216;50</w:t></w:r></w:p>".to_string(),
        // tracked deletion and a field code are not text of the document
        "<w:p><w:del><w:r><w:delText>REMOVED CLAUSE</w:delText></w:r></w:del><w:r><w:instrText> PAGE </w:instrText></w:r><w:r><w:t>Kept clause</w:t></w:r></w:p>".to_string(),
        table,
        text_box,
        "<w:p><w:r><w:t></w:t></w:r></w:p>".to_string(),
    ]
    .concat();
    let document = format!(r#"<?xml version="1.0" encoding="UTF-8"?><w:document {W_NS}><w:body>{body}<w:sectPr/></w:body></w:document>"#);
    let header = format!(r#"<?xml version="1.0" encoding="UTF-8"?><w:hdr {W_NS}>{}{}</w:hdr>"#, para("Project 1001"), para("Doc SPEC-0001 rev B"));
    let footer = format!(r#"<?xml version="1.0" encoding="UTF-8"?><w:ftr {W_NS}>{}</w:ftr>"#, para("Company confidential"));
    let footnotes = format!(r#"<?xml version="1.0" encoding="UTF-8"?><w:footnotes {W_NS}><w:footnote w:id="0"><w:p/></w:footnote><w:footnote w:id="2">{}</w:footnote></w:footnotes>"#, para("See ISO-1001-01."));
    let root = root_rels("word/document.xml");
    let bytes = zip_of(&[
        ("_rels/.rels", &root),
        ("word/document.xml", &document),
        ("word/header1.xml", &header),
        // the same header again for the first page: its lines are not repeated
        ("word/header2.xml", &header),
        ("word/footer1.xml", &footer),
        ("word/footnotes.xml", &footnotes),
        ("word/styles.xml", "<w:styles/>"),
    ]);
    let t = text(&s.file("spec.docx", &bytes));
    assert_eq!(t.pages.len(), 1);
    assert_eq!(t.page_info[0].class, "document");
    assert_eq!(
        lines(&t, 0),
        [
            "Piping specification A1A & B2B",
            "Line 6\"-P-1001 to pump",
            "second line Ø50",
            "Kept clause",
            "Tag | Service",
            "P-101A | Feed pump (standby)",
            "HOLD: confirm PSV-1203 set pressure",
            "Figure 1",
            "Project 1001",
            "Doc SPEC-0001 rev B",
            "Company confidential",
            "See ISO-1001-01.",
        ]
    );
    // a macro-enabled document is the same format
    assert_eq!(lines(&text(&s.file("spec.docm", &bytes)), 0).len(), 12);
    // and so is one that was given the old extension
    assert_eq!(lines(&text(&s.file("spec.doc", &bytes)), 0).len(), 12);

    assert!(error(&s.file("no-body.docx", &zip_of(&[("_rels/.rels", &root), ("word/styles.xml", "<w:styles/>")]))).contains("no document part"));
}

#[test]
fn damaged_docx_xml_is_reported_in_the_note() {
    let s = Scratch::new("docx-cut");
    let document = format!(r#"<?xml version="1.0" encoding="UTF-8"?><w:document {W_NS}><w:body>{}<w:p><w:r><w:t>cut he</w:body>"#, para("First paragraph"));
    let t = text(&s.file("cut.docx", &zip_of(&[("word/document.xml", &document)])));
    assert_eq!(lines(&t, 0)[0], "First paragraph");
    assert!(t.page_info[0].note.contains("damaged"), "{}", t.page_info[0].note);
}

// ------------------------------------------------------------- slides

const P_NS: &str = r#"xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;

fn shape(paras: &[&str]) -> String {
    let body: String = paras.iter().map(|t| format!("<a:p><a:r><a:rPr lang=\"en-GB\"/><a:t>{t}</a:t></a:r></a:p>")).collect();
    format!("<p:sp><p:txBody><a:bodyPr/>{body}</p:txBody></p:sp>")
}

fn slide(shapes: &str) -> String {
    format!(r#"<?xml version="1.0" encoding="UTF-8"?><p:sld {P_NS}><p:cSld><p:spTree>{shapes}</p:spTree></p:cSld></p:sld>"#)
}

#[test]
fn pptx_one_page_per_slide_in_show_order_with_notes() {
    let s = Scratch::new("pptx");
    let presentation = format!(r#"<?xml version="1.0" encoding="UTF-8"?><p:presentation {P_NS}><p:sldIdLst><p:sldId id="257" r:id="rId3"/><p:sldId id="256" r:id="rId2"/></p:sldIdLst></p:presentation>"#);
    let pres_rels = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="{RELS_NS}"><Relationship Id="rId1" Type="{DOC_REL}/slideMaster" Target="slideMasters/slideMaster1.xml"/><Relationship Id="rId2" Type="{DOC_REL}/slide" Target="slides/slide1.xml"/><Relationship Id="rId3" Type="{DOC_REL}/slide" Target="slides/slide2.xml"/></Relationships>"#
    );
    // slide1.xml is shown second
    let table = "<p:graphicFrame><a:graphic><a:graphicData><a:tbl><a:tr><a:tc><a:txBody><a:p><a:r><a:t>PSV-1203</a:t></a:r></a:p></a:txBody></a:tc><a:tc><a:txBody><a:p><a:r><a:t>Relief</a:t></a:r><a:br/><a:r><a:t>valve</a:t></a:r></a:p></a:txBody></a:tc></a:tr></a:tbl></a:graphicData></a:graphic></p:graphicFrame>";
    let number = "<p:sp><p:txBody><a:p><a:fld id=\"{1}\" type=\"slidenum\"><a:t>2</a:t></a:fld></a:p></p:txBody></p:sp>";
    let slide1 = slide(&format!("{}{table}{number}", shape(&["Valve list", "Unit 1001"])));
    let slide2 = slide(&shape(&["Kick-off  meeting", "Scope: isometrics ISO-1001-01 to -40"]));
    let slide1_rels = format!(r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="{RELS_NS}"><Relationship Id="rId1" Type="{DOC_REL}/notesSlide" Target="../notesSlides/notesSlide1.xml"/></Relationships>"#);
    let notes = format!(r#"<?xml version="1.0" encoding="UTF-8"?><p:notes {P_NS}><p:cSld><p:spTree>{}{number}</p:spTree></p:cSld></p:notes>"#, shape(&["Ask about the spare PSV."]));
    let root = root_rels("ppt/presentation.xml");
    let bytes = zip_of(&[
        ("_rels/.rels", &root),
        ("ppt/presentation.xml", &presentation),
        ("ppt/_rels/presentation.xml.rels", &pres_rels),
        ("ppt/slides/slide1.xml", &slide1),
        ("ppt/slides/slide2.xml", &slide2),
        ("ppt/slides/_rels/slide1.xml.rels", &slide1_rels),
        ("ppt/notesSlides/notesSlide1.xml", &notes),
    ]);
    let t = text(&s.file("kickoff.pptx", &bytes));
    assert_eq!(t.page_info.iter().map(|i| (i.class.as_str(), i.note.as_str())).collect::<Vec<_>>(), [("slide", "Slide 1"), ("slide", "Slide 2")]);
    assert_eq!(lines(&t, 0), ["Kick-off meeting", "Scope: isometrics ISO-1001-01 to -40"]);
    assert_eq!(lines(&t, 1), ["Valve list", "Unit 1001", "PSV-1203 | Relief valve", "Ask about the spare PSV."]);
    assert_eq!(text(&s.file("kickoff.pptm", &bytes)).pages.len(), 2);

    // without a slide list the slide parts are taken by number
    let bare = zip_of(&[("ppt/presentation.xml", "<p:presentation/>"), ("ppt/slides/slide2.xml", &slide2), ("ppt/slides/slide10.xml", &slide1), ("ppt/slides/slide1.xml", &slide2)]);
    let t = text(&s.file("bare.pptx", &bare));
    assert_eq!(t.pages.len(), 3);
    assert_eq!(lines(&t, 2)[0], "Valve list");
}

// ------------------------------------------------------------- email

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

struct Msg(cfb::CompoundFile<Cursor<Vec<u8>>>);

impl Msg {
    /// `fixed`: (property id, type, 8 value bytes) entries of the property stream.
    fn new(fixed: &[(u16, u16, [u8; 8])]) -> Msg {
        let mut cf = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        let mut props = vec![0u8; 32];
        for (id, ty, value) in fixed {
            props.extend_from_slice(&ty.to_le_bytes());
            props.extend_from_slice(&id.to_le_bytes());
            props.extend_from_slice(&6u32.to_le_bytes());
            props.extend_from_slice(value);
        }
        cf.create_stream("/__properties_version1.0").unwrap().write_all(&props).unwrap();
        Msg(cf)
    }

    fn stream(mut self, path: &str, bytes: &[u8]) -> Msg {
        if let Some((dir, _)) = path.rsplit_once('/')
            && !dir.is_empty()
            && !self.0.is_storage(dir)
        {
            self.0.create_storage(dir).unwrap();
        }
        self.0.create_stream(path).unwrap().write_all(bytes).unwrap();
        self
    }

    fn bytes(mut self) -> Vec<u8> {
        self.0.flush().unwrap();
        self.0.into_inner().into_inner()
    }
}

#[test]
fn msg_headers_body_and_attachment_names() {
    let s = Scratch::new("msg");
    // 2026-03-14 10:30:00 UTC as a FILETIME
    let sent = ((1_773_484_200u64 + 11_644_473_600) * 10_000_000).to_le_bytes();
    let m = Msg::new(&[(0x0039, 0x0040, sent), (0x3FFD, 0x0003, [0xE4, 0x04, 0, 0, 0, 0, 0, 0])])
        .stream("/__substg1.0_0037001F", &utf16("RE: ISO-1001-01 – hold on PSV-1203\0"))
        .stream("/__substg1.0_0C1A001F", &utf16("A. Engineer"))
        .stream("/__substg1.0_0C1F001F", &utf16("/O=EXAMPLE/OU=EXCHANGE/CN=RECIPIENTS/CN=AENGINEER"))
        .stream("/__substg1.0_5D01001F", &utf16("a.engineer@example.com"))
        .stream("/__substg1.0_0E04001F", &utf16("B. Checker; C. Lead"))
        .stream("/__substg1.0_0E03001F", &utf16("Document Control"))
        .stream("/__substg1.0_1000001F", &utf16("Hello,\r\n\r\nPlease hold line 6\"-P-1001-A1A.\r\n> earlier reply\r\n"))
        .stream("/__attach_version1.0_#00000000/__substg1.0_3707001F", &utf16("ISO-1001-01 Rev B.pdf"))
        .stream("/__attach_version1.0_#00000000/__substg1.0_3704001F", &utf16("ISO-10~1.PDF"))
        .stream("/__attach_version1.0_#00000001/__substg1.0_3001001F", &utf16("FW: earlier message"));
    let t = text(&s.file("hold.msg", &m.bytes()));
    assert_eq!(t.page_info[0].class, "email");
    assert_eq!(
        lines(&t, 0),
        [
            "Subject: RE: ISO-1001-01 – hold on PSV-1203",
            "From: A. Engineer <a.engineer@example.com>",
            "To: B. Checker; C. Lead",
            "Cc: Document Control",
            "Date: 2026-03-14 10:30:00 UTC",
            "Hello,",
            "Please hold line 6\"-P-1001-A1A.",
            "> earlier reply",
            "Attachment: ISO-1001-01 Rev B.pdf",
            "Attachment: FW: earlier message",
        ]
    );
}

#[test]
fn msg_html_only_and_rtf_only_bodies() {
    let s = Scratch::new("msg-bodies");
    // 8-bit strings in the message code page (1252), the body only as HTML
    let m = Msg::new(&[(0x3FFD, 0x0003, [0xE4, 0x04, 0, 0, 0, 0, 0, 0]), (0x3FDE, 0x0003, [0xE4, 0x04, 0, 0, 0, 0, 0, 0])])
        .stream("/__substg1.0_0037001E", b"Flange \xD850")
        .stream("/__substg1.0_0C1A001E", b"sender@example.com")
        .stream("/__substg1.0_0C1F001E", b"sender@example.com")
        .stream("/__substg1.0_10130102", b"<html><body><p>Size is \xD850.</p><div>Regards</div></body></html>");
    let t = text(&s.file("html.msg", &m.bytes()));
    assert_eq!(lines(&t, 0), ["Subject: Flange Ø50", "From: sender@example.com", "Size is Ø50.", "Regards"]);
    assert_eq!(t.page_info[0].note, "");

    let m = Msg::new(&[]).stream("/__substg1.0_0037001F", &utf16("Only RTF")).stream("/__substg1.0_10090102", &[0x10, 0, 0, 0, 0x20, 0, 0, 0, b'L', b'Z', b'F', b'u']);
    let t = text(&s.file("rtf.msg", &m.bytes()));
    assert_eq!(lines(&t, 0), ["Subject: Only RTF"]);
    assert_eq!(t.page_info[0].note, "the body is compressed RTF and was not read");

    // a compound file that is not a message
    let mut cf = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
    cf.create_stream("/WordDocument").unwrap().write_all(b"x").unwrap();
    cf.flush().unwrap();
    assert!(error(&s.file("not.msg", &cf.into_inner().into_inner())).contains("not an Outlook message"));
}

#[test]
fn eml_headers_parts_and_attachment_names() {
    let s = Scratch::new("eml");
    let eml = concat!(
        "Received: from mail.example.com\r\n",
        "From: =?UTF-8?Q?A=2E_Engineer?= <a.engineer@example.com>\r\n",
        "To: \"B. Checker\" <b.checker@example.com>,\r\n",
        "\tc.lead@example.com\r\n",
        "Cc: doc.control@example.com\r\n",
        "Subject: =?iso-8859-1?Q?Flange_=D850_?=\r\n",
        " =?iso-8859-1?Q?for_ISO-1001-01?=\r\n",
        "Date: Sat, 14 Mar 2026 10:30:00 +0000\r\n",
        "MIME-Version: 1.0\r\n",
        "Content-Type: multipart/mixed; boundary=\"outer\"\r\n",
        "\r\n",
        "preamble, not shown\r\n",
        "--outer\r\n",
        "Content-Type: multipart/alternative; boundary=inner\r\n",
        "\r\n",
        "--inner\r\n",
        "Content-Type: text/plain; charset=\"iso-8859-1\"\r\n",
        "Content-Transfer-Encoding: quoted-printable\r\n",
        "\r\n",
        "Size is =D850 at 45=B0, see line 6\"-P-1001-A1A which is =\r\n",
        "continued here.\r\n",
        "\r\n",
        "Regards\r\n",
        "--inner\r\n",
        "Content-Type: text/html; charset=utf-8\r\n",
        "\r\n",
        "<p>HTML twin, not used</p>\r\n",
        "--inner--\r\n",
        "--outer\r\n",
        "Content-Type: application/pdf; name=\"=?UTF-8?B?SVNPLTEwMDEtMDEgUmV2IEIucGRm?=\"\r\n",
        "Content-Disposition: attachment; filename=\"=?UTF-8?B?SVNPLTEwMDEtMDEgUmV2IEIucGRm?=\"\r\n",
        "Content-Transfer-Encoding: base64\r\n",
        "\r\n",
        "JVBERi0xLjcK\r\n",
        "--outer\r\n",
        "Content-Type: image/png\r\n",
        "Content-Disposition: inline; filename*=utf-8''logo%20small.png\r\n",
        "\r\n",
        "xx\r\n",
        "--outer--\r\n",
        "epilogue\r\n",
    );
    let t = text(&s.file("flange.eml", eml.as_bytes()));
    assert_eq!(t.page_info[0].class, "email");
    assert_eq!(
        lines(&t, 0),
        [
            "Subject: Flange Ø50 for ISO-1001-01",
            "From: A. Engineer <a.engineer@example.com>",
            "To: \"B. Checker\" <b.checker@example.com>, c.lead@example.com",
            "Cc: doc.control@example.com",
            "Date: Sat, 14 Mar 2026 10:30:00 +0000",
            "Size is Ø50 at 45°, see line 6\"-P-1001-A1A which is continued here.",
            "Regards",
            "Attachment: ISO-1001-01 Rev B.pdf",
            "Attachment: logo small.png",
        ]
    );

    // one part, base64, UTF-8, bare line feeds
    let single = "Subject: Minutes\nFrom: pm@example.com\nContent-Type: text/plain; charset=utf-8\nContent-Transfer-Encoding: base64\n\nUFNWLTEyMDMg4oCTIG9rCnNlY29uZCBsaW5l\n";
    assert_eq!(lines(&text(&s.file("single.eml", single.as_bytes())), 0), ["Subject: Minutes", "From: pm@example.com", "PSV-1203 – ok", "second line"]);

    // only an HTML body: its text, without the tags
    let html = "Subject: Report\r\nContent-Type: text/html\r\n\r\n<html><body><h1>Weekly</h1><p>Line&nbsp;list issued.</p></body></html>\r\n";
    assert_eq!(lines(&text(&s.file("html.eml", html.as_bytes())), 0), ["Subject: Report", "Weekly", "Line list issued."]);

    // no MIME at all
    let plain = "From: a@example.com\r\nSubject: Short\r\n\r\nBody line one.\r\nBody line two.\r\n";
    assert_eq!(lines(&text(&s.file("plain.eml", plain.as_bytes())), 0), ["Subject: Short", "From: a@example.com", "Body line one.", "Body line two."]);

    assert!(error(&s.file("not.eml", b"just some words\r\nwith no headers\r\n")).contains("not an email"));
}

// ------------------------------------------------------------- dxf

fn dxf(groups: &[(i32, &str)]) -> Vec<u8> {
    groups.iter().map(|(code, value)| format!("{code:>3}\r\n{value}\r\n")).collect::<String>().into_bytes()
}

#[test]
fn ascii_dxf_text_with_positions() {
    let s = Scratch::new("dxf");
    let chunk = "A".repeat(250);
    let groups: Vec<(i32, &str)> = vec![
        (0, "SECTION"),
        (2, "HEADER"),
        (9, "$ACADVER"),
        (1, "AC1015"),
        (9, "$DWGCODEPAGE"),
        (3, "ANSI_1252"),
        (9, "$EXTMIN"),
        (10, "0.0"),
        (20, "0.0"),
        (30, "0.0"),
        (9, "$EXTMAX"),
        (10, "400.0"),
        (20, "200.0"),
        (30, "0.0"),
        (0, "ENDSEC"),
        (0, "SECTION"),
        (2, "BLOCKS"),
        (0, "BLOCK"),
        (2, "TITLE"),
        (0, "ATTDEF"),
        (10, "1.0"),
        (20, "1.0"),
        (1, ""),
        (3, "Drawing number?"),
        (2, "DWGNO"),
        (0, "TEXT"),
        (10, "100.0"),
        (20, "150.0"),
        (1, "TEXT INSIDE A BLOCK"),
        (0, "ENDBLK"),
        (0, "ENDSEC"),
        (0, "SECTION"),
        (2, "ENTITIES"),
        (0, "LINE"),
        (10, "0.0"),
        (20, "0.0"),
        (11, "5.0"),
        (21, "5.0"),
        (0, "TEXT"),
        (8, "NOTES"),
        (10, "100.0"),
        (20, "150.0"),
        (30, "0.0"),
        (40, "2.5"),
        (1, "PSV-1203 %%c50 45%%d"),
        (0, "MTEXT"),
        (10, "200.0"),
        (20, "100.0"),
        (3, &chunk),
        (1, "\\PSEE {\\fArial|b1;ISO-1001-01}\\P\\A1;LINE 6\"-P-1001-A1A"),
        (0, "INSERT"),
        (2, "TITLE"),
        (10, "350.0"),
        (20, "10.0"),
        (66, "1"),
        (0, "ATTRIB"),
        (10, "300.0"),
        (20, "50.0"),
        (1, "ISO-1001-01 REV B"),
        (2, "DWGNO"),
        (0, "SEQEND"),
        (0, "TEXT"),
        (67, "1"),
        (10, "100.0"),
        (20, "150.0"),
        (1, "PAPER SPACE TITLE"),
        (0, "DIMENSION"),
        (10, "0.0"),
        (20, "0.0"),
        (11, "40.0"),
        (21, "20.0"),
        (1, "<> TYP"),
        (0, "DIMENSION"),
        (11, "40.0"),
        (21, "20.0"),
        (1, ""),
        (0, "MULTILEADER"),
        (304, "{\\LSEE DETAIL A}"),
        (0, "TEXT"),
        (10, "9000.0"),
        (20, "9000.0"),
        (1, "FAR OUTSIDE THE EXTENTS"),
        (0, "ENDSEC"),
        (0, "EOF"),
    ];
    let t = text(&s.file("plan.dxf", &dxf(&groups)));
    assert_eq!(t.page_info[0].class, "model");
    assert_eq!(t.page_info[0].note, "");
    let got: Vec<(&str, bool)> = t.pages[0].lines.iter().map(|l| (l.text.as_str(), l.bbox.x != 0.0 || l.bbox.y != 0.0)).collect();
    assert_eq!(
        got,
        [
            ("DWGNO", false),
            ("TEXT INSIDE A BLOCK", false),
            ("PSV-1203 Ø50 45°", true),
            (chunk.as_str(), true),
            ("SEE ISO-1001-01", true),
            ("LINE 6\"-P-1001-A1A", true),
            ("ISO-1001-01 REV B", true),
            ("PAPER SPACE TITLE", false),
            ("TYP", true),
            ("SEE DETAIL A", false),
            ("FAR OUTSIDE THE EXTENTS", false),
        ]
    );
    // (100, 150) in extents 400 x 200, y measured from the top
    let b = t.pages[0].lines[2].bbox;
    assert!((b.x - 0.25).abs() < 1e-5 && (b.y - 0.25).abs() < 1e-5, "{b:?}");

    // the same file found under the wrong extension is still DXF
    let as_dwg = s.file("plan.dwg", &dxf(&groups));
    assert_eq!(text(&as_dwg).pages[0].lines.len(), 11);
}

#[test]
fn dxf_without_extents_uses_the_text_points_and_bad_files_say_why() {
    let s = Scratch::new("dxf-bad");
    let groups = [(0, "SECTION"), (2, "ENTITIES"), (0, "TEXT"), (10, "10.0"), (20, "10.0"), (1, "LOW LEFT"), (0, "TEXT"), (10, "110.0"), (20, "60.0"), (1, "HIGH RIGHT"), (0, "ENDSEC"), (0, "EOF")];
    let t = text(&s.file("bare.dxf", &dxf(&groups)));
    let boxes: Vec<(f32, f32)> = t.pages[0].lines.iter().map(|l| (l.bbox.x, l.bbox.y)).collect();
    assert_eq!(boxes, [(0.0, 1.0), (1.0, 0.0)]);

    // Windows-1252 text in an old DXF
    let mut old = dxf(&[(0, "SECTION"), (2, "ENTITIES"), (0, "TEXT"), (10, "1"), (20, "1")]);
    old.extend_from_slice(b"  1\r\nFLANGE \xD850\r\n  0\r\nENDSEC\r\n  0\r\nEOF\r\n");
    assert_eq!(lines(&text(&s.file("old.dxf", &old)), 0), ["FLANGE Ø50"]);

    assert_eq!(error(&s.file("binary.dxf", b"AutoCAD Binary DXF\r\n\x1a\0\0\0")), "binary DXF is not supported");
    assert!(error(&s.file("words.dxf", b"this is\r\nnot a drawing\r\n")).contains("not a DXF"));

    // cut off in the middle: what was read, with a note
    let mut cut = dxf(&[(0, "SECTION"), (2, "ENTITIES"), (0, "TEXT"), (10, "1"), (20, "1"), (1, "BEFORE THE CUT"), (0, "TEXT")]);
    cut.extend_from_slice(b" 10");
    let t = text(&s.file("cut.dxf", &cut));
    assert_eq!(lines(&t, 0), ["BEFORE THE CUT"]);
    assert!(t.page_info[0].note.contains("damaged"), "{}", t.page_info[0].note);
}

// ------------------------------------------------------------- dwg

#[test]
fn dwg_without_libredwg_is_left_for_another_pc() {
    let s = Scratch::new("dwg-none");
    let p = s.file("plan.dwg", b"AC1027\0\0\0\0\0\0 not a real drawing");
    match read_with(&p, &Tools { dwgread: None }, &Limits::default()) {
        Ok(Extracted::NotHere(why)) => assert_eq!(why, "AutoCAD files need LibreDWG, which is not installed with this copy"),
        other => panic!("expected NotHere, got {:?}", other.map(|_| ())),
    }
    let gone = Tools { dwgread: Some(s.0.join("no-such-dwgread.exe")) };
    assert!(matches!(read_with(&p, &gone, &Limits::default()), Ok(Extracted::NotHere(_))));
    assert!(!sleuth_extract::abilities(&gone).dwg);
    assert!(!sleuth_extract::abilities(&Tools::default()).dwg);
}

#[test]
fn dwg_through_libredwg_when_it_is_installed() {
    let tools = Tools::find();
    let Some(dwgread) = tools.dwgread.clone() else {
        eprintln!("skipped: vendor/libredwg is not present (run scripts/fetch-vendor.ps1)");
        return;
    };
    assert!(sleuth_extract::abilities(&tools).dwg);
    let s = Scratch::new("dwg");
    // A version newer than any LibreDWG reads is refused before it is run.
    let newer = s.file("future.dwg", b"AC1099\0\0\0\0\0\0\0\0\0\0");
    let e = read_with(&newer, &tools, &Limits::default()).unwrap_err().to_string();
    assert!(e.contains("newer DWG version than LibreDWG reads"), "{e}");
    let junk = s.file("junk.dwg", b"\x01\x02\x03\x04\x05\x06\x07\x08 nothing like a drawing");
    assert!(read_with(&junk, &tools, &Limits::default()).unwrap_err().to_string().contains("not an AutoCAD drawing"));
    // The right first bytes but nothing behind them: LibreDWG calls that a
    // success with nothing in it, which is not handed on as an empty reading.
    let hollow = s.file("hollow.dwg", &[b"AC1015".as_slice(), &[0u8; 200]].concat());
    let e = read_with(&hollow, &tools, &Limits::default()).unwrap_err().to_string();
    assert_eq!(e, "damaged: LibreDWG found no objects in this drawing (version 2000)");
    // A drawing cut short: LibreDWG's failure, said plainly.
    let whole: &[u8] = include_bytes!("fixtures/sample.dwg");
    let cut = s.file("cut.dwg", &whole[..2500]);
    let e = read_with(&cut, &tools, &Limits::default()).unwrap_err().to_string();
    assert_eq!(e, "LibreDWG could not read this drawing (version 2000): damaged or not a valid DWG (LibreDWG error 0x800)");

    // A drawing written by LibreDWG's own dwgadd from invented content.
    let sample = s.file("sample.dwg", include_bytes!("fixtures/sample.dwg"));
    let Ok(Extracted::Text(t)) = read_with(&sample, &tools, &Limits::default()) else { panic!("text expected") };
    check_shape(&t);
    assert_eq!(t.page_info[0].class, "model");
    let got = lines(&t, 0);
    for want in ["ISO-1001-01", "PSV-1203 Ø50 45°", "GENERAL NOTES", "1. ALL DIMENSIONS IN MM", "2. LINE 6in-P-1001-A1A TO PUMP P-101A", "ISO-1001-01 REV B"] {
        assert!(got.contains(&want), "missing {want:?} in {got:?}");
    }
    // (300, 20) in extents 420 x 297
    let l = t.pages[0].lines.iter().find(|l| l.text == "ISO-1001-01").unwrap();
    assert!((l.bbox.x - 300.0 / 420.0).abs() < 1e-4 && (l.bbox.y - (1.0 - 20.0 / 297.0)).abs() < 1e-4, "{:?}", l.bbox);

    // The deadline kills the process.
    let instant = Limits { timeout: Duration::ZERO, ..Default::default() };
    let e = read_with(&sample, &Tools { dwgread: Some(dwgread) }, &instant).unwrap_err().to_string();
    assert_eq!(e, "timed out");
}

// ------------------------------------------------------------- dgn

/// A 32-bit integer the way V7 stores it: high word first.
fn mid(v: u32) -> [u8; 4] {
    let b = v.to_le_bytes();
    [b[2], b[3], b[0], b[1]]
}

/// A design coordinate as stored in a range (sign bit flipped).
fn rng(v: i32) -> [u8; 4] {
    mid((v as u32) ^ 0x8000_0000)
}

struct Elem {
    ty: u8,
    level: u8,
    complex: bool,
    deleted: bool,
    lo: (i32, i32),
    hi: (i32, i32),
    body: Vec<u8>,
}

impl Elem {
    fn new(ty: u8, lo: (i32, i32), hi: (i32, i32), body: Vec<u8>) -> Elem {
        Elem { ty, level: 1, complex: false, deleted: false, lo, hi, body }
    }

    fn complex(mut self) -> Elem {
        self.complex = true;
        self
    }

    fn deleted(mut self) -> Elem {
        self.deleted = true;
        self
    }

    fn bytes(&self) -> Vec<u8> {
        let mut e = vec![self.level | if self.complex { 0x80 } else { 0 }, self.ty | if self.deleted { 0x80 } else { 0 }, 0, 0];
        for v in [self.lo.0, self.lo.1, 0, self.hi.0, self.hi.1, 0] {
            e.extend_from_slice(&rng(v));
        }
        // graphic group, attribute index, properties, symbology
        e.extend_from_slice(&[0u8; 8]);
        e.extend_from_slice(&self.body);
        if e.len() % 2 == 1 {
            e.push(0);
        }
        let words = (e.len() / 2 - 2) as u16;
        e[2..4].copy_from_slice(&words.to_le_bytes());
        e
    }
}

/// The body of a text element (type 17) after the common header.
fn text_body(s: &[u8], origin: (i32, i32), three_d: bool) -> Vec<u8> {
    let mut b = vec![3u8, 0]; // font, justification
    b.extend_from_slice(&mid(1000)); // length multiplier
    b.extend_from_slice(&mid(1000)); // height multiplier
    if three_d {
        b.extend_from_slice(&[0u8; 16]); // quaternion
        b.extend_from_slice(&mid(origin.0 as u32));
        b.extend_from_slice(&mid(origin.1 as u32));
        b.extend_from_slice(&mid(0));
    } else {
        b.extend_from_slice(&mid(0)); // rotation
        b.extend_from_slice(&mid(origin.0 as u32));
        b.extend_from_slice(&mid(origin.1 as u32));
    }
    b.push(s.len() as u8);
    b.push(0); // enter-data fields
    b.extend_from_slice(s);
    b
}

fn text_elem(s: &[u8], lo: (i32, i32), hi: (i32, i32), three_d: bool) -> Elem {
    Elem::new(17, lo, hi, text_body(s, lo, three_d))
}

/// The design-file header: type 9 on level 8, 766 words after the header.
fn tcb(three_d: bool) -> Vec<u8> {
    let mut e = vec![0u8; 1536];
    e[0] = if three_d { 0xC8 } else { 0x08 };
    e[1] = 9;
    e[2..4].copy_from_slice(&766u16.to_le_bytes());
    if three_d {
        e[1214] |= 0x40;
    }
    e
}

fn rad50(s: &str) -> [u8; 2] {
    let code = |c: u8| -> u16 {
        match c {
            b' ' => 0,
            b'A'..=b'Z' => u16::from(c - b'A') + 1,
            b'$' => 27,
            b'.' => 28,
            b'0'..=b'9' => u16::from(c - b'0') + 30,
            _ => panic!("not Radix-50"),
        }
    };
    let b = s.as_bytes();
    (code(b[0]) * 1600 + code(b[1]) * 40 + code(b[2])).to_le_bytes()
}

fn design(three_d: bool, elems: &[Elem]) -> Vec<u8> {
    let mut f = tcb(three_d);
    for e in elems {
        f.extend_from_slice(&e.bytes());
    }
    f.extend_from_slice(&[0xFF, 0xFF]);
    f.resize(f.len().next_multiple_of(512), 0);
    f
}

#[test]
fn v7_dgn_element_stream() {
    let s = Scratch::new("dgn");
    // a cell header: total length word, then the name in Radix-50
    let mut cell = vec![0u8, 0];
    cell.extend_from_slice(&rad50("GAT"));
    cell.extend_from_slice(&rad50("EVL"));
    cell.extend_from_slice(&[0u8; 60]);
    // a tag element: value type 1 (string) at offset 74, the value at 154
    let mut tag = vec![0u8; 154 - 36];
    tag[74 - 36] = 1;
    tag.extend_from_slice(b"TAG-VALUE-7\0");
    let mut number_tag = vec![0u8; 154 - 36];
    number_tag[74 - 36] = 3;
    number_tag.extend_from_slice(&[42, 0, 0, 0]);
    let elems = [
        // the sheet border sets the extents: 0..4000 by 0..2000
        Elem::new(4, (0, 0), (4000, 2000), vec![0u8; 20]),
        text_elem(b"ISO-1001-01", (3000, 100), (3600, 200), false),
        text_elem(b"PSV-1203 \xD850", (1000, 1500), (1400, 1560), false),
        // a text node and its two lines (complex components)
        Elem::new(7, (200, 800), (900, 1000), vec![0u8; 40]),
        text_elem(b"GENERAL NOTES", (200, 920), (900, 1000), false).complex(),
        text_elem(b"1. ALL DIMENSIONS IN MM", (200, 800), (900, 880), false).complex(),
        text_elem(b"DELETED TEXT", (100, 100), (200, 200), false).deleted(),
        text_elem(b"P-101A", (500, 500), (600, 540), false),
        Elem::new(2, (2000, 1000), (2100, 1100), cell.clone()),
        text_elem(b"IN THE CELL", (2000, 1000), (2100, 1040), false).complex(),
        // the same cell again: its name is listed once
        Elem::new(2, (2500, 1000), (2600, 1100), cell),
        Elem::new(37, (300, 300), (400, 340), tag),
        Elem::new(37, (300, 400), (400, 440), number_tag),
        // a shared-cell definition: its text has the cell's own coordinates
        Elem::new(34, (0, 0), (0, 0), vec![0u8; 40]),
        text_elem(b"SYMBOL TEXT", (-900_000, -900_000), (-899_000, -899_900), false).complex(),
        text_elem(b"   ", (10, 10), (20, 20), false),
        // two-byte text: FF FD, then 16-bit characters
        text_elem(&[0xFF, 0xFD, b'O', 0, b'K', 0], (700, 700), (760, 740), false),
    ];
    let t = text(&s.file("1001-01.i01", &design(false, &elems)));
    assert_eq!(t.pages.len(), 1);
    assert_eq!(t.page_info[0].class, "model");
    assert_eq!(
        lines(&t, 0),
        ["ISO-1001-01", "PSV-1203 Ø50", "GENERAL NOTES", "1. ALL DIMENSIONS IN MM", "P-101A", "GATEVL", "IN THE CELL", "TAG-VALUE-7", "SYMBOL TEXT", "OK"]
    );
    // range (3000..3600, 100..200) in a sheet of 4000 x 2000, y from the top
    let b = t.pages[0].lines[0].bbox;
    assert!((b.x - 0.75).abs() < 1e-5 && (b.y - 0.9).abs() < 1e-5 && (b.w - 0.15).abs() < 1e-5 && (b.h - 0.05).abs() < 1e-5, "{b:?}");
    let symbol = t.pages[0].lines[8].bbox;
    assert_eq!((symbol.x, symbol.y, symbol.w, symbol.h), (0.0, 0.0, 0.0, 0.0), "text of a definition has no place");
    assert!(t.page_info[0].note.contains("two-byte text"));

    // the same content as .dgn reads the same
    assert_eq!(lines(&text(&s.file("model.dgn", &design(false, &elems))), 0).len(), 10);
}

#[test]
fn v7_dgn_three_dimensional_and_damaged() {
    let s = Scratch::new("dgn-3d");
    let elems = [Elem::new(3, (-1000, -1000), (1000, 1000), vec![0u8; 24]), text_elem(b"EL +100.500", (0, 0), (500, 100), true), text_elem(b"N 2500.000", (-1000, 900), (-400, 1000), true)];
    let whole = design(true, &elems);
    let t = text(&s.file("model.dgn", &whole));
    assert_eq!(lines(&t, 0), ["EL +100.500", "N 2500.000"]);
    let b = t.pages[0].lines[0].bbox;
    assert!((b.x - 0.5).abs() < 1e-5 && (b.y - 0.45).abs() < 1e-5, "{b:?}");
    assert_eq!(t.page_info[0].note, "");

    // cut in the middle of the last element: what came before, and a note
    let end = 1536 + elems[0].bytes().len() + elems[1].bytes().len() + 20;
    let t = text(&s.file("cut.dgn", &whole[..end]));
    assert_eq!(lines(&t, 0), ["EL +100.500"]);
    assert!(t.page_info[0].note.contains("the file ends inside element"), "{}", t.page_info[0].note);

    assert!(error(&s.file("zip.dgn", b"PK\x03\x04 a zip under a drawing name")).contains("not a MicroStation design file"));
    assert!(error(&s.file("words.i01", b"IP plain words, nothing more")).contains("not a MicroStation design file"));
}

#[test]
fn v8_dgn_goes_to_the_filter_or_another_pc() {
    // A compound file under .dgn is a V8 design. With no filter on this PC it
    // is left for one that has it; with a filter the outcome is the filter's.
    let s = Scratch::new("dgn-v8");
    let mut cf = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
    cf.create_stream("/Dgn-Md").unwrap().write_all(&[0u8; 16]).unwrap();
    cf.flush().unwrap();
    let p = s.file("model.dgn", &cf.into_inner().into_inner());
    if !sleuth_extract::abilities(&Tools::default()).dgn_filter {
        match read_with(&p, &Tools::default(), &Limits::default()) {
            Ok(Extracted::NotHere(why)) => assert_eq!(why, "MicroStation V8 files need Bentley's text filter, which this PC does not have"),
            other => panic!("expected NotHere, got {:?}", other.map(|_| ())),
        }
    }
}
