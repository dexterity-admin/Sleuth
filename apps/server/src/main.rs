//! `sleuth-server.exe`: set up and run the Sleuth office server.
//!
//! Every command takes `--data <folder>` (default
//! `%ProgramData%\Sleuth Server`). Changes made here (users, projects,
//! certificates) are written to the audit log as the Windows user running
//! the command; a running server picks up `server.json` changes within a few
//! seconds.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use sleuth_server::audit::{Audit, cli_user};
use sleuth_server::config::{self, Config, Role, UserCfg};
use sleuth_server::tls;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "sleuth-server", version, about = "Sleuth office server: one shared drawing index for the office, over HTTPS")]
struct Cli {
    /// Server data folder (configuration, certificates, index, audit log).
    #[arg(long, global = true)]
    data: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum RoleArg {
    User,
    Admin,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create server.json and the certificates.
    Init {
        /// NAME=\\server\share[,\\server\other] (repeat for more projects).
        #[arg(long = "project")]
        projects: Vec<String>,
        /// Address and port to listen on.
        #[arg(long, default_value = config::DEFAULT_LISTEN)]
        listen: String,
        /// Extra DNS name or address for the certificate (repeatable).
        #[arg(long = "hostname")]
        hostnames: Vec<String>,
        /// Shared reading cache folder (UNC); the CA file is copied there.
        #[arg(long)]
        shared_cache: Option<String>,
    },
    /// Add a project: NAME=\\server\share[,more].
    AddProject { project: String },
    /// Remove a project (no user may still list it).
    RemoveProject { name: String },
    /// Add a user and print their new access key once.
    AddUser {
        name: String,
        #[arg(long, value_enum, default_value = "user")]
        role: RoleArg,
        /// Project names separated by commas, or * for all.
        #[arg(long)]
        projects: String,
    },
    /// Remove a user; their access key stops working at once.
    RemoveUser { name: String },
    /// List users with their role and projects.
    ListUsers,
    /// Run in this console (Ctrl+C stops).
    Run,
    /// Install, remove, start or stop the Windows service.
    Service {
        #[command(subcommand)]
        action: ServiceCmd,
    },
    /// Make a new server certificate from the same CA.
    RenewCert,
    /// Check the configuration, the project folders and the certificate.
    Check,
    /// Entry point used by the Windows service.
    #[command(hide = true)]
    ServiceRun,
}

#[derive(Subcommand)]
enum ServiceCmd {
    Install,
    Uninstall,
    Start,
    Stop,
}

fn console_logging() {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_env("SLEUTH_LOG").unwrap_or_else(|_| "info".into())).init();
}

fn file_logging(data: &Path) {
    let dir = data.join("logs");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("server.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 10 * 1024 * 1024) {
        let _ = std::fs::rename(&path, dir.join("server.old.log"));
    }
    if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        tracing_subscriber::fmt()
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("SLEUTH_LOG").unwrap_or_else(|_| "info".into()))
            .init();
    }
}

fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--reader") {
        // Started by the server's own engine as a reader process.
        return sleuth_engine::readjob::serve();
    }
    let cli = Cli::parse();
    let data = sleuth_server::data_dir(cli.data);
    sleuth_server::use_data_dir(&data);
    match cli.cmd {
        Cmd::Init { projects, listen, hostnames, shared_cache } => init(&data, &projects, listen, hostnames, shared_cache),
        Cmd::AddProject { project } => change(&data, "add project", &project, |c| {
            let p = config::parse_project(&project)?;
            if c.project(&p.name).is_some() {
                bail!("there is already a project called {}", p.name);
            }
            c.projects.push(p);
            Ok(())
        }),
        Cmd::RemoveProject { name } => change(&data, "remove project", &name, |c| {
            if let Some(u) = c.users.iter().find(|u| u.projects.iter().any(|p| p.eq_ignore_ascii_case(&name))) {
                bail!("user {} still has project {name}; change or remove that user first", u.name);
            }
            let before = c.projects.len();
            c.projects.retain(|p| !p.name.eq_ignore_ascii_case(&name));
            if c.projects.len() == before {
                bail!("no project called {name}");
            }
            Ok(())
        }),
        Cmd::AddUser { name, role, projects } => add_user(&data, &name, role, &projects),
        Cmd::RemoveUser { name } => change(&data, "remove user", &name, |c| {
            let before = c.users.len();
            c.users.retain(|u| !u.name.eq_ignore_ascii_case(&name));
            if c.users.len() == before {
                bail!("no user called {name}");
            }
            Ok(())
        }),
        Cmd::ListUsers => {
            let c = Config::load(&data)?;
            if c.users.is_empty() {
                println!("no users yet (add one with `sleuth-server add-user NAME --projects ...`)");
            }
            for u in &c.users {
                println!("{:<24} {:<6} {}", u.name, u.role.as_str(), u.projects.join(","));
            }
            Ok(())
        }
        Cmd::Run => {
            console_logging();
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            rt.block_on(sleuth_server::run_until(&data, async {
                let _ = tokio::signal::ctrl_c().await;
            }))
        }
        Cmd::Service { action } => service(&data, action),
        Cmd::RenewCert => {
            let c = Config::load(&data)?;
            let (dns, ips) = tls::issue_server_cert(&data, &c.hostnames)?;
            print_names(&dns, &ips);
            println!("New server certificate written. Restart the server (sleuth-server service stop / start) to use it.");
            Audit::open(&data)?.record(&cli_user(), "renew certificate", "server.pem", "ok");
            Ok(())
        }
        Cmd::Check => check(&data),
        Cmd::ServiceRun => {
            file_logging(&data);
            std::panic::set_hook(Box::new(|p| tracing::error!("panic: {p}")));
            run_service(data)
        }
    }
}

#[cfg(windows)]
fn run_service(data: PathBuf) -> Result<()> {
    sleuth_server::winsvc::run_as_service(data)
}

#[cfg(not(windows))]
fn run_service(_data: PathBuf) -> Result<()> {
    bail!("the service is only available on Windows")
}

#[cfg(windows)]
fn service(data: &Path, action: ServiceCmd) -> Result<()> {
    use sleuth_server::winsvc;
    let (what, r) = match action {
        ServiceCmd::Install => {
            Config::load(data).context("set the server up with `init` before installing the service")?;
            ("install service", winsvc::install(data))
        }
        ServiceCmd::Uninstall => ("uninstall service", winsvc::uninstall()),
        ServiceCmd::Start => ("start service", winsvc::start()),
        ServiceCmd::Stop => ("stop service", winsvc::stop()),
    };
    if let Ok(a) = Audit::open(data) {
        a.record(&cli_user(), what, winsvc::SERVICE_NAME, if r.is_ok() { "ok" } else { "failed" });
    }
    r?;
    println!("{what}: done");
    if what == "install service" {
        println!("The service runs as {}. It needs read access to every project share (see docs/SERVER.md).", winsvc::SERVICE_ACCOUNT);
        println!("Start it with: sleuth-server service start");
    }
    Ok(())
}

#[cfg(not(windows))]
fn service(_data: &Path, _action: ServiceCmd) -> Result<()> {
    bail!("the service is only available on Windows")
}

fn print_names(dns: &[String], ips: &[std::net::IpAddr]) {
    println!("Server certificate names: {}", dns.join(", "));
    println!("Server certificate addresses: {}", ips.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", "));
}

fn init(data: &Path, projects: &[String], listen: String, hostnames: Vec<String>, shared_cache: Option<String>) -> Result<()> {
    if config::config_path(data).exists() {
        bail!("{} already exists; use add-project / add-user to change it", config::config_path(data).display());
    }
    let mut c = Config { listen, hostnames, shared_cache_dir: shared_cache.filter(|s| !s.trim().is_empty()), ..Default::default() };
    for p in projects {
        let p = config::parse_project(p)?;
        if c.project(&p.name).is_some() {
            bail!("project {} is given twice", p.name);
        }
        c.projects.push(p);
    }
    c.validate()?;
    std::fs::create_dir_all(data).with_context(|| format!("cannot create {}", data.display()))?;
    let made = tls::ensure_ca(data)?;
    let (dns, ips) = tls::issue_server_cert(data, &c.hostnames)?;
    c.save(data)?;
    let audit = Audit::open(data)?;
    audit.record(&cli_user(), "init", &data.display().to_string(), &format!("{} project(s)", c.projects.len()));
    println!("Set up in {}", data.display());
    println!("{} certificate authority: {}", if made { "New" } else { "Existing" }, data.join(tls::CA_CERT).display());
    print_names(&dns, &ips);
    let ca = std::fs::read_to_string(data.join(tls::CA_CERT))?;
    println!("CA fingerprint (SHA-256): {}", tls::fingerprint(&ca)?);
    match &c.shared_cache_dir {
        Some(dir) => match tls::publish_ca(data, dir) {
            Ok(p) => println!("Desktops will find the CA at {}", p.display()),
            Err(e) => println!("WARNING: {e:#}. Copy {} there by hand as {}.", data.join(tls::CA_CERT).display(), sleuth_engine::remote::CA_FILE_NAME),
        },
        None => println!("No shared cache folder set: give desktops {} (Settings > Office server > certificate file).", data.join(tls::CA_CERT).display()),
    }
    println!("Next: sleuth-server add-user NAME --projects {}", c.projects.first().map(|p| p.name.as_str()).unwrap_or("*"));
    Ok(())
}

/// Load, change, validate and save `server.json`, and audit the change.
fn change(data: &Path, action: &str, target: &str, f: impl FnOnce(&mut Config) -> Result<()>) -> Result<()> {
    let mut c = Config::load(data)?;
    let audit = Audit::open(data)?;
    let r = f(&mut c).and_then(|_| c.save(data));
    audit.record(&cli_user(), action, target, if r.is_ok() { "ok" } else { "refused" });
    r?;
    println!("{action}: done");
    Ok(())
}

fn add_user(data: &Path, name: &str, role: RoleArg, projects: &str) -> Result<()> {
    let key = config::new_key()?;
    let projects: Vec<String> = projects.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();
    let role = match role {
        RoleArg::User => Role::User,
        RoleArg::Admin => Role::Admin,
    };
    change(data, "add user", name, |c| {
        if c.user(name).is_some() {
            bail!("there is already a user called {name} (remove-user first to issue a new key)");
        }
        c.users.push(UserCfg { name: name.trim().to_string(), token_sha256: config::hash_key(&key), role, projects: projects.clone() });
        Ok(())
    })?;
    println!();
    println!("Access key for {name} (shown only this once; the server keeps only its hash):");
    println!();
    println!("    {key}");
    println!();
    println!("Enter it in Sleuth: Settings > Office server > Access key.");
    Ok(())
}

fn check(data: &Path) -> Result<()> {
    let mut problems = 0;
    let c = match Config::load(data) {
        Ok(c) => {
            println!("ok    server.json: {} project(s), {} user(s), listening on {}", c.projects.len(), c.users.len(), c.listen);
            c
        }
        Err(e) => {
            println!("FAIL  {e:#}");
            bail!("the configuration is not valid");
        }
    };
    let drives = sleuth_core::paths::DriveMap::current();
    for p in &c.projects {
        for r in &p.roots {
            let mapped = drives.to_unc(r) != *r;
            match sleuth_engine::crawl::list_with_timeout(r, Duration::from_secs(10)) {
                Ok(e) => println!("ok    {} {r}: {} entries at the top", p.name, e.len()),
                Err(e) => {
                    problems += 1;
                    println!("FAIL  {} {r}: {e}", p.name);
                }
            }
            if mapped {
                println!("warn  {} {r}: a mapped drive letter; the service cannot see mapped drives, use {}", p.name, drives.to_unc(r));
            }
        }
    }
    let now = sleuth_engine::service::now();
    for (name, file) in [("CA certificate", tls::CA_CERT), ("server certificate", tls::SERVER_CERT)] {
        match tls::expires(&data.join(file)) {
            Ok(t) if t <= now => {
                problems += 1;
                println!("FAIL  {name} has expired");
            }
            Ok(t) if t - now < 30 * 86_400 => println!("warn  {name} expires in {} days (renew-cert)", (t - now) / 86_400),
            Ok(t) => println!("ok    {name} valid for {} more days", (t - now) / 86_400),
            Err(e) => {
                problems += 1;
                println!("FAIL  {name}: {e:#}");
            }
        }
    }
    if let Err(e) = tls::server_config(data) {
        problems += 1;
        println!("FAIL  {e:#}");
    }
    if let Some(dir) = &c.shared_cache_dir {
        let p = Path::new(dir).join(sleuth_engine::remote::CA_FILE_NAME);
        let same = std::fs::read(&p).ok() == std::fs::read(data.join(tls::CA_CERT)).ok();
        if same {
            println!("ok    desktops' CA copy at {}", p.display());
        } else {
            println!("warn  {} is missing or not this server's CA; copy {} there", p.display(), data.join(tls::CA_CERT).display());
        }
    }
    if problems > 0 {
        bail!("{problems} problem(s) found");
    }
    println!("All checks passed.");
    Ok(())
}
