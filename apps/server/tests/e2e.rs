//! End to end: a real server on 127.0.0.1 over HTTPS, a project folder with
//! one text PDF, and the desktop's own client.
//!
//! One test, because the engine's data folder is process-wide
//! (`SLEUTH_DATA_DIR`). Two projects sit side by side as `proj` and
//! `project2`, so a prefix comparison would leak between them.

use sleuth_core::settings::Project;
use sleuth_engine::remote::Client;
use sleuth_server::config::{Config, Role, UserCfg, hash_key};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DEMO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/line-list.pdf");

fn fresh(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
}

/// GET with a key, returning status and JSON body (for checking error codes).
fn raw(ca: &[u8], url: &str, key: &str) -> (u16, serde_json::Value) {
    let cert = ureq::tls::Certificate::from_pem(ca).unwrap();
    let tls = ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::Specific(std::sync::Arc::new(vec![cert]))).build();
    let agent: ureq::Agent = ureq::Agent::config_builder().tls_config(tls).http_status_as_error(false).build().into();
    let mut req = agent.get(url);
    if !key.is_empty() {
        req = req.header("Authorization", &format!("Bearer {key}"));
    }
    let mut r = req.call().unwrap();
    let status = r.status().as_u16();
    let body = r.body_mut().read_to_string().unwrap();
    (status, serde_json::from_str(&body).unwrap_or(serde_json::Value::Null))
}

#[test]
fn office_server_end_to_end() {
    let base: PathBuf = std::env::temp_dir().join(format!("sleuth-server-e2e-{}", std::process::id()));
    let (data, proj, proj2) = (base.join("data"), base.join("share").join("proj"), base.join("share").join("project2"));
    fresh(&base);
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(&proj2).unwrap();
    std::fs::copy(DEMO, proj.join("Line list rev 3.pdf")).expect("copy the demo PDF");
    std::fs::write(proj2.join("notes.txt"), "nothing to see").unwrap();

    let s = |p: &Path| p.display().to_string();
    let cfg = Config {
        listen: "127.0.0.1:0".into(),
        projects: vec![Project { name: "P1".into(), roots: vec![s(&proj)] }, Project { name: "P2".into(), roots: vec![s(&proj2)] }],
        users: vec![
            UserCfg { name: "ana".into(), token_sha256: hash_key("slk_ana"), role: Role::User, projects: vec!["P1".into()] },
            UserCfg { name: "ben".into(), token_sha256: hash_key("slk_ben"), role: Role::User, projects: vec!["P2".into()] },
            UserCfg { name: "cai".into(), token_sha256: hash_key("slk_cai"), role: Role::User, projects: vec!["P1".into()] },
            UserCfg { name: "it".into(), token_sha256: hash_key("slk_it"), role: Role::Admin, projects: vec!["*".into()] },
        ],
        reader: serde_json::from_str(r#"{"ocr_backend":"cpu","name_rescan_minutes":15}"#).unwrap(),
        ..Default::default()
    };
    cfg.save(&data).unwrap();
    sleuth_server::tls::ensure_ca(&data).unwrap();
    sleuth_server::tls::issue_server_cert(&data, &[]).unwrap();
    sleuth_server::use_data_dir(&data);

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let running = rt.block_on(sleuth_server::start(&data, Some("127.0.0.1:0".parse().unwrap()))).expect("server starts");
    let url = format!("https://127.0.0.1:{}", running.addr.port());
    let ca = std::fs::read(data.join("ca.pem")).unwrap();

    let ana = Client::new(&url, "slk_ana", &ca).unwrap();
    let ben = Client::new(&url, "slk_ben", &ca).unwrap();
    let cai = Client::new(&url, "slk_cai", &ca).unwrap();

    // health needs no key; everything else does
    let (st, body) = raw(&ca, &format!("{url}/api/v1/health"), "");
    assert_eq!((st, body["status"].as_str()), (200, Some("ok")));
    let (st, body) = raw(&ca, &format!("{url}/api/v1/capabilities"), "");
    assert_eq!((st, body["code"].as_str()), (401, Some("unauthorized")));
    let (st, body) = raw(&ca, &format!("{url}/api/v1/capabilities"), "slk_wrong");
    assert_eq!((st, body["code"].as_str()), (401, Some("unauthorized")));
    assert!(Client::new(&url, "slk_wrong", &ca).unwrap().capabilities().is_err());

    let caps = ana.capabilities().unwrap();
    assert_eq!(caps.api_version, 1);
    assert_eq!((caps.user.as_str(), caps.role.as_str()), ("ana", "user"));
    assert_eq!(caps.projects, vec!["P1".to_string()]);
    assert_eq!(caps.roots, vec![s(&proj)]);

    // wait for the PDF to be read (text layer)
    let started = Instant::now();
    let hit = loop {
        let r = ana.search("P-1001", None, 50, 0).unwrap();
        if let Some(h) = r.hits.into_iter().find(|h| h.source == sleuth_core::protocol::TextSource::TextLayer) {
            break h;
        }
        assert!(started.elapsed() < Duration::from_secs(300), "the PDF was not read within 5 minutes");
        std::thread::sleep(Duration::from_secs(2));
    };
    assert!(hit.path.to_lowercase().ends_with("line list rev 3.pdf"));
    let doc = hit.doc_id.expect("hits carry the server's document id");
    assert_eq!(ana.lookup(&hit.path).unwrap().id, doc);

    // the page image with the match marked
    let page = ana.page(doc, 1, "P-1001", None, 800).unwrap();
    assert!(page.pages >= 1 && page.width > 0 && page.height > 0 && !page.png_base64.is_empty());
    assert!(!page.boxes.is_empty(), "the match is marked on the page");
    let d = ana.document(doc).unwrap();
    assert_eq!(d.id, doc);
    assert!(!d.pages.is_empty());
    // page 0 is refused, max_px is clamped
    let (st, body) = raw(&ca, &format!("{url}/api/v1/documents/{doc}/pages/0"), "slk_ana");
    assert_eq!((st, body["code"].as_str()), (400, Some("bad_request")));
    let (st, body) = raw(&ca, &format!("{url}/api/v1/documents/{doc}/pages/1?max_px=99999"), "slk_ana");
    assert_eq!(st, 200);
    assert!(body["width"].as_u64().unwrap() <= 3000 && body["height"].as_u64().unwrap() <= 3000);
    let (st, body) = raw(&ca, &format!("{url}/api/v1/search?q={}", "x".repeat(501)), "slk_ana");
    assert_eq!((st, body["code"].as_str()), (400, Some("bad_request")));

    // a project-limited search gives the same hit; another's project is refused
    assert_eq!(ana.search("P-1001", Some("P1"), 50, 0).unwrap().hits.len(), 1);
    let e = ana.search("P-1001", Some("P2"), 50, 0).unwrap_err();
    assert!(format!("{e:#}").contains("not allowed"), "{e:#}");
    let (st, body) = raw(&ca, &format!("{url}/api/v1/search?q=P-1001&project=Nope"), "slk_ana");
    assert_eq!((st, body["code"].as_str()), (403, Some("forbidden")));

    // the user without the project: no hits, and the document id is refused
    assert!(ben.search("P-1001", None, 50, 0).unwrap().hits.is_empty());
    assert!(ben.search("Line list", None, 50, 0).unwrap().hits.is_empty());
    assert!(ben.tags(None, None, None, 100, 0).unwrap().0.is_empty());
    assert!(ben.occurrences("P1001", None).map(|v| v.is_empty()).unwrap_or(true));
    for path in [format!("/documents/{doc}"), format!("/documents/{doc}/pages/1"), format!("/documents/{doc}/related"), format!("/documents/{doc}/tags"), format!("/compare?old={doc}&new={doc}")] {
        let (st, body) = raw(&ca, &format!("{url}/api/v1{path}"), "slk_ben");
        assert_eq!((st, body["code"].as_str()), (403, Some("forbidden")), "{path}");
    }
    assert!(format!("{:#}", ben.page(doc, 1, "", None, 800).unwrap_err()).contains("not allowed"));
    assert!(ben.lookup(&hit.path).is_err());
    let (st, body) = raw(&ca, &format!("{url}/api/v1/documents/999999999"), "slk_ana");
    assert_eq!((st, body["code"].as_str()), (404, Some("not_found")));

    // administrators only
    let (st, body) = raw(&ca, &format!("{url}/api/v1/monitor"), "slk_ana");
    assert_eq!((st, body["code"].as_str()), (403, Some("forbidden")));
    let (st, _) = raw(&ca, &format!("{url}/api/v1/monitor"), "slk_it");
    assert_eq!(st, 200);
    let (st, body) = raw(&ca, &format!("{url}/api/v1/audit"), "slk_it");
    assert_eq!(st, 200);
    let rows = body.as_array().unwrap();
    assert!(rows.iter().any(|r| r["user"] == "ben" && r["result"].as_str().unwrap().starts_with("forbidden")));
    assert!(rows.iter().any(|r| r["result"] == "unauthorized: unknown access key"));
    assert!(!body.to_string().contains("slk_"), "keys never reach the audit log");

    // two clients on the same index get the same answer
    let a = ana.search("P-1001", None, 50, 0).unwrap();
    let c = cai.search("P-1001", None, 50, 0).unwrap();
    assert_eq!(serde_json::to_value(&a.hits).unwrap(), serde_json::to_value(&c.hits).unwrap());
    assert_eq!(a.more, c.more);

    rt.block_on(running.stop());
    drop(rt);
    let _ = std::fs::remove_dir_all(&base);
}
