//! Guided setup: what `sleuth-server.exe` does when it is double-clicked (or
//! run as `sleuth-server setup`). The first run sets the server up step by
//! step; later runs offer a menu. Every step says what it did, and the end of
//! the first run shows exactly what to type on each desktop: in practice only
//! the access key, because setup leaves the address and the certificate in the
//! office index folder, where Sleuth on every PC finds them.

use anyhow::{Context, Result, bail};
use sleuth_core::protocol::OfficeServerCard;
use sleuth_core::settings::OFFICE_CACHE_UNC;
use sleuth_server::audit::{Audit, cli_user};
use sleuth_server::config::{self, Config, Role};
use sleuth_server::tls;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const RULE: &str = "------------------------------------------------------------------------";

fn say(s: &str) {
    println!("{s}");
}

fn step(n: u32, of: u32, title: &str) {
    println!();
    println!("{RULE}");
    println!(" Step {n} of {of}: {title}");
    println!("{RULE}");
}

/// Read one line; an empty answer takes `default`. End of input closes setup.
fn ask(question: &str, default: &str) -> Result<String> {
    if default.is_empty() {
        print!("{question}: ");
    } else {
        print!("{question} [{default}]: ");
    }
    std::io::stdout().flush().ok();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        bail!("setup closed");
    }
    let t = line.trim().trim_matches('"').trim();
    Ok(if t.is_empty() { default.to_string() } else { t.to_string() })
}

fn yes(question: &str, default_yes: bool) -> Result<bool> {
    let a = ask(&format!("{question} (y/n)"), if default_yes { "y" } else { "n" })?;
    Ok(a.to_lowercase().starts_with('y'))
}

/// Keep the window open so the person can read what happened.
pub fn pause() {
    print!("\nPress Enter to close this window.");
    std::io::stdout().flush().ok();
    let mut s = String::new();
    let _ = std::io::stdin().lock().read_line(&mut s);
}

fn is_elevated() -> bool {
    // "High Mandatory Level": this console runs as administrator.
    std::process::Command::new("whoami")
        .arg("/groups")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("S-1-16-12288"))
        .unwrap_or(false)
}

/// Start this program again as administrator (Windows asks the user). True when it started.
fn relaunch_elevated(exe: &Path) -> bool {
    let cmd = format!("Start-Process -FilePath '{}' -ArgumentList 'setup' -Verb RunAs", exe.display().to_string().replace('\'', "''"));
    std::process::Command::new("powershell").args(["-NoProfile", "-Command", &cmd]).status().is_ok_and(|s| s.success())
}

fn to_unc(p: &str) -> String {
    sleuth_core::paths::DriveMap::current().to_unc(p.trim())
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("cannot create {}", to.display()))?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let dst = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &dst)?;
        } else {
            std::fs::copy(e.path(), &dst).with_context(|| format!("cannot copy {}", e.path().display()))?;
        }
    }
    Ok(())
}

/// The program must live in a fixed folder: the Windows service runs it from
/// there. Opened from inside the zip or from Downloads, offer to install it.
/// Returns Some(new exe) when it was copied and should be started from there.
fn ensure_installed(exe: &Path) -> Result<Option<PathBuf>> {
    let dir = exe.parent().unwrap_or(Path::new("."));
    let lower = dir.display().to_string().to_lowercase();
    let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
    let home = PathBuf::from(&pf).join("Sleuth Server");
    if lower.starts_with(&pf.to_lowercase()) {
        return Ok(None);
    }
    let temp = std::env::var("TEMP").unwrap_or_default().to_lowercase();
    if (!temp.is_empty() && lower.starts_with(&temp)) || !dir.join("pdfium.dll").exists() {
        say("");
        say("It looks like this was opened straight from the zip file.");
        say("Close this window, right-click the zip > Extract All..., then open");
        say("sleuth-server.exe from the extracted folder.");
        bail!("not extracted");
    }
    say("");
    say(&format!("Sleuth Server is running from {}.", dir.display()));
    say(&format!("The Windows service needs it in a fixed place. Setup can copy it to {}.", home.display()));
    if !yes("Copy it there now (recommended)?", true)? {
        say("Keeping it here. Do not move or delete this folder afterwards: the service runs from it.");
        return Ok(None);
    }
    copy_dir(dir, &home)?;
    say(&format!("Copied to {}.", home.display()));
    Ok(Some(home.join("sleuth-server.exe")))
}

fn listen_port(c: &Config) -> u16 {
    c.listen.parse::<std::net::SocketAddr>().map(|a| a.port()).unwrap_or(8455)
}

/// What desktops need to find this server, left in the office index folder.
fn write_card(c: &Config) -> Result<Option<PathBuf>> {
    let Some(dir) = c.shared_cache_dir.as_deref() else { return Ok(None) };
    let port = listen_port(c);
    let card = OfficeServerCard {
        address: format!("https://{}:{port}", tls::computer_name().to_lowercase()),
        addresses: tls::local_ipv4().iter().map(|ip| format!("https://{ip}:{port}")).collect(),
        pc: tls::computer_name(),
        at: sleuth_engine::service::now(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let path = Path::new(dir).join(sleuth_engine::remote::SERVER_CARD_FILE);
    std::fs::write(&path, serde_json::to_string_pretty(&card)?).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(Some(path))
}

fn print_connection(data: &Path, c: &Config) {
    let port = listen_port(c);
    println!();
    println!("{RULE}");
    println!(" What to enter on each PC: Sleuth > Settings > Office server");
    println!("{RULE}");
    println!(" Address          https://{}:{port}", tls::computer_name().to_lowercase());
    for ip in tls::local_ipv4() {
        println!("                  (or https://{ip}:{port})");
    }
    match c.shared_cache_dir.as_deref() {
        Some(dir) => {
            println!(" Certificate file leave it empty: Sleuth finds it in the office index folder");
            println!("                  ({})", Path::new(dir).join(sleuth_engine::remote::CA_FILE_NAME).display());
            println!("                  Sleuth also fills the address from there: press \"Use it\".");
        }
        None => println!(" Certificate file {}", data.join(tls::CA_CERT).display()),
    }
    println!(" Access key       the key made for that person (\"Add a person\" in this menu).");
    println!("                  Keys are shown once; a lost key is replaced by removing and");
    println!("                  adding the person again.");
    println!(" Then press       Connect and save. \"Now: Connected.\" means it works.");
    println!("{RULE}");
}

fn ask_projects(c: &mut Config) -> Result<()> {
    say("A project is a name for a set of folders the server reads, for example");
    say(r"P12513 = \\fileserver\Projects\P12513. Type the folders as network paths;");
    say("a drive letter such as P: is turned into its network path for you.");
    loop {
        let first = c.projects.is_empty();
        let name = ask(if first { "Project name" } else { "Another project name (Enter to finish)" }, "")?;
        if name.is_empty() {
            if first {
                say("At least one project is needed.");
                continue;
            }
            return Ok(());
        }
        let folders = ask("Its folders (separate several with commas)", "")?;
        let roots: Vec<String> = folders.split(',').map(to_unc).filter(|f| !f.is_empty()).collect();
        if roots.is_empty() {
            say("No folder given; the project was not added.");
            continue;
        }
        for r in &roots {
            match sleuth_engine::crawl::list_with_timeout(r, Duration::from_secs(10)) {
                Ok(e) => say(&format!("  ok    {r} ({} entries at the top)", e.len())),
                Err(e) => say(&format!("  WARN  {r}: {e}. The service may not reach it; check the share permissions.")),
            }
        }
        match config::parse_project(&format!("{name}={}", roots.join(","))) {
            Ok(p) if c.project(&p.name).is_none() => {
                c.projects.push(p);
                say(&format!("Project {name} added."));
            }
            Ok(_) => say(&format!("There is already a project called {name}.")),
            Err(e) => say(&format!("Not added: {e:#}")),
        }
    }
}

/// Add people and show each new key once (also copied to the clipboard).
fn add_people(data: &Path) -> Result<()> {
    say("Each person gets their own access key, so one can be switched off without");
    say("affecting the others. (One shared key, e.g. a person called \"office\", also works.)");
    let mut made: Vec<(String, String)> = Vec::new();
    loop {
        let def = if made.is_empty() { cli_user().rsplit('\\').next().unwrap_or("office").to_lowercase() } else { String::new() };
        let name = ask(if made.is_empty() { "Person's name" } else { "Another person (Enter to finish)" }, &def)?;
        if name.is_empty() {
            break;
        }
        let admin = yes(&format!("May {name} change the server from Sleuth (admin)?"), made.is_empty())?;
        match crate::create_user(data, &name, if admin { Role::Admin } else { Role::User }, &["*".to_string()]) {
            Ok(key) => {
                say(&format!("  Access key for {name}: {key}"));
                made.push((name, key));
            }
            Err(e) => say(&format!("  Not added: {e:#}")),
        }
    }
    if !made.is_empty() {
        let text: String = made.iter().map(|(n, k)| format!("{n}\t{k}\r\n")).collect();
        let copied = std::process::Command::new("cmd")
            .args(["/c", "clip"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut ch| {
                ch.stdin.take().unwrap().write_all(text.as_bytes())?;
                ch.wait()
            })
            .is_ok_and(|s| s.success());
        println!();
        println!("{RULE}");
        println!(" New access keys (shown only now; the server keeps only a fingerprint)");
        println!("{RULE}");
        for (n, k) in &made {
            println!(" {n:<20} {k}");
        }
        if copied {
            println!(" They are also on the clipboard: paste them somewhere safe, or send each");
            println!(" person their own key.");
        }
    }
    Ok(())
}

fn service_up(data: &Path, elevated: bool) -> Result<()> {
    if !elevated {
        say("Skipped: installing the Windows service needs administrator rights.");
        say("Right-click sleuth-server.exe > Run as administrator, then choose \"Restart the service\".");
        return Ok(());
    }
    use sleuth_server::winsvc;
    match winsvc::install(data) {
        Ok(()) => say(&format!("Service installed ({}). It starts with Windows.", winsvc::SERVICE_NAME)),
        Err(e) => say(&format!("Service already installed or not installable: {e:#}")),
    }
    let _ = winsvc::stop();
    std::thread::sleep(Duration::from_secs(2));
    match winsvc::start() {
        Ok(()) => say("Service started."),
        Err(e) => say(&format!("FAILED to start the service: {e:#}. See {}", data.join("logs").join("server.log").display())),
    }
    if let Ok(a) = Audit::open(data) {
        a.record(&cli_user(), "setup service", winsvc::SERVICE_NAME, "ok");
    }
    Ok(())
}

/// Wait for the HTTPS port to answer, so "it works" is checked, not assumed.
fn wait_listening(c: &Config) -> bool {
    let port = listen_port(c);
    for _ in 0..15 {
        if std::net::TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(1)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    false
}

fn first_time(data: &Path, elevated: bool) -> Result<()> {
    say("This sets up the Sleuth office server on this PC. It takes about five minutes.");
    say("This PC should stay switched on: desktops search through it.");

    step(1, 5, "Office index folder");
    say("The shared folder every Sleuth PC already uses (Settings > Office index folder).");
    say("The server puts its certificate and its address there, so desktops find it.");
    say("The service cannot see mapped drive letters such as U:, so use the network path.");
    let dir = loop {
        let d = to_unc(&ask("Office index folder", OFFICE_CACHE_UNC)?);
        match std::fs::create_dir_all(&d).and_then(|_| std::fs::write(Path::new(&d).join(".server-write-test"), b"ok")) {
            Ok(()) => {
                let _ = std::fs::remove_file(Path::new(&d).join(".server-write-test"));
                say(&format!("  ok    {d} can be written."));
                break d;
            }
            Err(e) => say(&format!("  FAIL  {d}: {e}. Try again, or check the path in File Explorer.")),
        }
    };

    step(2, 5, "Projects to read");
    let mut c = Config { shared_cache_dir: Some(dir), hostnames: tls::local_ipv4().iter().map(|i| i.to_string()).collect(), ..Default::default() };
    ask_projects(&mut c)?;

    step(3, 5, "Certificates and settings");
    let projects: Vec<String> = c.projects.iter().map(|p| format!("{}={}", p.name, p.roots.join(","))).collect();
    crate::init(data, &projects, c.listen.clone(), c.hostnames.clone(), c.shared_cache_dir.clone())?;
    let c = Config::load(data)?;
    if let Some(p) = write_card(&c)? {
        say(&format!("Desktops find this server through {}.", p.display()));
    }

    step(4, 5, "Access keys");
    add_people(data)?;

    step(5, 5, "Windows service");
    service_up(data, elevated)?;
    if elevated {
        say("Checking that the server answers...");
        if wait_listening(&c) {
            say(&format!("  ok    the server answers on port {}.", listen_port(&c)));
        } else {
            say(&format!("  FAIL  nothing answers on port {} yet. See {}", listen_port(&c), data.join("logs").join("server.log").display()));
        }
    }
    print_connection(data, &c);
    say("Run sleuth-server.exe again at any time to add people, check or restart the server.");
    Ok(())
}

fn menu(data: &Path, elevated: bool) -> Result<()> {
    loop {
        let c = Config::load(data)?;
        println!();
        println!("{RULE}");
        println!(" Sleuth Server on {}: {} project(s), {} person(s), data in {}", tls::computer_name(), c.projects.len(), c.users.len(), data.display());
        println!("{RULE}");
        say(" 1  Show what to enter on each PC (address, certificate, key)");
        say(" 2  Add a person (makes a new access key)");
        say(" 3  Remove a person (their key stops working)");
        say(" 4  List people");
        say(" 5  Add a project");
        say(" 6  Check everything");
        say(" 7  Restart the service");
        say(" 8  Renew the server certificate");
        say(" 0  Close");
        let r = match ask("Choose", "0")?.as_str() {
            "1" => {
                if let Ok(Some(p)) = write_card(&c) {
                    say(&format!("(Connection card refreshed: {})", p.display()));
                }
                if let Some(d) = c.shared_cache_dir.as_deref() {
                    if let Err(e) = tls::publish_ca(data, d) {
                        say(&format!("WARN  could not copy the certificate to the office index folder: {e:#}"));
                    }
                }
                print_connection(data, &c);
                Ok(())
            }
            "2" => add_people(data),
            "3" => {
                let name = ask("Name of the person to remove", "")?;
                if name.is_empty() {
                    Ok(())
                } else {
                    crate::change(data, "remove user", &name, |c| {
                        let before = c.users.len();
                        c.users.retain(|u| !u.name.eq_ignore_ascii_case(&name));
                        if c.users.len() == before {
                            bail!("no person called {name}");
                        }
                        Ok(())
                    })
                }
            }
            "4" => {
                for u in &c.users {
                    println!(" {:<24} {:<6} {}", u.name, u.role.as_str(), u.projects.join(","));
                }
                if c.users.is_empty() {
                    say(" Nobody yet: choose 2.");
                }
                Ok(())
            }
            "5" => {
                let mut c2 = c.clone();
                let before = c2.projects.len();
                ask_projects(&mut c2)?;
                let added: Vec<_> = c2.projects[before..].to_vec();
                crate::change(data, "add project", "setup", |cfg| {
                    cfg.projects.extend(added);
                    Ok(())
                })
            }
            "6" => crate::check(data).map(|_| ()),
            "7" => service_up(data, elevated).map(|_| {
                if elevated && wait_listening(&c) {
                    say("  ok    the server answers.");
                }
            }),
            "8" => tls::issue_server_cert(data, &c.hostnames).map(|_| say("New certificate written. Choose 7 to restart the service with it.")),
            _ => return Ok(()),
        };
        if let Err(e) = r {
            say(&format!("FAILED: {e:#}"));
        }
    }
}

/// Ok(true): done here, keep the window open. Ok(false): carried on in another window.
pub fn run(data: &Path) -> Result<bool> {
    println!("{RULE}");
    println!(" Sleuth Server {} setup", env!("CARGO_PKG_VERSION"));
    println!("{RULE}");
    let exe = std::env::current_exe()?;
    let mut elevated = is_elevated();
    if !elevated {
        say("Setup needs administrator rights to install the Windows service.");
        say("Windows will ask for them now; setup continues in the new window.");
        if relaunch_elevated(&exe) {
            return Ok(false);
        }
        say("Administrator rights were not given. Setup goes on, but cannot install the service.");
        elevated = false;
    }
    if let Some(new_exe) = ensure_installed(&exe)? {
        say("Carrying on from the installed copy...");
        // same window: the installed copy runs the rest and keeps the window open itself
        std::process::Command::new(&new_exe).arg("setup").status().with_context(|| format!("cannot start {}", new_exe.display()))?;
        return Ok(false);
    }
    if !config::config_path(data).exists() {
        first_time(data, elevated)?;
    }
    menu(data, elevated)?;
    Ok(false)
}
