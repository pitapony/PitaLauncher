//! Headless developer CLI.
//!
//! This is not the shipping `pitagames` binary (that will be the Dioxus app);
//! it exists so the scan layer can be exercised without a GUI, in CI, and on
//! machines where the webview stack is unavailable.

use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use pitagames_core::desktop::{self, DesktopEntry};
use pitagames_core::paths;
use pitagames_core::{
    AntiCheat, Game, GameId, GameMetadata, LaunchSpec, Platforms, Store, read_shim_profile,
    shim_launch_options, shim_profile_path, steam_install,
};
use pitagames_db::{Db, GameRepository};
use pitagames_platform::anticheat::{
    AnticheatScan, AnticheatSource, EffectiveAnticheat, GateDecision,
};
use pitagames_platform::compat::{CompatDb, CompatEntry};
use pitagames_platform::disk;
use pitagames_platform::logs as log_files;
use pitagames_platform::prefix::{self, Prefix};
use pitagames_platform::proton::{self, DiscoveryReport, ProtonInstall, ProtonRoot, ProtonSource};
use pitagames_secrets::{SteamApiKey, SteamKeyStore};
use pitagames_stores::amazon::{self, Amazon};
use pitagames_stores::compat_tool::CompatTool;
use pitagames_stores::epic::{self, Epic};
use pitagames_stores::gog::{self, Gog};
use pitagames_stores::import::{ImportReport, Importer, SourceReport};
use pitagames_stores::install::{DEFAULT_POLL_INTERVAL, InstallMonitor, InstallSample};
use pitagames_stores::status::{self, RowState, SessionState, StoreStatusReport, UpdateAvailable};
use pitagames_stores::steam::{self, ScanReport};
use pitagames_stores::steam_config::{
    self, CompatRestoreReport, CompatWriteReport, SteamProcessState, WriteOptions, WriteReport,
};
use pitagames_stores::sync::{self, Backends, SyncReport};
use pitagames_stores::{
    CliCommand, CliOptions, CliSpec, DEFAULT_PRIORITY, DownloadProgress, ProgressEvent,
};

/// Give up on an install after an hour unless told otherwise.
const DEFAULT_INSTALL_TIMEOUT: Duration = Duration::from_secs(3600);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pitagames-cli: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        println!("{}", usage());
        return Ok(());
    };

    match command.as_str() {
        "scan" => scan(&args.collect::<Vec<_>>()),
        "install" => install(&args.collect::<Vec<_>>()),
        "metadata" => metadata(&args.collect::<Vec<_>>()),
        "proton" => proton(&args.collect::<Vec<_>>()),
        "compat" => compat(&args.collect::<Vec<_>>()),
        "anticheat" => anticheat(&args.collect::<Vec<_>>()),
        "profile" => profile(&args.collect::<Vec<_>>()),
        "logs" => logs(&args.collect::<Vec<_>>()),
        "import" => import(&args.collect::<Vec<_>>()),
        "prefix" => prefix(&args.collect::<Vec<_>>()),
        "disk" => disk(&args.collect::<Vec<_>>()),
        "stores" => stores(&args.collect::<Vec<_>>()),
        "desktop-entry" => desktop_entry(&args.collect::<Vec<_>>()),
        "activate" => activate(&args.collect::<Vec<_>>()),
        "launch-options" => launch_options(&args.collect::<Vec<_>>()),
        "compat-tool" => compat_tool(&args.collect::<Vec<_>>()),
        "steam-account" => steam_account(&args.collect::<Vec<_>>()),
        "epic" => epic(&args.collect::<Vec<_>>()),
        "amazon" => amazon(&args.collect::<Vec<_>>()),
        "gog" => gog(&args.collect::<Vec<_>>()),
        "-h" | "--help" | "help" => {
            println!("{}", usage());
            Ok(())
        }
        "-V" | "--version" => {
            println!("pitagames-cli {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => bail!("unknown command `{other}`\n\n{}", usage()),
    }
}

fn scan(args: &[String]) -> Result<()> {
    let mut steam_root: Option<PathBuf> = None;
    let mut json = false;
    let mut include_tools = false;

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--json" => json = true,
            "--include-tools" => include_tools = true,
            "--steam-root" => {
                index += 1;
                let value = args
                    .get(index)
                    .with_context(|| "--steam-root requires a path")?;
                steam_root = Some(PathBuf::from(value));
            }
            "-h" | "--help" => {
                println!("{}", scan_usage());
                return Ok(());
            }
            _ if arg.starts_with("--steam-root=") => {
                let value = &arg["--steam-root=".len()..];
                if value.is_empty() {
                    bail!("--steam-root requires a path");
                }
                steam_root = Some(PathBuf::from(value));
            }
            other => bail!(
                "unexpected argument `{other}` for `scan`\n\n{}",
                scan_usage()
            ),
        }
        index += 1;
    }

    let root = match steam_root {
        Some(root) => root,
        None => discover_steam_root().context(
            "no Steam library found; pass --steam-root <path> (usually ~/.local/share/Steam)",
        )?,
    };

    let started = Instant::now();
    let report = steam::scan_steam(&root, include_tools);
    let elapsed = started.elapsed();

    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&report.games)?);
    } else {
        print_table(&root, &report, elapsed);
    }

    Ok(())
}

fn discover_steam_root() -> Option<PathBuf> {
    pitagames_core::paths::steam_roots()
        .into_iter()
        .find(|root| root.join("steamapps").is_dir())
}

/// Ask Steam to install an app, then follow the appmanifest until it is ready.
fn install(args: &[String]) -> Result<()> {
    let mut app_id: Option<u32> = None;
    let mut steam_root: Option<PathBuf> = None;
    let mut timeout = DEFAULT_INSTALL_TIMEOUT;
    let mut hand_off = true;

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--steam-root" => {
                index += 1;
                let value = args
                    .get(index)
                    .with_context(|| "--steam-root requires a path")?;
                steam_root = Some(PathBuf::from(value));
            }
            "--timeout" => {
                index += 1;
                let value = args
                    .get(index)
                    .with_context(|| "--timeout requires a number of seconds")?;
                timeout = parse_timeout(value)?;
            }
            "--no-launch" => hand_off = false,
            "-h" | "--help" => {
                println!("{}", install_usage());
                return Ok(());
            }
            _ if arg.starts_with("--steam-root=") => {
                let value = &arg["--steam-root=".len()..];
                if value.is_empty() {
                    bail!("--steam-root requires a path");
                }
                steam_root = Some(PathBuf::from(value));
            }
            _ if arg.starts_with("--timeout=") => {
                timeout = parse_timeout(&arg["--timeout=".len()..])?;
            }
            other if !other.starts_with('-') => {
                if app_id.is_some() {
                    bail!(
                        "unexpected argument `{other}` for `install`\n\n{}",
                        install_usage()
                    );
                }
                app_id = Some(
                    other
                        .parse::<u32>()
                        .with_context(|| format!("`{other}` is not a numeric Steam appid"))?,
                );
            }
            other => bail!(
                "unexpected argument `{other}` for `install`\n\n{}",
                install_usage()
            ),
        }
        index += 1;
    }

    let app_id = app_id.context("missing Steam appid (for example: `install 620`)")?;

    let roots: Vec<PathBuf> = match &steam_root {
        Some(root) => vec![root.clone()],
        None => pitagames_core::paths::steam_roots()
            .into_iter()
            .filter(|root| root.is_dir())
            .collect(),
    };
    if roots.is_empty() {
        bail!("no Steam root found; pass --steam-root <path> (usually ~/.local/share/Steam)");
    }

    let suspects: Vec<&PathBuf> = roots
        .iter()
        .filter(|root| !root.join("steamapps").is_dir())
        .collect();
    if !suspects.is_empty() {
        bail!(
            "{} has no steamapps/ directory; is it a Steam root?",
            suspects
                .iter()
                .map(|root| root.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let monitor = InstallMonitor::with_roots(app_id, &roots, timeout);
    if monitor.libraries().is_empty() {
        bail!(
            "no Steam library found under {}",
            roots
                .iter()
                .map(|root| root.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    if hand_off {
        // Steam owns the download; we only hand over the request.
        spawn(&steam_install(&app_id.to_string())).context(
            "could not start Steam; is the client installed and on PATH? \
             Use --no-launch to poll an install started elsewhere",
        )?;
        println!("Asked Steam to install app {app_id}; watching the appmanifest…");
    } else {
        println!("Watching app {app_id}; no install request sent (--no-launch).");
    }

    let mut printer = SamplePrinter::default();
    let sample = monitor.wait(DEFAULT_POLL_INTERVAL, |sample| {
        if let Some(line) = printer.render(sample) {
            println!("{line}");
        }
    });

    match sample {
        InstallSample::Installed { manifest } => {
            println!("App {app_id} is installed: {}", manifest.display());
            Ok(())
        }
        other => bail!(
            "app {app_id} did not finish installing within {} s (last state: {})",
            timeout.as_secs(),
            describe(&other)
        ),
    }
}

/// Fetch Steam store metadata for one or more appids, live.
///
/// This is a diagnostic path: it does not touch the SQLite cache, so it can be
/// run on a machine where the GUI has never started.
fn metadata(args: &[String]) -> Result<()> {
    let mut app_ids: Vec<u32> = Vec::new();
    let mut json = false;
    let mut language = pitagames_metadata::DEFAULT_LANGUAGE.to_string();

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--json" => json = true,
            "--language" => {
                index += 1;
                let value = args
                    .get(index)
                    .with_context(|| "--language requires a Steam language code")?;
                language = value.clone();
            }
            "-h" | "--help" => {
                println!("{}", metadata_usage());
                return Ok(());
            }
            _ if arg.starts_with("--language=") => {
                let value = &arg["--language=".len()..];
                if value.is_empty() {
                    bail!("--language requires a Steam language code");
                }
                language = value.to_string();
            }
            other if !other.starts_with('-') => app_ids.push(
                other
                    .parse::<u32>()
                    .with_context(|| format!("`{other}` is not a numeric Steam appid"))?,
            ),
            other => bail!(
                "unexpected argument `{other}` for `metadata`\n\n{}",
                metadata_usage()
            ),
        }
        index += 1;
    }

    if app_ids.is_empty() {
        bail!("missing Steam appid\n\n{}", metadata_usage());
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start the async runtime")?;

    let fetched = runtime.block_on(async {
        let client = pitagames_metadata::SteamClient::new()
            .context("could not build the metadata client")?;
        client
            .language(language)
            .fetch(&app_ids)
            .await
            .context("metadata fetch failed")
    })?;

    for failure in &fetched.failures {
        eprintln!("warning: appid {}: {}", failure.app_id, failure.reason);
    }
    if fetched.items.is_empty() {
        bail!(
            "no metadata returned for {} appid{}",
            app_ids.len(),
            if app_ids.len() == 1 { "" } else { "s" }
        );
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&fetched.items)?);
    } else {
        for (index, metadata) in fetched.items.iter().enumerate() {
            if index > 0 {
                println!();
            }
            print_metadata(metadata);
        }
    }

    Ok(())
}

fn print_metadata(metadata: &GameMetadata) {
    let mut headline = format!("{}  [{}]", metadata.name, metadata.kind);
    if metadata.coming_soon {
        headline.push_str(" · coming soon");
    }
    println!("{headline}");

    let mut facts = Vec::new();
    if let Some(date) = &metadata.release_date {
        facts.push(format!("released {date}"));
    }
    if !metadata.developers.is_empty() {
        facts.push(metadata.developers.join(", "));
    }
    if !metadata.publishers.is_empty() {
        facts.push(format!("published by {}", metadata.publishers.join(", ")));
    }
    if metadata.is_free {
        facts.push("free to play".to_string());
    }
    if !facts.is_empty() {
        println!("{}", facts.join(" · "));
    }

    if !metadata.genres.is_empty() {
        println!("Genres: {}", metadata.genres.join(", "));
    }
    if let Some(score) = metadata.metacritic {
        println!("Metacritic: {score}");
    }
    println!("Platforms: {}", platforms_line(metadata.platforms));
    if let Some(description) = &metadata.short_description {
        println!();
        println!("{description}");
    }
    if let Some(header) = &metadata.header_image {
        println!();
        println!("Art: {header}");
    }
    if let Some(website) = &metadata.website {
        println!("Site: {website}");
    }
}

fn platforms_line(platforms: Platforms) -> String {
    let mut names = Vec::new();
    if platforms.windows {
        names.push("Windows");
    }
    if platforms.mac {
        names.push("macOS");
    }
    if platforms.linux {
        names.push("Linux");
    }
    if names.is_empty() {
        "none listed".to_string()
    } else {
        names.join(", ")
    }
}

/// List the Proton builds this machine can launch games with.
///
/// Read-only diagnostic: same discovery the GUI and the env compiler will use,
/// with `--root`/`--steam-root` overrides so it can run against a fixture tree
/// or a second Steam install.
fn proton(args: &[String]) -> Result<()> {
    let parsed = parse_proton_args(args)?;
    if parsed.help {
        println!("{}", proton_usage());
        return Ok(());
    }

    let report = if parsed.roots.is_empty() {
        proton::discover()
    } else {
        proton::discover_in(&parsed.roots)
    };

    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }

    if parsed.json {
        let payload = serde_json::json!({
            "installs": report.installs,
            "auto": report.best(),
            "roots": report.roots,
            "warnings": report.warnings,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        for line in proton_lines(&report) {
            println!("{line}");
        }
    }

    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ProtonArgs {
    roots: Vec<ProtonRoot>,
    json: bool,
    help: bool,
}

fn parse_proton_args(args: &[String]) -> Result<ProtonArgs> {
    let mut parsed = ProtonArgs::default();
    let mut index = 0;

    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--json" => parsed.json = true,
            "--root" | "--steam-root" => {
                index += 1;
                let value = args
                    .get(index)
                    .with_context(|| format!("{arg} requires a path"))?;
                append_proton_root(&mut parsed.roots, arg, value)?;
            }
            "-h" | "--help" => parsed.help = true,
            _ if arg.starts_with("--root=") || arg.starts_with("--steam-root=") => {
                let (flag, value) = arg
                    .split_once('=')
                    .expect("the guard above requires an `=`");
                if value.is_empty() {
                    bail!("{flag} requires a path");
                }
                append_proton_root(&mut parsed.roots, flag, value)?;
            }
            other => bail!(
                "unexpected argument `{other}` for `proton`\n\n{}",
                proton_usage()
            ),
        }
        index += 1;
    }

    Ok(parsed)
}

fn append_proton_root(roots: &mut Vec<ProtonRoot>, flag: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{flag} requires a path");
    }
    let path = PathBuf::from(value);
    match flag {
        "--root" => roots.push(ProtonRoot::new(path, ProtonSource::UserCompatibilityTools)),
        "--steam-root" => {
            roots.push(ProtonRoot::new(
                path.join("compatibilitytools.d"),
                ProtonSource::UserCompatibilityTools,
            ));
            roots.push(ProtonRoot::new(
                path.join("steamapps/common"),
                ProtonSource::SteamCommon,
            ));
        }
        other => bail!("internal error: unknown Proton root flag `{other}`"),
    }
    Ok(())
}

/// Render a discovery report as plain lines (no colours, piped-friendly).
fn proton_lines(report: &DiscoveryReport) -> Vec<String> {
    let mut lines = vec![format!("Searched {} location(s):", report.roots.len())];
    for root in &report.roots {
        lines.push(format!(
            "  {:<52} {}",
            root.path.display().to_string(),
            root.source.label()
        ));
    }

    let Some(best) = report.best() else {
        lines.push(String::new());
        lines.push("No Proton installations found.".to_string());
        lines.push("Install proton-cachyos, or unpack a GE-Proton build into".to_string());
        lines.push("~/.local/share/Steam/compatibilitytools.d and restart Steam.".to_string());
        return lines;
    };

    lines.push(String::new());
    lines.push(format!(
        "Found {} Proton installation(s), best first:",
        report.installs.len()
    ));

    let name_width = report
        .installs
        .iter()
        .map(|install| install.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(20);
    for install in &report.installs {
        lines.push(format_install(install, name_width));
    }

    lines.push(String::new());
    lines.push(format!(
        "Auto selects: {} ({})",
        best.name,
        best.path.display()
    ));
    lines
}

fn format_install(install: &ProtonInstall, name_width: usize) -> String {
    let version = install.distinct_version().unwrap_or("-");
    format!(
        "  {:<name_width$}  {:<20}  {:<28}  {}",
        install.name,
        install.kind.label(),
        fit(version, 28),
        install.path.display(),
        name_width = name_width,
    )
}

/// Show the compiled shim profile the launcher writes for one game.
///
/// The GUI writes `profiles/steam-<appid>.json` on every profile save and
/// `pitagames-shim` reads it at launch. This command prints what is stored, so a
/// launch can be explained on a machine where the shim is not on PATH.
fn profile(args: &[String]) -> Result<()> {
    let mut app_id: Option<String> = None;
    let mut dir = pitagames_core::paths::profiles_dir();
    let mut json = false;

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--json" => json = true,
            "--dir" => {
                index += 1;
                let value = args.get(index).with_context(|| "--dir requires a path")?;
                dir = Some(PathBuf::from(value));
            }
            "-h" | "--help" => {
                println!("{}", profile_usage());
                return Ok(());
            }
            _ if arg.starts_with("--dir=") => {
                let value = &arg["--dir=".len()..];
                if value.is_empty() {
                    bail!("--dir requires a path");
                }
                dir = Some(PathBuf::from(value));
            }
            other if !other.starts_with('-') => {
                if app_id.is_some() {
                    bail!(
                        "unexpected argument `{other}` for `profile`\n\n{}",
                        profile_usage()
                    );
                }
                app_id = Some(other.to_string());
            }
            other => bail!(
                "unexpected argument `{other}` for `profile`\n\n{}",
                profile_usage()
            ),
        }
        index += 1;
    }

    let app_id = app_id.context("missing Steam appid\n\nsee `pitagames-cli profile --help`")?;
    if app_id.is_empty() || !app_id.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("`{app_id}` is not a numeric Steam appid");
    }

    let dir = dir.context("cannot determine the XDG data dir; pass --dir <path>")?;
    let path = shim_profile_path(&dir, &GameId::new(Store::Steam, &app_id));
    if !path.is_file() {
        bail!(
            "no shim profile for appid {app_id} at {}\nthe GUI writes it when you save a profile",
            path.display()
        );
    }
    let profile = read_shim_profile(&path).context("could not read the shim profile")?;

    if json {
        let payload = serde_json::json!({ "path": path, "profile": profile });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!("Path:    {}", path.display());
    println!("Profile: {}", profile.profile);
    match &profile.proton {
        Some(proton) => match &proton.install {
            Some(install) => println!(
                "Proton:  {} ({}) {}",
                install.name,
                install.source,
                install.path.display()
            ),
            None => println!("Proton:  Steam decides"),
        },
        None => println!("Proton:  Steam decides"),
    }

    println!("Environment:");
    if profile.env.is_empty() {
        println!("  (none)");
    }
    for (key, value) in &profile.env {
        println!("  {key}={value}");
    }

    println!("Wrappers:");
    if profile.wrappers.is_empty() {
        println!("  (none)");
    }
    for wrapper in &profile.wrappers {
        let mut line = wrapper.program.display().to_string();
        for arg in &wrapper.args {
            line.push(' ');
            line.push_str(arg);
        }
        println!("  {line}");
    }

    Ok(())
}

/// List or tail the PitaGames log directory.
///
/// The GUI's Logs screen renders the same files through `platform::logs`; this
/// is the headless diagnostic, so a bug report can attach a launch line or a
/// Proton log without the webview.
fn logs(args: &[String]) -> Result<()> {
    let parsed = parse_logs_args(args)?;
    if parsed.help {
        println!("{}", logs_usage());
        return Ok(());
    }

    let dir = match parsed.dir.clone() {
        Some(dir) => dir,
        None => pitagames_core::paths::logs_dir()
            .context("cannot determine the XDG data dir; pass --dir <path>")?,
    };

    let Some(file) = parsed.file.clone() else {
        let files = log_files::list(&dir);
        if parsed.json {
            let payload = serde_json::json!({
                "dir": dir,
                "files": files.iter().map(|file| serde_json::json!({
                    "name": file.name,
                    "kind": file.kind.label(),
                    "size": file.size,
                    "modified": file.modified
                        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|elapsed| elapsed.as_secs()),
                })).collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&payload)?);
        } else if files.is_empty() {
            println!("No logs in {}", dir.display());
        } else {
            println!("Logs in {}", dir.display());
            for file in &files {
                println!(
                    "  {:<30} {:<7} {:>10}",
                    file.name,
                    file.kind.label(),
                    human_bytes(file.size)
                );
            }
        }
        return Ok(());
    };

    let path = if Path::new(&file).is_absolute() {
        PathBuf::from(&file)
    } else {
        dir.join(&file)
    };
    let value = log_files::tail(&path, parsed.tail, parsed.grep.as_deref())
        .with_context(|| format!("could not read {}", path.display()))?;

    if parsed.json {
        let payload = serde_json::json!({
            "path": path,
            "lines": value.lines,
            "window_lines": value.window_lines,
            "truncated": value.truncated,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        if value.truncated {
            println!(
                "# tail of {} (showing {} of {} line(s) in the window)",
                path.display(),
                value.lines.len(),
                value.window_lines
            );
        }
        for line in &value.lines {
            println!("{line}");
        }
    }

    Ok(())
}

/// Parsed `logs` arguments.
#[derive(Debug, PartialEq, Eq)]
struct LogsArgs {
    dir: Option<PathBuf>,
    file: Option<String>,
    tail: usize,
    grep: Option<String>,
    json: bool,
    help: bool,
}

impl Default for LogsArgs {
    fn default() -> Self {
        Self {
            dir: None,
            file: None,
            tail: DEFAULT_LOG_TAIL,
            grep: None,
            json: false,
            help: false,
        }
    }
}

/// Default tail length for `pitagames-cli logs --file`.
const DEFAULT_LOG_TAIL: usize = 200;

fn parse_logs_args(args: &[String]) -> Result<LogsArgs> {
    let mut parsed = LogsArgs::default();
    let mut index = 0;

    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--json" => parsed.json = true,
            "--dir" | "--file" | "--grep" | "--tail" => {
                index += 1;
                let value = args
                    .get(index)
                    .with_context(|| format!("{arg} requires a value"))?;
                apply_logs_flag(&mut parsed, arg, value)?;
            }
            "-h" | "--help" => parsed.help = true,
            _ if arg.starts_with("--dir=")
                || arg.starts_with("--file=")
                || arg.starts_with("--grep=")
                || arg.starts_with("--tail=") =>
            {
                let (flag, value) = arg
                    .split_once('=')
                    .expect("the guard above requires an `=`");
                apply_logs_flag(&mut parsed, flag, value)?;
            }
            other => bail!(
                "unexpected argument `{other}` for `logs`\n\n{}",
                logs_usage()
            ),
        }
        index += 1;
    }

    Ok(parsed)
}

fn apply_logs_flag(parsed: &mut LogsArgs, flag: &str, value: &str) -> Result<()> {
    match flag {
        "--dir" => {
            if value.is_empty() {
                bail!("--dir requires a path");
            }
            parsed.dir = Some(PathBuf::from(value));
        }
        "--file" => {
            if value.is_empty() {
                bail!("--file requires a name or path");
            }
            parsed.file = Some(value.to_string());
        }
        "--grep" => parsed.grep = Some(value.to_string()),
        "--tail" => {
            parsed.tail = value
                .parse::<usize>()
                .with_context(|| format!("--tail must be a whole number, got `{value}`"))?;
        }
        other => bail!("internal error: unknown logs flag `{other}`"),
    }
    Ok(())
}

/// Read the games Heroic, Lutris and Bottles have installed.
///
/// Installed-only and read-only: nothing is launched, downloaded or written,
/// and the store CLIs (`legendary`/`nile`/`lgogdownloader`) are not needed —
/// this is what a user sees when they are not installed.
fn import(args: &[String]) -> Result<()> {
    let parsed = parse_import_args(args)?;
    if parsed.help {
        println!("{}", import_usage());
        return Ok(());
    }

    let defaults = Importer::default_roots();
    let roots = |explicit: &[PathBuf], discovered: &[PathBuf]| {
        if explicit.is_empty() {
            discovered.to_vec()
        } else {
            explicit.to_vec()
        }
    };
    let report = Importer::with_roots(
        roots(&parsed.heroic, defaults.heroic_roots()),
        roots(&parsed.lutris, defaults.lutris_roots()),
        roots(&parsed.bottles, defaults.bottles_roots()),
    )
    .scan();

    if parsed.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    for source in &report.sources {
        println!(
            "{:<8} {:<11} {}",
            source.origin.label(),
            source.kind,
            source_line(source)
        );
    }
    println!();
    println!("{}", report_summary(&report));
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    for source in report.failed() {
        eprintln!(
            "warning: {} {} could not be read: {}",
            source.origin.label(),
            source.kind,
            source.error.as_deref().unwrap_or("unknown error")
        );
    }

    Ok(())
}

/// One line per source file: where it is and what came out of it.
fn source_line(source: &SourceReport) -> String {
    let path = source.path.display();
    if let Some(error) = &source.error {
        return format!("{path}  (failed: {error})");
    }
    if !source.found() {
        return format!("{path}  (missing)");
    }
    match source.games {
        0 => format!("{path}  (no games)"),
        1 => format!("{path}  (1 game)"),
        games => format!("{path}  ({games} games)"),
    }
}

/// The one-line total, by store, for the human-readable listing.
fn report_summary(report: &ImportReport) -> String {
    let mut line = match report.games.len() {
        1 => "1 game".to_string(),
        games => format!("{games} games"),
    };
    let counts: Vec<String> = [
        Store::Steam,
        Store::Gog,
        Store::Epic,
        Store::Amazon,
        Store::Local,
    ]
    .into_iter()
    .filter_map(|store| {
        let count = report
            .games
            .iter()
            .filter(|game| game.id.store == store)
            .count();
        (count > 0).then(|| format!("{store} {count}"))
    })
    .collect();
    if !counts.is_empty() {
        line.push_str(&format!(": {}", counts.join(", ")));
    }
    if report.duplicates > 0 {
        line.push_str(&format!(", {} duplicate(s) dropped", report.duplicates));
    }
    line
}

/// Parsed `import` arguments.
#[derive(Debug, Default, PartialEq, Eq)]
struct ImportArgs {
    heroic: Vec<PathBuf>,
    lutris: Vec<PathBuf>,
    bottles: Vec<PathBuf>,
    json: bool,
    help: bool,
}

fn parse_import_args(args: &[String]) -> Result<ImportArgs> {
    let mut parsed = ImportArgs::default();
    let mut index = 0;

    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--json" => parsed.json = true,
            "--heroic" | "--lutris" | "--bottles" => {
                index += 1;
                let value = args
                    .get(index)
                    .with_context(|| format!("{arg} requires a path"))?;
                apply_import_flag(&mut parsed, arg, value)?;
            }
            "-h" | "--help" => parsed.help = true,
            _ if arg.starts_with("--heroic=")
                || arg.starts_with("--lutris=")
                || arg.starts_with("--bottles=") =>
            {
                let (flag, value) = arg
                    .split_once('=')
                    .expect("the guard above requires an `=`");
                apply_import_flag(&mut parsed, flag, value)?;
            }
            other => bail!(
                "unexpected argument `{other}` for `import`\n\n{}",
                import_usage()
            ),
        }
        index += 1;
    }

    Ok(parsed)
}

fn apply_import_flag(parsed: &mut ImportArgs, flag: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{flag} requires a path");
    }
    match flag {
        "--heroic" => parsed.heroic.push(PathBuf::from(value)),
        "--lutris" => parsed.lutris.push(PathBuf::from(value)),
        "--bottles" => parsed.bottles.push(PathBuf::from(value)),
        other => bail!("internal error: unknown import flag `{other}`"),
    }
    Ok(())
}

/// Show where one non-Steam game's Wine/Proton prefix lives and what state it
/// is in.
///
/// Read-only: nothing is created, launched or deleted. Steam games keep their
/// prefixes inside Steam; this is for Epic/Amazon/GOG and imports, and it is the
/// diagnostic for the layout `platform::prefix` computes and the umu environment
/// the launch hand-off will merge.
fn prefix(args: &[String]) -> Result<()> {
    let parsed = parse_prefix_args(args)?;
    if parsed.help {
        println!("{}", prefix_usage());
        return Ok(());
    }

    let (store, id) = match parsed.positional.as_slice() {
        [store, id] => (
            Store::from_str(store).map_err(|error| anyhow::anyhow!("{error}"))?,
            id.clone(),
        ),
        _ => bail!("expected `<store> <id>`\n\n{}", prefix_usage()),
    };
    let game = GameId::new(store, id);

    let root = match parsed.root.clone() {
        Some(root) => root,
        None => {
            prefix::default_root().context("cannot determine the prefix root; pass --root <dir>")?
        }
    };
    let report = prefix::inspect(&root, &game);

    // Resolve a Proton for the umu environment: an explicit path wins, else the
    // best available build, else there is simply no `PROTONPATH` to show.
    let discovery = proton::discover();
    let (proton_path, proton_source) = match parsed.proton.clone() {
        Some(path) => (Some(path), Some("explicit")),
        None => match discovery.best() {
            Some(install) => (Some(install.path.clone()), Some("discovered")),
            None => (None, None),
        },
    };
    let env = proton_path
        .as_deref()
        .map(|proton| prefix::umu_env(&game, proton, &report.path));

    if parsed.json {
        let payload = serde_json::json!({
            "prefix": report,
            "proton": proton_path,
            "proton_source": proton_source,
            "env": env,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        print_prefix(&report, proton_path.as_deref(), proton_source, env.as_ref());
    }

    Ok(())
}

fn print_prefix(
    report: &Prefix,
    proton: Option<&Path>,
    proton_source: Option<&str>,
    env: Option<&std::collections::BTreeMap<String, String>>,
) {
    let state = if report.is_ready() {
        "ready"
    } else if report.present {
        "present, not initialized"
    } else {
        "not present"
    };
    println!("Prefix {}", report.key);
    println!("  root         {}", report.root.display());
    println!("  path         {}  ({state})", report.path.display());
    println!("  size         {}", report.size.summary());
    match proton {
        Some(proton) => println!(
            "  proton       {} ({})",
            proton.display(),
            proton_source.unwrap_or("explicit")
        ),
        None => println!("  proton       none found (pass --proton <path>)"),
    }
    if let Some(env) = env {
        println!("  umu env");
        for (key, value) in env {
            println!("    {key}={value}");
        }
    }
}

/// Parsed `prefix` arguments.
#[derive(Debug, Default, PartialEq, Eq)]
struct PrefixArgs {
    positional: Vec<String>,
    root: Option<PathBuf>,
    proton: Option<PathBuf>,
    json: bool,
    help: bool,
}

fn parse_prefix_args(args: &[String]) -> Result<PrefixArgs> {
    let mut parsed = PrefixArgs::default();
    let mut index = 0;

    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--json" => parsed.json = true,
            "--root" | "--proton" => {
                index += 1;
                let value = args
                    .get(index)
                    .with_context(|| format!("{arg} requires a path"))?;
                apply_prefix_flag(&mut parsed, arg, value)?;
            }
            "-h" | "--help" => parsed.help = true,
            _ if arg.starts_with("--root=") || arg.starts_with("--proton=") => {
                let (flag, value) = arg
                    .split_once('=')
                    .expect("the guard above requires an `=`");
                apply_prefix_flag(&mut parsed, flag, value)?;
            }
            other if !other.starts_with('-') => parsed.positional.push(other.to_string()),
            other => bail!(
                "unexpected argument `{other}` for `prefix`\n\n{}",
                prefix_usage()
            ),
        }
        index += 1;
    }

    Ok(parsed)
}

fn apply_prefix_flag(parsed: &mut PrefixArgs, flag: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{flag} requires a path");
    }
    match flag {
        "--root" => parsed.root = Some(PathBuf::from(value)),
        "--proton" => parsed.proton = Some(PathBuf::from(value)),
        other => bail!("internal error: unknown prefix flag `{other}`"),
    }
    Ok(())
}

/// Report the free space on a filesystem, optionally against a requirement.
fn disk(args: &[String]) -> Result<()> {
    let parsed = parse_disk_args(args)?;
    if parsed.help {
        println!("{}", disk_usage());
        return Ok(());
    }

    let Some(path) = parsed.path.clone() else {
        bail!("expected a path\n\n{}", disk_usage());
    };

    let space = disk::space(&path)
        .with_context(|| format!("cannot read filesystem space for {}", path.display()))?;
    let check = parsed.need.map(|required| disk::SpaceCheck {
        required,
        available: space.available,
    });

    if parsed.json {
        let payload = serde_json::json!({
            "path": path,
            "total": space.total,
            "free": space.free,
            "available": space.available,
            "used_fraction": space.used_fraction(),
            "check": check,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!("Filesystem for {}", path.display());
        println!("  total        {}", disk::human_bytes(space.total));
        println!("  free         {}", disk::human_bytes(space.free));
        println!("  available    {}", disk::human_bytes(space.available));
        println!("  used         {:.1}%", space.used_fraction() * 100.0);
        match check {
            Some(check) if check.ok() => println!(
                "  need {}: enough ({} to spare)",
                disk::human_bytes(check.required),
                disk::human_bytes(check.available - check.required)
            ),
            Some(check) => println!(
                "  need {}: NOT ENOUGH ({} short)",
                disk::human_bytes(check.required),
                disk::human_bytes(check.shortfall())
            ),
            None => {}
        }
    }

    Ok(())
}

/// Parsed `disk` arguments.
#[derive(Debug, Default, PartialEq, Eq)]
struct DiskArgs {
    path: Option<PathBuf>,
    need: Option<u64>,
    json: bool,
    help: bool,
}

fn parse_disk_args(args: &[String]) -> Result<DiskArgs> {
    let mut parsed = DiskArgs::default();
    let mut index = 0;

    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--json" => parsed.json = true,
            "--need" => {
                index += 1;
                let value = args.get(index).with_context(|| "--need requires a size")?;
                parsed.need = Some(parse_size_bytes(value)?);
            }
            "-h" | "--help" => parsed.help = true,
            _ if arg.starts_with("--need=") => {
                let value = &arg["--need=".len()..];
                parsed.need = Some(parse_size_bytes(value)?);
            }
            other if !other.starts_with('-') => {
                if parsed.path.replace(PathBuf::from(other)).is_some() {
                    bail!("`disk` takes one path\n\n{}", disk_usage());
                }
            }
            other => bail!(
                "unexpected argument `{other}` for `disk`\n\n{}",
                disk_usage()
            ),
        }
        index += 1;
    }

    Ok(parsed)
}

/// Parse a byte count: `1500`, `20K`, `1.5GiB`, `512M`, `2T`.
///
/// A bare number is whole bytes; a suffix (`B`/`K`/`M`/`G`/`T`, with an optional
/// `i`/`iB`, case-insensitive) is base-1024 and may carry a decimal point. A
/// fractional value with no suffix is refused, so `1.5` cannot silently become
/// one byte.
fn parse_size_bytes(value: &str) -> Result<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("size is empty");
    }

    let split = trimmed
        .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
        .unwrap_or(trimmed.len());
    let (number, suffix) = trimmed.split_at(split);
    if number.is_empty() {
        bail!("`{value}` is not a size");
    }

    let multiplier: u64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        "t" | "tb" | "tib" => 1024_u64.pow(4),
        other => bail!("unknown size suffix `{other}` in `{value}`"),
    };

    if number.contains('.') {
        if multiplier == 1 {
            bail!("`{value}` needs a unit for a fractional size (e.g. `1.5GiB`)");
        }
        let amount: f64 = number
            .parse()
            .with_context(|| format!("`{number}` is not a number"))?;
        if !amount.is_finite() || amount < 0.0 {
            bail!("`{value}` is not a size");
        }
        Ok((amount * multiplier as f64) as u64)
    } else {
        let amount: u64 = number
            .parse()
            .with_context(|| format!("`{number}` is not a whole number"))?;
        Ok(amount.saturating_mul(multiplier))
    }
}

///
/// The GUI regenerates the user-level entry on startup so an uninstalled
/// checkout still appears in menus; this command is the same renderer without a
/// webview, and `--print` is how `packaging/pitagames.desktop` is regenerated:
///
/// ```text
/// pitagames-cli desktop-entry --print > packaging/pitagames.desktop
/// ```
fn desktop_entry(args: &[String]) -> Result<()> {
    let parsed = parse_desktop_entry_args(args)?;
    if parsed.help {
        println!("{}", desktop_entry_usage());
        return Ok(());
    }

    let exec = if parsed.exec.is_empty() {
        vec!["pitagames".to_string()]
    } else {
        parsed.exec.clone()
    };
    let entry = DesktopEntry::default_for(exec);
    let path = match parsed.path.clone() {
        Some(path) => path,
        None => pitagames_core::paths::desktop_entry_file()
            .context("cannot determine the XDG data dir; pass --path <file>")?,
    };

    if parsed.print {
        print!("{}", entry.render()?);
        return Ok(());
    }

    if parsed.remove {
        let removed = DesktopEntry::remove(&path)?;
        if parsed.json {
            let payload = serde_json::json!({
                "path": path,
                "removed": removed,
            });
            println!("{}", serde_json::to_string_pretty(&payload)?);
        } else if removed {
            println!("Removed {}", path.display());
        } else {
            println!("No desktop entry at {}", path.display());
        }
        return Ok(());
    }

    if parsed.write {
        let outcome = entry.install(&path)?;
        if parsed.json {
            let payload = serde_json::json!({
                "path": path,
                "outcome": outcome.label(),
                "exec": entry.exec,
            });
            println!("{}", serde_json::to_string_pretty(&payload)?);
        } else {
            println!("{} {}", outcome.label(), path.display());
        }
        return Ok(());
    }

    let current = entry.matches(&path)?;
    let packaged = desktop::packaged_entry(&pitagames_core::paths::data_dirs());
    if parsed.json {
        let payload = serde_json::json!({
            "path": path,
            "present": path.exists(),
            "current": current,
            "packaged": packaged,
            "exec": entry.exec,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!("Desktop entry: {}", path.display());
        println!(
            "  state:      {}",
            if !path.exists() {
                "absent"
            } else if current {
                "current"
            } else {
                "stale (run --write)"
            }
        );
        println!(
            "  packaged:   {}",
            if packaged {
                "yes (the package owns the entry; the GUI leaves it alone)"
            } else {
                "no"
            }
        );
        println!("  exec:       {}", entry.exec.join(" "));
    }

    Ok(())
}

/// Parsed `desktop-entry` arguments.
#[derive(Debug, Default, PartialEq, Eq)]
struct DesktopEntryArgs {
    exec: Vec<String>,
    path: Option<PathBuf>,
    print: bool,
    write: bool,
    remove: bool,
    json: bool,
    help: bool,
}

fn parse_desktop_entry_args(args: &[String]) -> Result<DesktopEntryArgs> {
    let mut parsed = DesktopEntryArgs::default();
    let mut index = 0;

    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--print" => parsed.print = true,
            "--write" => parsed.write = true,
            "--remove" => parsed.remove = true,
            "--json" => parsed.json = true,
            "--exec" | "--path" => {
                index += 1;
                let value = args
                    .get(index)
                    .with_context(|| format!("{arg} requires a value"))?;
                apply_desktop_entry_flag(&mut parsed, arg, value)?;
            }
            "-h" | "--help" => parsed.help = true,
            _ if arg.starts_with("--exec=") || arg.starts_with("--path=") => {
                let (flag, value) = arg
                    .split_once('=')
                    .expect("the guard above requires an `=`");
                apply_desktop_entry_flag(&mut parsed, flag, value)?;
            }
            other => bail!(
                "unexpected argument `{other}` for `desktop-entry`\n\n{}",
                desktop_entry_usage()
            ),
        }
        index += 1;
    }

    let actions = [parsed.print, parsed.write, parsed.remove]
        .into_iter()
        .filter(|chosen| *chosen)
        .count();
    if actions > 1 {
        bail!(
            "--print, --write and --remove are exclusive\n\n{}",
            desktop_entry_usage()
        );
    }

    Ok(parsed)
}

fn apply_desktop_entry_flag(parsed: &mut DesktopEntryArgs, flag: &str, value: &str) -> Result<()> {
    match flag {
        "--exec" => {
            if value.is_empty() {
                bail!("--exec requires a path or program name");
            }
            parsed.exec.push(value.to_string());
        }
        "--path" => {
            if value.is_empty() {
                bail!("--path requires a file path");
            }
            parsed.path = Some(PathBuf::from(value));
        }
        other => bail!("internal error: unknown desktop-entry flag `{other}`"),
    }
    Ok(())
}

/// Ask a running launcher to come to the front.
///
/// The second launch of the GUI does exactly this through
/// `pitagames_core::activation`; the command exists so the channel can be
/// exercised without a second window, and so a desktop shortcut or hotkey can
/// raise the launcher.
fn activate(args: &[String]) -> Result<()> {
    let mut path: Option<PathBuf> = None;
    let mut json = false;

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--json" => json = true,
            "--socket" => {
                index += 1;
                let value = args
                    .get(index)
                    .with_context(|| "--socket requires a path")?;
                path = Some(PathBuf::from(value));
            }
            "-h" | "--help" => {
                println!("{}", activate_usage());
                return Ok(());
            }
            _ if arg.starts_with("--socket=") => {
                let value = &arg["--socket=".len()..];
                if value.is_empty() {
                    bail!("--socket requires a path");
                }
                path = Some(PathBuf::from(value));
            }
            other => bail!(
                "unexpected argument `{other}` for `activate`\n\n{}",
                activate_usage()
            ),
        }
        index += 1;
    }

    let socket = match path {
        Some(path) => path,
        None => pitagames_core::paths::activation_socket()
            .context("cannot determine the XDG data dir; pass --socket <path>")?,
    };

    match pitagames_core::activation::request_focus(&socket) {
        Ok(()) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "socket": socket,
                        "activated": true,
                    }))?
                );
            } else {
                println!("Asked the running PitaGames to come to the front.");
            }
            Ok(())
        }
        Err(error) => bail!(
            "no running PitaGames answered on {}: {error}",
            socket.display()
        ),
    }
}

/// Read or manage the Steam Web API key and the account it unlocks.
///
/// The key is the only secret PitaGames stores; this command is how it is set
/// on a headless box, and how a key is checked before the GUI is involved. It
/// reads and writes through the same `pitagames-secrets` chain the GUI uses, so
/// `--set-key` here is visible there immediately.
///
/// `--refresh` calls `IPlayerService/GetOwnedGames` once and prints the result.
/// It deliberately does not touch the SQLite cache: the GUI owns that, and a
/// diagnostic must not leave state behind.
fn steam_account(args: &[String]) -> Result<()> {
    let parsed = parse_steam_account_args(args)?;
    if parsed.help {
        println!("{}", steam_account_usage());
        return Ok(());
    }

    let root = parsed.steam_root.clone().or_else(discover_steam_root);

    if let Some(raw) = parsed.set_key.clone() {
        let key = if raw == "-" { read_stdin_key()? } else { raw };
        let key = SteamApiKey::parse(&key).context("not a usable Steam Web API key")?;
        let store =
            SteamKeyStore::default_chain().context("no config directory to store the key in")?;
        let saved = store.save(&key).context("could not store the key")?;
        for warning in &saved.warnings {
            eprintln!("warning: {warning}");
        }
        println!(
            "Stored the key in the {} as {}.",
            saved.backend,
            key.masked()
        );
    }

    if parsed.forget {
        let store =
            SteamKeyStore::default_chain().context("no config directory to remove the key from")?;
        let forgotten = store.forget().context("could not remove the key")?;
        for warning in &forgotten.warnings {
            eprintln!("warning: {warning}");
        }
        if forgotten.removed {
            println!("Removed the stored key.");
        } else {
            println!("No key was stored.");
        }
    }

    let status = match SteamKeyStore::default_chain() {
        Some(store) => store.load().context("could not read the stored key")?,
        None => pitagames_secrets::KeyStatus {
            key: None,
            backend: None,
            warnings: vec!["no config directory (HOME and XDG_CONFIG_HOME are unset)".into()],
        },
    };
    let account_id = root.as_deref().and_then(steam::active_user_id);
    let steam_id = account_id.map(steam::steam_id64);
    let limit = parsed.limit;
    let json = parsed.json;

    let owned = if parsed.refresh {
        let key = status
            .key
            .as_ref()
            .context("no Steam Web API key is configured; run `steam-account --set-key`")?;
        let steam_id = steam_id
            .clone()
            .context("no Steam account found in userdata; sign into Steam once")?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("could not start the async runtime")?;
        let client = pitagames_metadata::OwnedClient::new()
            .context("could not build the owned-games client")?;
        Some(
            runtime
                .block_on(client.fetch(key.expose_secret(), &steam_id))
                .context("the owned-games request failed")?,
        )
    } else {
        None
    };

    if json {
        let payload = serde_json::json!({
            "steam_root": root,
            "account_id": account_id,
            "steam_id": steam_id,
            "configured": status.is_configured(),
            "backend": status.backend.map(|backend| backend.as_str()),
            "masked_key": status.key.as_ref().map(SteamApiKey::masked),
            "warnings": status.warnings,
            "owned": owned.as_ref().map(|owned| serde_json::json!({
                "game_count": owned.game_count,
                "games": owned.games,
            })),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    match root.as_deref() {
        Some(root) => println!("Steam root:  {}", root.display()),
        None => println!("Steam root:  (none found)"),
    }
    match (&status.key, status.backend) {
        (Some(key), Some(backend)) => println!("Key:         {} ({backend})", key.masked()),
        _ => println!("Key:         not configured — the library is installed-only"),
    }
    match (&steam_id, account_id) {
        (Some(id), Some(account_id)) => {
            println!("SteamID:     {id} (Steam userdata account {account_id})");
        }
        _ => println!("SteamID:     (no account in userdata; sign into Steam once)"),
    }
    for warning in &status.warnings {
        println!("Warning:     {warning}");
    }

    if let Some(owned) = owned {
        println!();
        println!(
            "Owned games: {} ({} rows returned)",
            owned.game_count,
            owned.games.len()
        );
        for game in owned.games.iter().take(limit) {
            println!("  {}  {}", game.id.store_id, game.title);
        }
        if owned.games.len() > limit {
            println!("  …and {} more", owned.games.len() - limit);
        }
    }

    Ok(())
}

/// Read one line from stdin, for `--set-key -`.
fn read_stdin_key() -> Result<String> {
    use std::io::Read as _;

    let mut buffer = String::new();
    std::io::stdin()
        .read_to_string(&mut buffer)
        .context("could not read the key from stdin")?;
    Ok(buffer.trim().to_string())
}

/// Parsed `steam-account` arguments.
#[derive(Debug, Default, PartialEq, Eq)]
struct SteamAccountArgs {
    steam_root: Option<PathBuf>,
    json: bool,
    set_key: Option<String>,
    forget: bool,
    refresh: bool,
    limit: usize,
    help: bool,
}

fn parse_steam_account_args(args: &[String]) -> Result<SteamAccountArgs> {
    let mut parsed = SteamAccountArgs {
        limit: 25,
        ..SteamAccountArgs::default()
    };

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--json" => parsed.json = true,
            "--forget-key" => parsed.forget = true,
            "--refresh" => parsed.refresh = true,
            "--steam-root" => {
                index += 1;
                let value = args
                    .get(index)
                    .with_context(|| "--steam-root requires a path")?;
                parsed.steam_root = Some(PathBuf::from(value));
            }
            "--set-key" => {
                index += 1;
                let value = args
                    .get(index)
                    .with_context(|| "--set-key requires a key, or `-` to read stdin")?;
                parsed.set_key = Some(value.clone());
            }
            "--limit" => {
                index += 1;
                let value = args
                    .get(index)
                    .with_context(|| "--limit requires a number")?;
                parsed.limit = value
                    .parse()
                    .with_context(|| format!("`{value}` is not a number"))?;
            }
            "-h" | "--help" => parsed.help = true,
            _ if arg.starts_with("--steam-root=") => {
                let value = &arg["--steam-root=".len()..];
                if value.is_empty() {
                    bail!("--steam-root requires a path");
                }
                parsed.steam_root = Some(PathBuf::from(value));
            }
            _ if arg.starts_with("--set-key=") => {
                let value = &arg["--set-key=".len()..];
                if value.is_empty() {
                    bail!("--set-key requires a key, or `-` to read stdin");
                }
                parsed.set_key = Some(value.to_string());
            }
            _ if arg.starts_with("--limit=") => {
                let value = &arg["--limit=".len()..];
                parsed.limit = value
                    .parse()
                    .with_context(|| format!("`{value}` is not a number"))?;
            }
            other => bail!(
                "unexpected argument `{other}` for `steam-account`\n\n{}",
                steam_account_usage()
            ),
        }
        index += 1;
    }

    Ok(parsed)
}

/// The Epic Games Store backend, through the `legendary` CLI.
///
/// Every subcommand is a thin wrapper over `stores::epic`, the same code the GUI
/// will call: `status`/`version` for the store row, `list`/`installed` for the
/// library, `auth`/`logout` for the session, and `install`/`verify`/`launch`
/// with the driver's live progress. `--dry-run` prints the argv without running
/// anything, which is how the preset is checked on a machine without legendary.
fn epic(args: &[String]) -> Result<()> {
    let parsed = parse_store_args(args, StoreCli::Epic)?;
    if parsed.help {
        println!("{}", epic_usage());
        return Ok(());
    }
    let Some(subcommand) = parsed.positional.first().cloned() else {
        bail!("missing subcommand for `epic`\n\n{}", epic_usage());
    };
    let backend = Epic::with_program(&parsed.program);

    match subcommand.as_str() {
        "status" => epic_status(&backend, &parsed),
        "list" => epic_owned(&backend, &parsed),
        "installed" => epic_installed(&backend, &parsed),
        "auth" => epic_auth(&backend, &parsed),
        "logout" => epic_logout(&backend, &parsed),
        "install" => epic_install(&backend, &parsed),
        "verify" => epic_verify(&backend, &parsed),
        "launch" => epic_launch(&backend, &parsed),
        other => bail!("unknown `epic` subcommand `{other}`\n\n{}", epic_usage()),
    }
}

fn epic_status(backend: &Epic, parsed: &StoreArgs) -> Result<()> {
    expect_positionals(parsed, 1)?;
    let status = backend.status(None);
    let options = store_options(parsed, false);

    let mut version = None;
    let mut account = None;
    let mut problem = None;
    if status.is_ready() {
        match backend.version(&options) {
            Ok(found) => version = found,
            Err(error) => problem = Some(format!("version: {error}")),
        }
        match backend.account(&options) {
            Ok(found) => account = Some(found),
            Err(error) => {
                if problem.is_none() {
                    problem = Some(format!("account: {error}"));
                }
            }
        }
    }

    if parsed.json {
        let value = serde_json::json!({
            "program": status.path().map(|path| path.display().to_string()),
            "ready": status.is_ready(),
            "missing": status.path().is_none(),
            "version": version,
            "account": account,
            "problem": problem,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    match status.path() {
        Some(path) => println!("legendary: {}", path.display()),
        None => println!(
            "legendary: not found in PATH — install it to enable Epic installs \
             (`pipx install legendary-gl`, or the AUR package)"
        ),
    }
    if let Some(version) = &version {
        println!("version: {version}");
    }
    if let Some(account) = &account {
        if account.logged_in() {
            println!(
                "account: {} ({} owned, {} installed)",
                account.account, account.games_available, account.games_installed
            );
        } else {
            println!("account: not logged in — run `pitagames-cli epic auth`");
        }
        println!("config: {}", account.config_directory);
    }
    if let Some(problem) = &problem {
        println!("problem: {problem}");
    }
    Ok(())
}

fn epic_owned(backend: &Epic, parsed: &StoreArgs) -> Result<()> {
    expect_positionals(parsed, 1)?;
    let games = backend.owned(&store_options(parsed, false))?;
    if parsed.json {
        println!("{}", serde_json::to_string_pretty(&games)?);
        return Ok(());
    }
    for game in &games {
        println!("{:<32}  {}", game.id.store_id, game.title);
    }
    println!("\n{} owned game(s).", games.len());
    Ok(())
}

fn epic_installed(backend: &Epic, parsed: &StoreArgs) -> Result<()> {
    expect_positionals(parsed, 1)?;
    let games = backend.installed(&store_options(parsed, false))?;
    if parsed.json {
        println!("{}", serde_json::to_string_pretty(&games)?);
        return Ok(());
    }
    for game in &games {
        println!(
            "{:<32}  {:<40}  {}",
            game.id.store_id,
            game.title,
            game.install_dir
                .as_ref()
                .map(|dir| dir.display().to_string())
                .unwrap_or_else(|| "-".to_string())
        );
    }
    println!("\n{} installed game(s).", games.len());
    Ok(())
}

/// The code-paste sign-in.
///
/// legendary opens the Epic login page and asks for the `authorizationCode` from
/// its JSON response. That is an `input()` prompt with no trailing newline, which
/// is exactly what the driver's partial-line flush exists for. A prompt line ends
/// with `:`, so each one gets one line of stdin. A PTY echoes the answer, so the
/// transcript on disk can contain the code; it is never printed or stored.
fn epic_auth(backend: &Epic, parsed: &StoreArgs) -> Result<()> {
    use std::io::{BufRead as _, Write as _};

    expect_positionals(parsed, 1)?;
    let options = store_options(parsed, true);
    let handle = backend.spawn(&CliCommand::Auth, &options)?;
    let mut stdin = std::io::stdin().lock();
    let mut cancelled = false;

    while let Some(event) = handle.next_event()? {
        match event {
            ProgressEvent::Line(text)
            | ProgressEvent::Stage(text)
            | ProgressEvent::Percent(_, text) => {
                println!("{text}");
                if text.trim_end().ends_with(':') {
                    print!("> ");
                    std::io::stdout().flush().ok();
                    let mut line = String::new();
                    if stdin.read_line(&mut line)? == 0 {
                        handle.cancel();
                        cancelled = true;
                        break;
                    }
                    handle.write_stdin(line.as_bytes())?;
                }
            }
            // The verdict is the report below; a `Done` here only means
            // legendary exited 0, which it also does for a refusal.
            ProgressEvent::Done => {}
            ProgressEvent::Failed { code, message } => {
                eprintln!("legendary auth failed ({code}): {message}");
            }
        }
    }

    let outcome = handle.wait()?;
    if cancelled {
        bail!("authentication cancelled: stdin closed");
    }
    // legendary can log an error and still exit 0, so the log lines count too.
    let report = backend.report(&CliCommand::Auth, outcome);
    if !report.succeeded() {
        bail!(
            "legendary auth did not finish: {}",
            report
                .failure_message()
                .unwrap_or_else(|| "no session was stored".to_string())
        );
    }
    println!("Authenticated.");
    Ok(())
}

fn epic_logout(backend: &Epic, parsed: &StoreArgs) -> Result<()> {
    expect_positionals(parsed, 1)?;
    let report = backend.stream(
        &CliCommand::Logout,
        &store_options(parsed, false),
        store_event,
    )?;
    if !report.succeeded() {
        if let Some(log) = report.log_path() {
            eprintln!("Transcript: {}", log.display());
        }
        bail!(
            "legendary logout failed: {}",
            report.failure_message().unwrap_or_default()
        );
    }
    println!("Stored Epic session removed.");
    Ok(())
}

fn epic_install(backend: &Epic, parsed: &StoreArgs) -> Result<()> {
    expect_positionals(parsed, 2)?;
    let id = positional(parsed, 1, "Epic app name")?;
    let dir = match &parsed.dir {
        Some(dir) => dir.clone(),
        None => epic::install_dir(id)
            .with_context(|| "no data directory; pass --dir <path>".to_string())?,
    };
    let command = CliCommand::Install {
        id: id.to_string(),
        install_dir: dir.clone(),
    };

    if parsed.dry_run {
        print_planned(backend.program(), backend.spec(), &command);
        println!("install dir: {}", dir.display());
        return Ok(());
    }

    let report = backend.stream(&command, &store_options(parsed, false), store_event)?;
    if !report.succeeded() {
        if let Some(log) = report.log_path() {
            eprintln!("Transcript: {}", log.display());
        }
        bail!(
            "legendary install failed: {}",
            report.failure_message().unwrap_or_default()
        );
    }
    println!("Installed {id} into {}.", dir.display());
    if let Some(log) = report.log_path() {
        println!("Transcript: {}", log.display());
    }
    Ok(())
}

fn epic_verify(backend: &Epic, parsed: &StoreArgs) -> Result<()> {
    expect_positionals(parsed, 2)?;
    let id = positional(parsed, 1, "Epic app name")?;
    let command = CliCommand::Verify {
        id: id.to_string(),
        install_dir: None,
    };

    if parsed.dry_run {
        print_planned(backend.program(), backend.spec(), &command);
        return Ok(());
    }

    let report = backend.stream(&command, &store_options(parsed, false), store_event)?;
    // The exit code is always 0 here; the verdict is the summary line.
    let verdict = pitagames_stores::epic::verify_report(&report.text);
    if parsed.json {
        let value = serde_json::json!({
            "verified": verdict.verified,
            "corrupted": verdict.corrupted,
            "missing": verdict.missing,
            "exited_zero": report.outcome.success,
            "log": report.log_path().map(|path| path.display().to_string()),
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else if verdict.verified {
        println!("Verified {id}: every file matches.");
    } else if verdict.corrupted > 0 || verdict.missing > 0 {
        println!(
            "{id}: {} file(s) corrupted, {} file(s) missing.",
            verdict.corrupted, verdict.missing
        );
    } else {
        println!("{id}: verification did not complete.");
    }

    if !verdict.verified {
        if let Some(log) = report.log_path() {
            eprintln!("Transcript: {}", log.display());
        }
        bail!(
            "legendary verify did not pass for {id}: {}",
            report
                .failure_message()
                .unwrap_or_else(|| "no verdict line".to_string())
        );
    }
    Ok(())
}

fn epic_launch(backend: &Epic, parsed: &StoreArgs) -> Result<()> {
    expect_positionals(parsed, 2)?;
    let id = positional(parsed, 1, "Epic app name")?;
    let command = CliCommand::Launch {
        id: id.to_string(),
        install_dir: None,
    };

    if parsed.dry_run {
        print_planned(backend.program(), backend.spec(), &command);
        return Ok(());
    }

    let report = backend.stream(&command, &store_options(parsed, false), store_event)?;
    // The game shares the PTY, so its own `[name] ERROR:` lines must not count;
    // only the exit status says whether the launch worked.
    if !report.exited_cleanly() {
        if let Some(log) = report.log_path() {
            eprintln!("Transcript: {}", log.display());
        }
        bail!(
            "legendary launch failed: {}",
            report.failure_message().unwrap_or_default()
        );
    }
    Ok(())
}

/// The Amazon Games backend, through the `nile` CLI.
///
/// The same shape as `epic`, with the two differences nile brings: the library
/// is cached locally and refreshed by `sync`, and every failure exits `0`, so
/// the verdicts come from the CLI's own log lines (`stores::report`).
fn amazon(args: &[String]) -> Result<()> {
    let parsed = parse_store_args(args, StoreCli::Amazon)?;
    if parsed.help {
        println!("{}", amazon_usage());
        return Ok(());
    }
    let Some(subcommand) = parsed.positional.first().cloned() else {
        bail!("missing subcommand for `amazon`\n\n{}", amazon_usage());
    };
    let backend = Amazon::with_program(&parsed.program);

    match subcommand.as_str() {
        "status" => amazon_status(&backend, &parsed),
        "list" => amazon_owned(&backend, &parsed),
        "installed" => amazon_installed(&backend, &parsed),
        "sync" => amazon_sync(&backend, &parsed),
        "auth" => amazon_auth(&backend, &parsed),
        "logout" => amazon_logout(&backend, &parsed),
        "install" => amazon_install(&backend, &parsed),
        "verify" => amazon_verify(&backend, &parsed),
        "launch" => amazon_launch(&backend, &parsed),
        other => bail!(
            "unknown `amazon` subcommand `{other}`\n\n{}",
            amazon_usage()
        ),
    }
}

fn amazon_status(backend: &Amazon, parsed: &StoreArgs) -> Result<()> {
    expect_positionals(parsed, 1)?;
    let status = backend.status(None);
    let options = store_options(parsed, false);

    let mut version = None;
    let mut account = None;
    let mut problem = None;
    if status.is_ready() {
        match backend.version(&options) {
            Ok(found) => version = found,
            Err(error) => problem = Some(format!("version: {error}")),
        }
        match backend.account(&options) {
            Ok(found) => account = Some(found),
            Err(error) => {
                if problem.is_none() {
                    problem = Some(format!("account: {error}"));
                }
            }
        }
    }

    if parsed.json {
        let value = serde_json::json!({
            "program": status.path().map(|path| path.display().to_string()),
            "ready": status.is_ready(),
            "missing": status.path().is_none(),
            "version": version,
            "account": account,
            "problem": problem,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    match status.path() {
        Some(path) => println!("nile: {}", path.display()),
        None => println!(
            "nile: not found in PATH — install it to enable Amazon installs \
             (the upstream release binary, or the AUR `nile-git` package)"
        ),
    }
    if let Some(version) = &version {
        println!("version: {version}");
    }
    if let Some(account) = &account {
        if account.signed_in() {
            println!("account: {}", account.username);
        } else {
            println!("account: not signed in — run `pitagames-cli amazon auth`");
        }
    }
    if let Some(problem) = &problem {
        println!("problem: {problem}");
    }
    Ok(())
}

fn amazon_owned(backend: &Amazon, parsed: &StoreArgs) -> Result<()> {
    expect_positionals(parsed, 1)?;
    let games = backend.owned(&store_options(parsed, false))?;
    if parsed.json {
        println!("{}", serde_json::to_string_pretty(&games)?);
        return Ok(());
    }
    for game in &games {
        println!("{:<40}  {}", game.id.store_id, game.title);
    }
    println!(
        "\n{} owned game(s); `pitagames-cli amazon sync` refreshes the local library.",
        games.len()
    );
    Ok(())
}

fn amazon_installed(backend: &Amazon, parsed: &StoreArgs) -> Result<()> {
    expect_positionals(parsed, 1)?;
    let games = backend.installed(&store_options(parsed, false))?;
    if parsed.json {
        println!("{}", serde_json::to_string_pretty(&games)?);
        return Ok(());
    }
    for game in &games {
        println!(
            "{:<40}  {:<40}  {}",
            game.id.store_id,
            game.title,
            game.install_dir
                .as_ref()
                .map(|dir| dir.display().to_string())
                .unwrap_or_else(|| "-".to_string())
        );
    }
    println!("\n{} installed game(s).", games.len());
    Ok(())
}

fn amazon_sync(backend: &Amazon, parsed: &StoreArgs) -> Result<()> {
    expect_positionals(parsed, 1)?;
    if parsed.dry_run {
        print_planned(backend.program(), backend.spec(), &CliCommand::Sync);
        return Ok(());
    }
    let report = backend.stream(
        &CliCommand::Sync,
        &store_options(parsed, false),
        store_event,
    )?;
    if !report.succeeded() {
        if let Some(log) = report.log_path() {
            eprintln!("Transcript: {}", log.display());
        }
        bail!(
            "nile library sync failed: {}",
            report.failure_message().unwrap_or_default()
        );
    }
    println!("Amazon library synced.");
    Ok(())
}

/// The Amazon sign-in.
///
/// nile prints a login URL, waits for ENTER (which is where it opens the
/// browser), and then asks for the **final `amazon.com` address** it redirected
/// to — that URL carries the `openid.oa2.authorization_code`. Amazon's 2FA/TOTP
/// happens in the browser, so there is no code prompt of our own; the pasted URL
/// is the long line the driver relays. A PTY echoes it, so the transcript can
/// contain the authorization code: local diagnostics only, never printed or
/// stored.
fn amazon_auth(backend: &Amazon, parsed: &StoreArgs) -> Result<()> {
    use std::io::{BufRead as _, Write as _};

    expect_positionals(parsed, 1)?;
    let handle = backend.spawn(&CliCommand::Auth, &store_options(parsed, true))?;
    let mut stdin = std::io::stdin().lock();
    let mut cancelled = false;

    while let Some(event) = handle.next_event()? {
        match event {
            ProgressEvent::Line(text)
            | ProgressEvent::Stage(text)
            | ProgressEvent::Percent(_, text) => {
                println!("{text}");
                if text.contains("Press ENTER") {
                    // Nothing to type; nile opens the login page here.
                    println!("(opening the login page in your browser)");
                    handle.write_stdin(b"\n")?;
                } else if text.trim_end().ends_with(':') {
                    println!(
                        "Sign in, then paste the amazon.com address you were redirected to, \
                         and press Enter."
                    );
                    print!("> ");
                    std::io::stdout().flush().ok();
                    let mut line = String::new();
                    if stdin.read_line(&mut line)? == 0 {
                        handle.cancel();
                        cancelled = true;
                        break;
                    }
                    handle.write_stdin(line.as_bytes())?;
                }
            }
            // The verdict is the report below; nile exits 0 for a refusal too.
            ProgressEvent::Done => {}
            ProgressEvent::Failed { code, message } => {
                eprintln!("nile auth failed ({code}): {message}");
            }
        }
    }

    let outcome = handle.wait()?;
    if cancelled {
        bail!("authentication cancelled: stdin closed");
    }
    // nile logs an ERROR and exits 0 when the pasted URL carried no code, so
    // the exit status alone would report a failed sign-in as a success.
    let report = backend.report(&CliCommand::Auth, outcome);
    if !report.succeeded() {
        bail!(
            "nile auth did not finish: {}",
            report
                .failure_message()
                .unwrap_or_else(|| "no session was stored".to_string())
        );
    }
    println!("Signed in.");
    Ok(())
}

fn amazon_logout(backend: &Amazon, parsed: &StoreArgs) -> Result<()> {
    expect_positionals(parsed, 1)?;
    let report = backend.stream(
        &CliCommand::Logout,
        &store_options(parsed, false),
        store_event,
    )?;
    if !report.succeeded() {
        if let Some(log) = report.log_path() {
            eprintln!("Transcript: {}", log.display());
        }
        bail!(
            "nile logout failed: {}",
            report.failure_message().unwrap_or_default()
        );
    }
    println!("Stored Amazon session removed.");
    Ok(())
}

fn amazon_install(backend: &Amazon, parsed: &StoreArgs) -> Result<()> {
    expect_positionals(parsed, 2)?;
    let id = positional(parsed, 1, "Amazon product id")?;
    let dir = match &parsed.dir {
        Some(dir) => dir.clone(),
        None => amazon::install_dir(&amazon_display_name(backend, parsed, id))
            .with_context(|| "no data directory; pass --dir <path>".to_string())?,
    };
    let command = CliCommand::Install {
        id: id.to_string(),
        install_dir: dir.clone(),
    };

    if parsed.dry_run {
        print_planned(backend.program(), backend.spec(), &command);
        println!("install dir: {}", dir.display());
        return Ok(());
    }

    let report = backend.stream(&command, &store_options(parsed, false), store_event)?;
    if !report.succeeded() {
        if let Some(log) = report.log_path() {
            eprintln!("Transcript: {}", log.display());
        }
        bail!(
            "nile install failed: {}",
            report.failure_message().unwrap_or_default()
        );
    }
    println!("Installed {id} into {}.", dir.display());
    if let Some(log) = report.log_path() {
        println!("Transcript: {}", log.display());
    }
    Ok(())
}

/// The title nile's cached library has for `id`, else the id itself.
///
/// The default install directory is named after the game, and the id alone
/// (`amzn1.adg.product.…`) is unreadable. nile's library is a local cache, so
/// the lookup is cheap; a signed-out or empty cache just keeps the id.
fn amazon_display_name(backend: &Amazon, parsed: &StoreArgs, id: &str) -> String {
    backend
        .owned(&store_options(parsed, false))
        .ok()
        .and_then(|games| {
            games
                .into_iter()
                .find(|game| game.id.store_id == id)
                .map(|game| game.title)
        })
        .unwrap_or_else(|| id.to_string())
}

fn amazon_verify(backend: &Amazon, parsed: &StoreArgs) -> Result<()> {
    expect_positionals(parsed, 2)?;
    let id = positional(parsed, 1, "Amazon product id")?;
    let command = CliCommand::Verify {
        id: id.to_string(),
        install_dir: None,
    };

    if parsed.dry_run {
        print_planned(backend.program(), backend.spec(), &command);
        return Ok(());
    }

    let report = backend.stream(&command, &store_options(parsed, false), store_event)?;
    // nile's `verify` re-downloads missing files and prints no verdict line, so
    // the classification is the CLI's own error lines.
    let verdict = amazon::verify_report(&report);
    if parsed.json {
        let value = serde_json::json!({
            "verified": verdict.verified,
            "errors": verdict.errors,
            "exited_zero": report.outcome.success,
            "log": report.log_path().map(|path| path.display().to_string()),
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else if verdict.verified {
        println!("Verified {id}: nile repaired nothing and logged no error.");
    } else {
        println!("{id}: verification did not complete.");
    }

    if !verdict.verified {
        if let Some(log) = report.log_path() {
            eprintln!("Transcript: {}", log.display());
        }
        bail!(
            "nile verify did not pass for {id}: {}",
            report
                .failure_message()
                .unwrap_or_else(|| "no verdict line".to_string())
        );
    }
    Ok(())
}

fn amazon_launch(backend: &Amazon, parsed: &StoreArgs) -> Result<()> {
    expect_positionals(parsed, 2)?;
    let id = positional(parsed, 1, "Amazon product id")?;
    let command = CliCommand::Launch {
        id: id.to_string(),
        install_dir: None,
    };

    if parsed.dry_run {
        print_planned(backend.program(), backend.spec(), &command);
        return Ok(());
    }

    let report = backend.stream(&command, &store_options(parsed, false), store_event)?;
    // nile exits 0 even when it refuses to launch, so its own `Launching` marker
    // is what separates "the game ran" from "nile gave up".
    if !amazon::launched(&report) {
        if let Some(log) = report.log_path() {
            eprintln!("Transcript: {}", log.display());
        }
        bail!(
            "nile launch did not start {id}: {}",
            report
                .failure_message()
                .unwrap_or_else(|| "no launch marker and exit 0".to_string())
        );
    }
    Ok(())
}

/// The GOG backend: `lgogdownloader` for the library and login, `gogdl` for
/// installs, repairs and launches.
///
/// The two programs are not interchangeable (see `stores::gog`): only
/// `lgogdownloader` lists the account's games with titles and only `gogdl`
/// installs a launchable directory and can start it. `--program` overrides the
/// library CLI and `--game-program` the game CLI, so a fixture or a second
/// install can drive either.
fn gog(args: &[String]) -> Result<()> {
    let parsed = parse_store_args(args, StoreCli::Gog)?;
    if parsed.help {
        println!("{}", gog_usage());
        return Ok(());
    }
    let Some(subcommand) = parsed.positional.first().cloned() else {
        bail!("missing subcommand for `gog`\n\n{}", gog_usage());
    };
    let backend = gog_backend(&parsed);

    match subcommand.as_str() {
        "status" => gog_status(&backend, &parsed),
        "list" => gog_owned(&backend, &parsed),
        "auth" => gog_auth(&backend, &parsed),
        "install" => gog_install(&backend, &parsed),
        "verify" => gog_verify(&backend, &parsed),
        "launch" => gog_launch(&backend, &parsed),
        other => bail!("unknown `gog` subcommand `{other}`\n\n{}", gog_usage()),
    }
}

fn gog_backend(parsed: &StoreArgs) -> Gog {
    let game = parsed
        .game_program
        .clone()
        .unwrap_or_else(|| gog::GAME_PROGRAM.to_string());
    // An explicit `--auth-file` wins; otherwise `gogdl`'s session lives under
    // PitaGames' own config dir (never Heroic's).
    let auth = parsed.auth_file.clone().or_else(gog::auth_file);
    Gog::with_programs_and_auth(parsed.program.clone(), game, auth)
}

fn gog_status(backend: &Gog, parsed: &StoreArgs) -> Result<()> {
    expect_positionals(parsed, 1)?;
    let library = backend.library_status(None);
    let game = backend.game_status(None);
    let options = store_options(parsed, false);

    let mut version = None;
    let mut account = None;
    let mut problem = None;
    if library.is_ready() {
        match backend.version(&options) {
            Ok(found) => version = found,
            Err(error) => problem = Some(format!("version: {error}")),
        }
        match backend.account(&options) {
            Ok(found) => account = Some(found),
            Err(error) => {
                if problem.is_none() {
                    problem = Some(format!("account: {error}"));
                }
            }
        }
    }

    if parsed.json {
        let value = serde_json::json!({
            "library": {
                "program": library.path().map(|path| path.display().to_string()),
                "ready": library.is_ready(),
            },
            "game": {
                "program": game.path().map(|path| path.display().to_string()),
                "ready": game.is_ready(),
            },
            "version": version,
            "logged_in": account.map(|account| account.logged_in),
            "problem": problem,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    match library.path() {
        Some(path) => println!("lgogdownloader: {}", path.display()),
        None => println!(
            "lgogdownloader: not found in PATH — install it to list GOG games and \
             sign in (`pacman -S lgogdownloader`)"
        ),
    }
    match game.path() {
        Some(path) => println!("gogdl: {}", path.display()),
        None => println!(
            "gogdl: not found in PATH — install it to install, repair and launch \
             GOG games (Heroic ships a `gogdl` build)"
        ),
    }
    if let Some(version) = &version {
        println!("version: {version}");
    }
    if let Some(account) = &account {
        if account.logged_in {
            println!("account: signed in");
        } else {
            println!("account: not signed in — run `pitagames-cli gog auth`");
        }
    }
    if let Some(problem) = &problem {
        println!("problem: {problem}");
    }
    Ok(())
}

fn gog_owned(backend: &Gog, parsed: &StoreArgs) -> Result<()> {
    expect_positionals(parsed, 1)?;
    let games = backend.owned(&store_options(parsed, false))?;
    if parsed.json {
        println!("{}", serde_json::to_string_pretty(&games)?);
        return Ok(());
    }
    for game in &games {
        println!("{:<40}  {}", game.id.store_id, game.title);
    }
    println!("\n{} owned game(s).", games.len());
    Ok(())
}

/// The GOG sign-in.
///
/// `lgogdownloader --browser-login` prints the GOG login URL, an instruction
/// line, and a `URL: ` prompt, then reads the **full redirect URL** from stdin
/// and pulls the OAuth code out of it. That is the same code-paste shape as
/// Epic's and Amazon's flows, so it answers one stdin line per prompt. A PTY
/// echoes it, so the transcript can contain the code: local diagnostics only.
fn gog_auth(backend: &Gog, parsed: &StoreArgs) -> Result<()> {
    use std::io::{BufRead as _, Write as _};

    expect_positionals(parsed, 1)?;
    let handle = backend.spawn(&CliCommand::Auth, &store_options(parsed, true))?;
    let mut stdin = std::io::stdin().lock();
    let mut cancelled = false;

    while let Some(event) = handle.next_event()? {
        match event {
            ProgressEvent::Line(text)
            | ProgressEvent::Stage(text)
            | ProgressEvent::Percent(_, text) => {
                println!("{text}");
                if text.trim_end().ends_with(':') {
                    println!(
                        "Sign in in the browser, then paste the full URL you were \
                         redirected to and press Enter."
                    );
                    print!("> ");
                    std::io::stdout().flush().ok();
                    let mut line = String::new();
                    if stdin.read_line(&mut line)? == 0 {
                        handle.cancel();
                        cancelled = true;
                        break;
                    }
                    handle.write_stdin(line.as_bytes())?;
                }
            }
            // The verdict is the report below; a `Done` only means exit 0, which
            // is not the same as a stored session.
            ProgressEvent::Done => {}
            ProgressEvent::Failed { code, message } => {
                eprintln!("lgogdownloader auth failed ({code}): {message}");
            }
        }
    }

    let outcome = handle.wait()?;
    if cancelled {
        bail!("authentication cancelled: stdin closed");
    }
    let report = backend.report(&CliCommand::Auth, outcome);
    if !report.succeeded() {
        bail!(
            "lgogdownloader auth did not finish: {}",
            report
                .failure_message()
                .unwrap_or_else(|| "no session was stored".to_string())
        );
    }
    println!("Signed in.");
    Ok(())
}

fn gog_install(backend: &Gog, parsed: &StoreArgs) -> Result<()> {
    expect_positionals(parsed, 2)?;
    let id = positional(parsed, 1, "GOG product id")?;
    let dir = match &parsed.dir {
        Some(dir) => dir.clone(),
        None => gog::install_dir(&gog_display_name(backend, parsed, id))
            .with_context(|| "no data directory; pass --dir <path>".to_string())?,
    };
    let command = CliCommand::Install {
        id: id.to_string(),
        install_dir: dir.clone(),
    };

    if parsed.dry_run {
        print_planned(
            backend.program_for(&command),
            backend.spec_for(&command),
            &command,
        );
        println!("install dir: {}", dir.display());
        return Ok(());
    }

    let report = backend.stream(&command, &store_options(parsed, false), store_event)?;
    if !report.succeeded() {
        if let Some(log) = report.log_path() {
            eprintln!("Transcript: {}", log.display());
        }
        bail!(
            "gogdl install failed: {}",
            report.failure_message().unwrap_or_default()
        );
    }
    println!("Installed {id} into {}.", dir.display());
    if let Some(log) = report.log_path() {
        println!("Transcript: {}", log.display());
    }
    Ok(())
}

/// The title `lgogdownloader` has for `id`, else the id itself.
///
/// The default install directory is named after the game, and a bare
/// `1207660623` is unreadable. `--list json` is an account round-trip, so the
/// lookup is best-effort: a signed-out or failing library just keeps the id.
fn gog_display_name(backend: &Gog, parsed: &StoreArgs, id: &str) -> String {
    backend
        .owned(&store_options(parsed, false))
        .ok()
        .and_then(|games| {
            games
                .into_iter()
                .find(|game| game.id.store_id == id)
                .map(|game| game.title)
        })
        .unwrap_or_else(|| id.to_string())
}

fn gog_verify(backend: &Gog, parsed: &StoreArgs) -> Result<()> {
    expect_positionals(parsed, 2)?;
    let id = positional(parsed, 1, "GOG product id")?;
    let dir = parsed
        .dir
        .clone()
        .with_context(|| "gog verify needs the install directory: pass --dir <path>")?;
    let command = CliCommand::Verify {
        id: id.to_string(),
        install_dir: Some(dir),
    };

    if parsed.dry_run {
        print_planned(
            backend.program_for(&command),
            backend.spec_for(&command),
            &command,
        );
        return Ok(());
    }

    let report = backend.stream(&command, &store_options(parsed, false), store_event)?;
    // `gogdl repair` re-downloads what its manifest says is missing and exits
    // non-zero on failure, so "exit 0 and no error line" is the verdict.
    let verified = report.succeeded();
    if parsed.json {
        let value = serde_json::json!({
            "verified": verified,
            "errors": report.errors,
            "exited_zero": report.outcome.success,
            "log": report.log_path().map(|path| path.display().to_string()),
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else if verified {
        println!("Verified {id}: gogdl repaired nothing and logged no error.");
    } else {
        println!("{id}: verification did not complete.");
    }

    if !verified {
        if let Some(log) = report.log_path() {
            eprintln!("Transcript: {}", log.display());
        }
        bail!(
            "gogdl verify did not pass for {id}: {}",
            report
                .failure_message()
                .unwrap_or_else(|| "no verdict".to_string())
        );
    }
    Ok(())
}

fn gog_launch(backend: &Gog, parsed: &StoreArgs) -> Result<()> {
    expect_positionals(parsed, 2)?;
    let id = positional(parsed, 1, "GOG product id")?;
    let dir = parsed
        .dir
        .clone()
        .with_context(|| "gog launch needs the install directory: pass --dir <path>")?;
    let command = CliCommand::Launch {
        id: id.to_string(),
        install_dir: Some(dir),
    };

    if parsed.dry_run {
        print_planned(
            backend.program_for(&command),
            backend.spec_for(&command),
            &command,
        );
        return Ok(());
    }

    let report = backend.stream(&command, &store_options(parsed, false), store_event)?;
    // The game shares the PTY, so its own error lines must not fail the launch;
    // gogdl exits with the game's status, which is the verdict.
    if !report.exited_cleanly() {
        if let Some(log) = report.log_path() {
            eprintln!("Transcript: {}", log.display());
        }
        bail!(
            "gogdl launch failed: {}",
            report.failure_message().unwrap_or_default()
        );
    }
    Ok(())
}

fn store_event(event: &ProgressEvent) {
    match event {
        ProgressEvent::Line(text)
        | ProgressEvent::Stage(text)
        | ProgressEvent::Percent(_, text) => println!("{text}"),
        ProgressEvent::Done => println!("done."),
        ProgressEvent::Failed { code, message } => eprintln!("failed ({code}): {message}"),
    }
}

fn print_planned(program: &str, spec: &CliSpec, command: &CliCommand) {
    let argv = spec
        .argv(command)
        .unwrap_or_else(|error| vec![format!("<invalid template: {error}>")]);
    println!(
        "{}",
        pitagames_core::launch::render_shell_command(&[], program, &argv)
    );
}

/// The store status rows and the unified update list.
///
/// `stores` probes each CLI store without installing anything: is the program
/// on `PATH`, which version, is there a session, and how many games the store
/// sees. `stores updates` unifies the update signals — Steam's manifest state
/// and the CLI stores' own install flags — into one list. `stores sync` is the
/// write side: it folds each store's listings into the database's `games` table.
fn stores(args: &[String]) -> Result<()> {
    let parsed = parse_stores_args(args)?;
    if parsed.help {
        println!("{}", stores_usage());
        return Ok(());
    }

    if let Some(name) = parsed.subcommand.as_deref()
        && !matches!(name, "status" | "updates" | "sync")
    {
        bail!(
            "unknown subcommand `{name}` for `stores`\n\n{}",
            stores_usage()
        );
    }

    match parsed.subcommand.as_deref() {
        Some("updates") => stores_updates(&parsed),
        Some("sync") => stores_sync(&parsed),
        _ => stores_status(&parsed),
    }
}

/// The status rows for the CLI stores.
fn stores_status(parsed: &StoresArgs) -> Result<()> {
    if parsed.with_stores || parsed.steam_root.is_some() || parsed.db.is_some() || parsed.dry_run {
        bail!(
            "--with-stores, --steam-root, --db and --dry-run do not belong to `stores` (status)\n\n{}",
            stores_usage()
        );
    }

    let filters = store_filters(&parsed.filters)?;
    if (parsed.program.is_some() || parsed.game_program.is_some()) && filters.len() != 1 {
        bail!(
            "--program and --game-program need exactly one --store\n\n{}",
            stores_usage()
        );
    }

    let options = stores_options(parsed);
    let reports: Vec<StoreStatusReport> = status::CLI_STORES
        .iter()
        .copied()
        .filter(|store| filters.is_empty() || filters.contains(store))
        .map(|store| store_report(store, parsed, &options))
        .collect();

    if parsed.json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
        return Ok(());
    }

    for report in &reports {
        print_store_row(report);
    }
    Ok(())
}

/// The unified update list.
fn stores_updates(parsed: &StoresArgs) -> Result<()> {
    if !parsed.filters.is_empty()
        || parsed.list
        || parsed.program.is_some()
        || parsed.game_program.is_some()
        || parsed.db.is_some()
        || parsed.dry_run
    {
        bail!(
            "--store, --list, --program, --db and --dry-run do not belong to `stores updates`\n\n{}",
            stores_usage()
        );
    }

    let options = stores_options(parsed);
    let (found, notes) = collect_updates(parsed, &options);

    if parsed.json {
        println!("{}", serde_json::to_string_pretty(&found)?);
        for note in &notes {
            eprintln!("note: {note}");
        }
        return Ok(());
    }

    if found.is_empty() {
        println!("No updates available.");
    } else {
        for update in &found {
            let version = update
                .version
                .as_deref()
                .map(|version| format!(" {version}"))
                .unwrap_or_default();
            println!(
                "{:<32}  {:<8}{:<18}  {}",
                update.title,
                update.id.store.as_str(),
                version,
                update.source.label()
            );
        }
        println!("\n{} update(s) available.", found.len());
    }
    for note in &notes {
        eprintln!("note: {note}");
    }
    Ok(())
}

/// Gathers the update signals into one list, with the notes to report beside
/// it (a store that could not be checked, an absent Steam root).
fn collect_updates(
    parsed: &StoresArgs,
    options: &CliOptions,
) -> (Vec<UpdateAvailable>, Vec<String>) {
    let mut games = Vec::new();
    let mut phases = HashMap::new();
    let mut notes = Vec::new();

    match parsed.steam_root.clone().or_else(discover_steam_root) {
        Some(root) => {
            let report = steam::scan_steam(&root, false);
            notes.extend(report.warnings.iter().cloned());
            phases = report.phases;
            games.extend(report.games);
        }
        None => notes.push("no Steam library found on this machine".to_string()),
    }

    if parsed.with_stores {
        collect_store_installs(options, &mut games, &mut notes);
    } else {
        notes.push(
            "CLI-store installs were not checked; pass --with-stores to include them".to_string(),
        );
    }

    (status::updates(&games, &phases), notes)
}

/// Runs each ready, signed-in store's install listing and adds its games.
///
/// Only Epic and Amazon can list what is installed; GOG's library CLI has no
/// such listing, so it contributes no rows (and therefore no updates).
fn collect_store_installs(options: &CliOptions, games: &mut Vec<Game>, notes: &mut Vec<String>) {
    let epic = Epic::new();
    let epic_status = StoreStatusReport::epic(&epic, options, false);
    if epic_status.can_install() {
        match epic.installed(options) {
            Ok(installed) => games.extend(installed),
            Err(error) => notes.push(format!("epic: {error}")),
        }
    } else if epic_status.is_ready() {
        notes.push("epic: not signed in; skipped".to_string());
    }

    let amazon = Amazon::new();
    let amazon_status = StoreStatusReport::amazon(&amazon, options, false);
    if amazon_status.can_install() {
        match amazon.installed(options) {
            Ok(installed) => games.extend(installed),
            Err(error) => notes.push(format!("amazon: {error}")),
        }
    } else if amazon_status.is_ready() {
        notes.push("amazon: not signed in; skipped".to_string());
    }

    let gog_status = StoreStatusReport::gog(&Gog::new(), options, false);
    if gog_status.is_ready() {
        notes.push("gog: no install listing, so no update signal".to_string());
    }
}

/// The library sync: fold each CLI store's two listings into cached rows.
///
/// `stores` probes; `stores sync` writes. For each store it reads the rows the
/// cache already holds, merges the fresh listing over them
/// ([`pitagames_stores::sync::merge`]) and replaces that store's rows in SQLite.
/// Other stores are untouched, so a Steam scan and an Epic sync share a table
/// without either deleting the other's rows.
///
/// A store whose listing failed is reported and skipped, never written: a CLI
/// that logs an error and exits `0` (legendary, nile) would otherwise look like
/// an empty library and delete every row the store owns.
fn stores_sync(parsed: &StoresArgs) -> Result<()> {
    if parsed.list || parsed.with_stores || parsed.steam_root.is_some() {
        bail!(
            "--list, --with-stores and --steam-root do not belong to `stores sync`\n\n{}",
            stores_usage()
        );
    }

    let filters = store_filters(&parsed.filters)?;
    if (parsed.program.is_some() || parsed.game_program.is_some()) && filters.len() != 1 {
        bail!(
            "--program and --game-program need exactly one --store\n\n{}",
            stores_usage()
        );
    }

    let stores: Vec<Store> = status::CLI_STORES
        .iter()
        .copied()
        .filter(|store| filters.is_empty() || filters.contains(store))
        .collect();
    let db_path = match &parsed.db {
        Some(path) => path.clone(),
        None => paths::data_dir()
            .map(|dir| dir.join("pitagames.db"))
            .context("no data directory (HOME is unset); pass --db <path>")?,
    };

    let options = stores_options(parsed);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start the async runtime")?;
    let reports = runtime
        .block_on(sync_stores(
            &db_path,
            &stores,
            &sync_backends(parsed),
            &options,
            parsed.dry_run,
        ))
        .with_context(|| format!("could not sync into {}", db_path.display()))?;

    if parsed.json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
        return Ok(());
    }

    for report in &reports {
        print_sync_row(report);
    }
    if parsed.dry_run {
        println!("\nDry run: nothing was written to {}.", db_path.display());
    }
    Ok(())
}

/// The three backends a sync drives, with any `--program` override applied.
fn sync_backends(parsed: &StoresArgs) -> Backends {
    let epic = match &parsed.program {
        Some(program) => Epic::with_program(program),
        None => Epic::new(),
    };
    let amazon = match &parsed.program {
        Some(program) => Amazon::with_program(program),
        None => Amazon::new(),
    };
    let gog = Gog::with_programs(
        parsed
            .program
            .clone()
            .unwrap_or_else(|| gog::PROGRAM.to_string()),
        parsed
            .game_program
            .clone()
            .unwrap_or_else(|| gog::GAME_PROGRAM.to_string()),
    );
    Backends { epic, amazon, gog }
}

/// Runs the listings, merges them over the cache and writes the result.
async fn sync_stores(
    db_path: &Path,
    stores: &[Store],
    backends: &Backends,
    options: &CliOptions,
    dry_run: bool,
) -> Result<Vec<SyncReport>> {
    let db = Db::open(db_path).await?;
    let mut reports = Vec::new();

    for store in stores {
        let library = backends.library(*store, options);
        let cached = db.store_games(*store).await?;
        let rows = sync::merge(&cached, &library);
        let applied = library.can_replace() && !dry_run;
        let removed = if applied {
            db.replace_store_games(*store, &rows).await?.removed
        } else {
            0
        };
        reports.push(SyncReport::new(&library, &rows, removed, applied));
    }

    Ok(reports)
}

fn print_sync_row(report: &SyncReport) {
    if report.applied {
        let removed = match report.removed {
            0 => String::new(),
            1 => "  (1 row removed)".to_string(),
            count => format!("  ({count} rows removed)"),
        };
        println!(
            "{:<7} synced    {} rows · {} installed · {} to install{}",
            report.store, report.rows, report.installed, report.owned_only, removed
        );
    } else {
        println!(
            "{:<7} skipped   (the listing was incomplete; the cache is unchanged)",
            report.store
        );
    }
    for warning in &report.warnings {
        println!("        warning: {warning}");
    }
}

fn store_report(store: Store, parsed: &StoresArgs, options: &CliOptions) -> StoreStatusReport {
    let list = parsed.list;
    match store {
        Store::Epic => {
            let backend = match &parsed.program {
                Some(program) => Epic::with_program(program),
                None => Epic::new(),
            };
            StoreStatusReport::epic(&backend, options, list)
        }
        Store::Amazon => {
            let backend = match &parsed.program {
                Some(program) => Amazon::with_program(program),
                None => Amazon::new(),
            };
            StoreStatusReport::amazon(&backend, options, list)
        }
        Store::Gog => {
            let library = parsed
                .program
                .clone()
                .unwrap_or_else(|| gog::PROGRAM.to_string());
            let game = parsed
                .game_program
                .clone()
                .unwrap_or_else(|| gog::GAME_PROGRAM.to_string());
            StoreStatusReport::gog(&Gog::with_programs(library, game), options, list)
        }
        other => unreachable!("{other} is not a CLI store"),
    }
}

fn print_store_row(report: &StoreStatusReport) {
    println!("{}: {}", report.store, store_state_label(report));
    for tool in &report.tools {
        match (tool.path(), &tool.version) {
            (Some(path), Some(version)) => {
                println!(
                    "    {:<16} {:<10} {}",
                    tool.program,
                    version,
                    path.display()
                );
            }
            (Some(path), None) => println!("    {:<16} {}", tool.program, path.display()),
            (None, _) => println!("    {:<16} not found in PATH", tool.program),
        }
    }
    match &report.session {
        SessionState::SignedIn { account } if account.trim().is_empty() => {
            println!("    signed in");
        }
        SessionState::SignedIn { account } => println!("    signed in as {account}"),
        SessionState::SignedOut => println!(
            "    not signed in — run `pitagames-cli {} auth`",
            report.store
        ),
        SessionState::Unknown => {}
    }
    match (report.owned, report.installed) {
        (Some(owned), Some(installed)) => println!("    {owned} owned · {installed} installed"),
        (Some(owned), None) => println!("    {owned} owned"),
        (None, Some(installed)) => println!("    {installed} installed"),
        (None, None) => {}
    }
    for problem in &report.problems {
        println!("    problem: {problem}");
    }
}

fn store_state_label(report: &StoreStatusReport) -> &'static str {
    match report.state() {
        RowState::Ready => "ready",
        RowState::CliMissing => "not installed",
        RowState::SignedOut => "sign in required",
        RowState::Unverified => "status unknown",
    }
}

fn stores_options(parsed: &StoresArgs) -> CliOptions {
    let log_dir = parsed
        .log_dir
        .clone()
        .or_else(pitagames_core::paths::logs_dir);
    CliOptions {
        log_dir,
        ..CliOptions::default()
    }
}

/// Parses the `--store` filters into the three CLI stores.
fn store_filters(values: &[String]) -> Result<Vec<Store>> {
    let mut stores = Vec::new();
    for value in values {
        let store: Store = value.parse().map_err(|_| {
            anyhow::anyhow!("unknown store `{value}`; expected epic, amazon or gog")
        })?;
        if !status::CLI_STORES.contains(&store) {
            bail!("`{value}` has no CLI store backend (expected epic, amazon or gog)");
        }
        if !stores.contains(&store) {
            stores.push(store);
        }
    }
    Ok(stores)
}

/// Parsed `stores` arguments.
#[derive(Debug, Default, PartialEq, Eq)]
struct StoresArgs {
    /// `status` (default) or `updates`.
    subcommand: Option<String>,
    /// `--store` filters (status only).
    filters: Vec<String>,
    list: bool,
    with_stores: bool,
    program: Option<String>,
    game_program: Option<String>,
    log_dir: Option<PathBuf>,
    steam_root: Option<PathBuf>,
    db: Option<PathBuf>,
    dry_run: bool,
    json: bool,
    help: bool,
}

fn parse_stores_args(args: &[String]) -> Result<StoresArgs> {
    let mut parsed = StoresArgs::default();
    let mut index = 0;

    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--json" => parsed.json = true,
            "--list" => parsed.list = true,
            "--dry-run" => parsed.dry_run = true,
            "--with-stores" => parsed.with_stores = true,
            "-h" | "--help" => parsed.help = true,
            "--store" => parsed
                .filters
                .push(flag_value(args, &mut index, "--store")?),
            "--program" => parsed.program = Some(flag_value(args, &mut index, "--program")?),
            "--game-program" => {
                parsed.game_program = Some(flag_value(args, &mut index, "--game-program")?)
            }
            "--log-dir" => {
                parsed.log_dir = Some(PathBuf::from(flag_value(args, &mut index, "--log-dir")?))
            }
            "--steam-root" => {
                parsed.steam_root =
                    Some(PathBuf::from(flag_value(args, &mut index, "--steam-root")?))
            }
            "--db" => parsed.db = Some(PathBuf::from(flag_value(args, &mut index, "--db")?)),
            _ if arg.starts_with("--store=") => parsed.filters.push(after_eq(arg).to_string()),
            _ if arg.starts_with("--program=") => parsed.program = Some(after_eq(arg).to_string()),
            _ if arg.starts_with("--game-program=") => {
                parsed.game_program = Some(after_eq(arg).to_string())
            }
            _ if arg.starts_with("--log-dir=") => {
                parsed.log_dir = Some(PathBuf::from(after_eq(arg)))
            }
            _ if arg.starts_with("--steam-root=") => {
                parsed.steam_root = Some(PathBuf::from(after_eq(arg)))
            }
            _ if arg.starts_with("--db=") => parsed.db = Some(PathBuf::from(after_eq(arg))),
            other if !other.starts_with('-') => {
                if parsed.subcommand.is_some() {
                    bail!(
                        "unexpected argument `{other}` for `stores`\n\n{}",
                        stores_usage()
                    );
                }
                parsed.subcommand = Some(other.to_string());
            }
            other => bail!(
                "unexpected argument `{other}` for `stores`\n\n{}",
                stores_usage()
            ),
        }
        index += 1;
    }

    Ok(parsed)
}

/// The value after a `--flag` token, bumping the index past it.
fn flag_value(args: &[String], index: &mut usize, name: &str) -> Result<String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .with_context(|| format!("{name} requires a value"))
}

/// The value half of a `--flag=value` token.
fn after_eq(arg: &str) -> &str {
    arg.split_once('=').map(|(_, value)| value).unwrap_or("")
}

fn store_options(parsed: &StoreArgs, interactive: bool) -> CliOptions {
    let log_dir = parsed
        .log_dir
        .clone()
        .or_else(pitagames_core::paths::logs_dir);
    if interactive {
        CliOptions {
            log_dir,
            ..CliOptions::interactive()
        }
    } else {
        CliOptions {
            log_dir,
            ..CliOptions::default()
        }
    }
}

fn positional<'a>(parsed: &'a StoreArgs, index: usize, what: &str) -> Result<&'a str> {
    parsed
        .positional
        .get(index)
        .map(String::as_str)
        .with_context(|| format!("missing {what}"))
}

fn expect_positionals(parsed: &StoreArgs, expected: usize) -> Result<()> {
    if parsed.positional.len() > expected {
        bail!(
            "unexpected argument `{}` for `{}`\n\n{}",
            parsed.positional[expected],
            parsed.store.label(),
            parsed.store.usage()
        );
    }
    Ok(())
}

/// Which store a shared argument parser is parsing for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoreCli {
    Epic,
    Amazon,
    Gog,
}

impl StoreCli {
    fn label(self) -> &'static str {
        match self {
            Self::Epic => "epic",
            Self::Amazon => "amazon",
            Self::Gog => "gog",
        }
    }

    fn program(self) -> &'static str {
        match self {
            Self::Epic => epic::PROGRAM,
            Self::Amazon => amazon::PROGRAM,
            // GOG's default `--program` is the library CLI; `--game-program`
            // overrides the installer/launcher.
            Self::Gog => gog::PROGRAM,
        }
    }

    fn usage(self) -> String {
        match self {
            Self::Epic => epic_usage(),
            Self::Amazon => amazon_usage(),
            Self::Gog => gog_usage(),
        }
    }
}

/// Parsed store-CLI (`epic` / `amazon` / `gog`) arguments.
#[derive(Debug, PartialEq, Eq)]
struct StoreArgs {
    store: StoreCli,
    program: String,
    /// The `gogdl` program, when `gog` is parsed (`--game-program`).
    game_program: Option<String>,
    /// The `gogdl` token file, when `gog` is parsed (`--auth-file`).
    auth_file: Option<PathBuf>,
    log_dir: Option<PathBuf>,
    dir: Option<PathBuf>,
    json: bool,
    dry_run: bool,
    positional: Vec<String>,
    help: bool,
}

fn parse_store_args(args: &[String], store: StoreCli) -> Result<StoreArgs> {
    let mut parsed = StoreArgs {
        store,
        program: store.program().to_string(),
        game_program: None,
        auth_file: None,
        log_dir: None,
        dir: None,
        json: false,
        dry_run: false,
        positional: Vec::new(),
        help: false,
    };

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--json" => parsed.json = true,
            "--dry-run" => parsed.dry_run = true,
            "--program" => {
                index += 1;
                let value = args
                    .get(index)
                    .with_context(|| "--program requires a path")?;
                parsed.program = value.clone();
            }
            "--game-program" => {
                index += 1;
                let value = args
                    .get(index)
                    .with_context(|| "--game-program requires a path")?;
                parsed.game_program = Some(value.clone());
            }
            "--auth-file" => {
                index += 1;
                let value = args
                    .get(index)
                    .with_context(|| "--auth-file requires a path")?;
                parsed.auth_file = Some(PathBuf::from(value));
            }
            "--log-dir" => {
                index += 1;
                let value = args
                    .get(index)
                    .with_context(|| "--log-dir requires a path")?;
                parsed.log_dir = Some(PathBuf::from(value));
            }
            "--dir" => {
                index += 1;
                let value = args.get(index).with_context(|| "--dir requires a path")?;
                parsed.dir = Some(PathBuf::from(value));
            }
            "-h" | "--help" => parsed.help = true,
            _ if arg.starts_with("--program=") => {
                parsed.program = arg["--program=".len()..].to_string();
            }
            _ if arg.starts_with("--game-program=") => {
                parsed.game_program = Some(arg["--game-program=".len()..].to_string());
            }
            _ if arg.starts_with("--auth-file=") => {
                parsed.auth_file = Some(PathBuf::from(&arg["--auth-file=".len()..]));
            }
            _ if arg.starts_with("--log-dir=") => {
                parsed.log_dir = Some(PathBuf::from(&arg["--log-dir=".len()..]));
            }
            _ if arg.starts_with("--dir=") => {
                parsed.dir = Some(PathBuf::from(&arg["--dir=".len()..]));
            }
            other if other.starts_with('-') => bail!(
                "unexpected argument `{other}` for `{}`\n\n{}",
                store.label(),
                store.usage()
            ),
            other => parsed.positional.push(other.to_string()),
        }
        index += 1;
    }

    Ok(parsed)
}

/// Read or write one app's Steam launch options.
///
/// This is the diagnostic surface for the highest-risk write path: it prints
/// the file it resolved, the Steam process state and the exact value, and it
/// refuses to write while Steam is running unless `--force` is passed. The GUI
/// calls the same `stores::steam_config` functions, so what this command shows
/// is what the GUI will do.
fn launch_options(args: &[String]) -> Result<()> {
    let parsed = parse_launch_options_args(args)?;
    if parsed.help {
        println!("{}", launch_options_usage());
        return Ok(());
    }

    let target = resolve_localconfig(&parsed)?;
    let backup_dir = match &parsed.backup_dir {
        Some(dir) => dir.clone(),
        None => pitagames_core::paths::data_dir()
            .map(|dir| dir.join("backups").join("steam"))
            .context("cannot determine the XDG data dir; pass --backup-dir <path>")?,
    };
    let mut write = WriteOptions::new(backup_dir);
    write.allow_running = parsed.force;

    let steam = match target.steam_root.as_deref() {
        Some(root) => steam_config::detect_steam_process(root),
        None => SteamProcessState::Unknown,
    };

    let action = parsed.action.clone().unwrap_or(LaunchAction::Read);
    match &action {
        LaunchAction::Read => {
            let current = steam_config::read_launch_options(&target.localconfig, &target.app_id)?;
            print_launch_options(&target, &steam, current.as_deref(), parsed.json);
            Ok(())
        }
        LaunchAction::Restore => {
            if parsed.dry_run {
                bail!("--dry-run applies to --set/--shim/--clear, not --restore");
            }
            let report = steam_config::restore_launch_options(
                &target.localconfig,
                &target.app_id,
                steam,
                &write,
            )?;
            print_restore(&target, &report, parsed.json);
            Ok(())
        }
        LaunchAction::Set(_) | LaunchAction::Shim | LaunchAction::Clear => {
            let current = steam_config::read_launch_options(&target.localconfig, &target.app_id)?;
            let next = planned_value(&action, &target.app_id, current.as_deref());

            if parsed.dry_run {
                print_launch_options(&target, &steam, next.as_deref(), parsed.json);
                eprintln!("dry run: no files were written");
                return Ok(());
            }

            let report = steam_config::apply_launch_options(
                &target.localconfig,
                &target.app_id,
                next.as_deref(),
                steam,
                &write,
            )?;
            print_write(&target, &steam, &report, parsed.json);
            Ok(())
        }
    }
}

fn planned_value(action: &LaunchAction, app_id: &str, current: Option<&str>) -> Option<String> {
    match action {
        LaunchAction::Read | LaunchAction::Restore => current.map(str::to_string),
        LaunchAction::Set(value) => Some(value.clone()),
        LaunchAction::Shim => Some(shim_launch_options(app_id, current.unwrap_or_default())),
        LaunchAction::Clear => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LaunchAction {
    Read,
    Set(String),
    Shim,
    Clear,
    Restore,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct LaunchOptionsArgs {
    app_id: Option<String>,
    steam_root: Option<PathBuf>,
    localconfig: Option<PathBuf>,
    user: Option<String>,
    backup_dir: Option<PathBuf>,
    action: Option<LaunchAction>,
    dry_run: bool,
    force: bool,
    json: bool,
    help: bool,
}

fn parse_launch_options_args(args: &[String]) -> Result<LaunchOptionsArgs> {
    let mut parsed = LaunchOptionsArgs::default();
    let mut index = 0;

    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--json" => parsed.json = true,
            "--dry-run" => parsed.dry_run = true,
            "--force" => parsed.force = true,
            "--shim" => set_action(&mut parsed, LaunchAction::Shim, arg)?,
            "--clear" => set_action(&mut parsed, LaunchAction::Clear, arg)?,
            "--restore" => set_action(&mut parsed, LaunchAction::Restore, arg)?,
            "--set" | "--steam-root" | "--localconfig" | "--user" | "--backup-dir" => {
                index += 1;
                let value = args
                    .get(index)
                    .with_context(|| format!("{arg} requires a value"))?;
                if value.is_empty() && arg != "--set" {
                    bail!("{arg} requires a value");
                }
                apply_flag_value(&mut parsed, arg, value)?;
            }
            "-h" | "--help" => parsed.help = true,
            _ if arg.starts_with("--set=")
                || arg.starts_with("--steam-root=")
                || arg.starts_with("--localconfig=")
                || arg.starts_with("--user=")
                || arg.starts_with("--backup-dir=") =>
            {
                let (flag, value) = arg
                    .split_once('=')
                    .expect("the guard above requires an `=`");
                if value.is_empty() {
                    bail!("{flag} requires a value");
                }
                apply_flag_value(&mut parsed, flag, value)?;
            }
            other if !other.starts_with('-') => {
                if parsed.app_id.is_some() {
                    bail!(
                        "unexpected argument `{other}` for `launch-options`\n\n{}",
                        launch_options_usage()
                    );
                }
                parsed.app_id = Some(other.to_string());
            }
            other => bail!(
                "unexpected argument `{other}` for `launch-options`\n\n{}",
                launch_options_usage()
            ),
        }
        index += 1;
    }

    Ok(parsed)
}

fn set_action(args: &mut LaunchOptionsArgs, action: LaunchAction, flag: &str) -> Result<()> {
    if args.action.is_some() {
        bail!("{flag} conflicts with the other action flags (--set/--shim/--clear/--restore)");
    }
    args.action = Some(action);
    Ok(())
}

fn apply_flag_value(args: &mut LaunchOptionsArgs, flag: &str, value: &str) -> Result<()> {
    match flag {
        "--set" => {
            set_action(args, LaunchAction::Set(value.to_string()), flag)?;
        }
        "--steam-root" => args.steam_root = Some(PathBuf::from(value)),
        "--localconfig" => args.localconfig = Some(PathBuf::from(value)),
        "--user" => args.user = Some(value.to_string()),
        "--backup-dir" => args.backup_dir = Some(PathBuf::from(value)),
        other => bail!("internal error: unknown flag `{other}`"),
    }
    Ok(())
}

/// The `localconfig.vdf` to edit and the Steam root it belongs to (when known).
#[derive(Debug, Clone, PartialEq, Eq)]
struct LaunchTarget {
    localconfig: PathBuf,
    steam_root: Option<PathBuf>,
    user_id: String,
    app_id: String,
}

fn resolve_localconfig(args: &LaunchOptionsArgs) -> Result<LaunchTarget> {
    let app_id = args
        .app_id
        .clone()
        .context("missing Steam appid\n\nsee `pitagames-cli launch-options --help`")?;
    if app_id.is_empty() || !app_id.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("`{app_id}` is not a numeric Steam appid");
    }

    let (localconfig, steam_root) = match &args.localconfig {
        Some(path) => {
            let path = path.clone();
            let root = derive_steam_root(&path);
            (path, root)
        }
        None => {
            let root = match &args.steam_root {
                Some(root) => root.clone(),
                None => discover_steam_root()
                    .context("no Steam root found; pass --steam-root or --localconfig")?,
            };
            let userdata = root.join("userdata");
            let user = match &args.user {
                Some(user) => user.clone(),
                None => pick_user(&userdata)?,
            };
            (
                userdata.join(&user).join("config").join("localconfig.vdf"),
                Some(root),
            )
        }
    };

    if !localconfig.is_file() {
        bail!(
            "{} does not exist; is this the right Steam user? pass --user or --localconfig",
            localconfig.display()
        );
    }

    let user_id = localconfig
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .to_string();

    Ok(LaunchTarget {
        localconfig,
        steam_root,
        user_id,
        app_id,
    })
}

/// `<root>/userdata/<id>/config/localconfig.vdf` → `<root>`.
fn derive_steam_root(localconfig: &Path) -> Option<PathBuf> {
    let mut current = localconfig;
    while let Some(parent) = current.parent() {
        if parent.file_name().is_some_and(|name| name == "userdata") {
            return parent.parent().map(Path::to_path_buf);
        }
        current = parent;
    }
    None
}

/// Pick the only Steam user with a config, or fail and name the candidates.
fn pick_user(userdata: &Path) -> Result<String> {
    let mut users: Vec<String> = std::fs::read_dir(userdata)
        .with_context(|| format!("cannot read {}", userdata.display()))?
        .flatten()
        .filter(|entry| entry.path().join("config/localconfig.vdf").is_file())
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .collect();
    users.sort();

    match users.as_slice() {
        [] => bail!(
            "no Steam user with a localconfig.vdf under {}; sign in to Steam once",
            userdata.display()
        ),
        [only] => Ok(only.clone()),
        many => {
            eprintln!(
                "warning: {} Steam users found ({}); using {} — pass --user to choose",
                many.len(),
                many.join(", "),
                many[0]
            );
            Ok(many[0].clone())
        }
    }
}

fn print_launch_options(
    target: &LaunchTarget,
    steam: &SteamProcessState,
    value: Option<&str>,
    json: bool,
) {
    if json {
        let payload = serde_json::json!({
            "localconfig": target.localconfig,
            "steam_root": target.steam_root,
            "user_id": target.user_id,
            "app_id": target.app_id,
            "launch_options": value,
            "steam": steam.describe(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).expect("a JSON value always serializes")
        );
        return;
    }

    println!("Steam root:   {}", display_root(target));
    println!("User:         {}", target.user_id);
    println!("App:          {}", target.app_id);
    println!("Steam:        {}", steam.describe());
    match value {
        Some(value) => println!("Launch options: {value}"),
        None => println!("Launch options: (not set)"),
    }
}

fn print_write(target: &LaunchTarget, steam: &SteamProcessState, report: &WriteReport, json: bool) {
    if json {
        let payload = serde_json::json!({
            "localconfig": report.localconfig,
            "steam_root": target.steam_root,
            "user_id": report.user_id,
            "app_id": report.app_id,
            "previous": report.previous,
            "applied": report.applied,
            "changed": report.changed,
            "backup": report.backup,
            "steam": steam.describe(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).expect("a JSON value always serializes")
        );
        return;
    }

    if !report.changed {
        println!(
            "Launch options already set to {}",
            display_value(report.applied.as_deref())
        );
        return;
    }

    println!("Updated {}", report.localconfig.display());
    println!("User:         {}", report.user_id);
    println!("App:          {}", report.app_id);
    println!(
        "Launch options: {} -> {}",
        display_value(report.previous.as_deref()),
        display_value(report.applied.as_deref())
    );
    if let Some(backup) = &report.backup {
        println!("Backup:       {}", backup.display());
    }
    println!(
        "Steam:        {} (quit it before the next Steam start if it was running)",
        steam.describe()
    );
}

fn print_restore(target: &LaunchTarget, report: &steam_config::RestoreReport, json: bool) {
    if json {
        let payload = serde_json::json!({
            "localconfig": report.localconfig,
            "steam_root": target.steam_root,
            "user_id": report.user_id,
            "app_id": report.app_id,
            "restored": report.restored,
            "exact": report.exact,
            "backup": report.backup,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).expect("a JSON value always serializes")
        );
        return;
    }

    println!("Restored {}", report.localconfig.display());
    println!("User:         {}", report.user_id);
    println!("App:          {}", report.app_id);
    println!(
        "Launch options: {}",
        display_value(report.restored.as_deref())
    );
    println!(
        "Mode:         {}",
        if report.exact {
            "byte-identical file restore"
        } else {
            "merged (Steam changed other keys; only this app's options were put back)"
        }
    );
    println!("Backup:       {}", report.backup.display());
}

fn display_root(target: &LaunchTarget) -> String {
    target
        .steam_root
        .as_deref()
        .map(|root| root.display().to_string())
        .unwrap_or_else(|| "(derived from --localconfig)".to_string())
}

fn display_value(value: Option<&str>) -> String {
    match value {
        Some(value) => format!("`{value}`"),
        None => "(not set)".to_string(),
    }
}

// ---------------------------------------------------------------------------
// compat-tool
// ---------------------------------------------------------------------------

/// Read or write one app's Steam compatibility-tool mapping (`config.vdf`).
///
/// This is the diagnostic surface for the "force a Proton build" write: it
/// prints the file it resolved, the Steam process state and the exact mapping,
/// and refuses to write while Steam is running unless `--force` is passed. The
/// GUI calls the same `stores::steam_config` functions.
fn compat_tool(args: &[String]) -> Result<()> {
    let parsed = parse_compat_tool_args(args)?;
    if parsed.help {
        println!("{}", compat_tool_usage());
        return Ok(());
    }

    let target = resolve_compat_config(&parsed)?;
    let backup_dir = match &parsed.backup_dir {
        Some(dir) => dir.clone(),
        None => pitagames_core::paths::data_dir()
            .map(|dir| dir.join("backups").join("steam"))
            .context("cannot determine the XDG data dir; pass --backup-dir <path>")?,
    };
    let mut write = WriteOptions::new(backup_dir);
    write.allow_running = parsed.force;

    let steam = match target.steam_root.as_deref() {
        Some(root) => steam_config::detect_steam_process(root),
        None => SteamProcessState::Unknown,
    };

    let action = parsed.action.clone().unwrap_or(CompatAction::Read);
    match &action {
        CompatAction::Read => {
            let current = steam_config::read_compat_tool(&target.config, &target.app_id)?;
            print_compat(&target, &steam, current.as_ref(), parsed.json);
            Ok(())
        }
        CompatAction::Restore => {
            if parsed.dry_run {
                bail!("--dry-run applies to --set/--clear, not --restore");
            }
            let report =
                steam_config::restore_compat_tool(&target.config, &target.app_id, steam, &write)?;
            print_compat_restore(&target, &report, parsed.json);
            Ok(())
        }
        CompatAction::Set(_) | CompatAction::Clear => {
            let next = planned_compat(&action, parsed.priority());
            if parsed.dry_run {
                print_compat(&target, &steam, next.as_ref(), parsed.json);
                eprintln!("dry run: no files were written");
                return Ok(());
            }
            let report = steam_config::apply_compat_tool(
                &target.config,
                &target.app_id,
                next.as_ref(),
                steam,
                &write,
            )?;
            print_compat_write(&target, &steam, &report, parsed.json);
            Ok(())
        }
    }
}

/// The mapping `--set`/`--clear` would write.
fn planned_compat(action: &CompatAction, priority: i32) -> Option<CompatTool> {
    match action {
        CompatAction::Set(name) => Some(CompatTool {
            name: name.clone(),
            config: String::new(),
            priority,
        }),
        CompatAction::Clear | CompatAction::Read | CompatAction::Restore => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CompatAction {
    Read,
    Set(String),
    Clear,
    Restore,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CompatToolArgs {
    app_id: Option<String>,
    steam_root: Option<PathBuf>,
    config: Option<PathBuf>,
    backup_dir: Option<PathBuf>,
    action: Option<CompatAction>,
    priority: Option<i32>,
    dry_run: bool,
    force: bool,
    json: bool,
    help: bool,
}

impl CompatToolArgs {
    /// Steam's own default when it forces a tool for one game.
    fn priority(&self) -> i32 {
        self.priority.unwrap_or(DEFAULT_PRIORITY)
    }
}

fn parse_compat_tool_args(args: &[String]) -> Result<CompatToolArgs> {
    let mut parsed = CompatToolArgs::default();
    let mut index = 0;

    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--json" => parsed.json = true,
            "--dry-run" => parsed.dry_run = true,
            "--force" => parsed.force = true,
            "--clear" => set_compat_action(&mut parsed, CompatAction::Clear, arg)?,
            "--restore" => set_compat_action(&mut parsed, CompatAction::Restore, arg)?,
            "--set" | "--steam-root" | "--config" | "--backup-dir" | "--priority" => {
                index += 1;
                let value = args
                    .get(index)
                    .with_context(|| format!("{arg} requires a value"))?;
                if value.is_empty() && arg != "--set" {
                    bail!("{arg} requires a value");
                }
                apply_compat_flag(&mut parsed, arg, value)?;
            }
            "-h" | "--help" => parsed.help = true,
            _ if arg.starts_with("--set=")
                || arg.starts_with("--steam-root=")
                || arg.starts_with("--config=")
                || arg.starts_with("--backup-dir=")
                || arg.starts_with("--priority=") =>
            {
                let (flag, value) = arg
                    .split_once('=')
                    .expect("the guard above requires an `=`");
                if value.is_empty() {
                    bail!("{flag} requires a value");
                }
                apply_compat_flag(&mut parsed, flag, value)?;
            }
            other if !other.starts_with('-') => {
                if parsed.app_id.is_some() {
                    bail!(
                        "unexpected argument `{other}` for `compat-tool`\n\n{}",
                        compat_tool_usage()
                    );
                }
                parsed.app_id = Some(other.to_string());
            }
            other => bail!(
                "unexpected argument `{other}` for `compat-tool`\n\n{}",
                compat_tool_usage()
            ),
        }
        index += 1;
    }

    Ok(parsed)
}

fn set_compat_action(args: &mut CompatToolArgs, action: CompatAction, flag: &str) -> Result<()> {
    if args.action.is_some() {
        bail!("{flag} conflicts with the other action flags (--set/--clear/--restore)");
    }
    args.action = Some(action);
    Ok(())
}

fn apply_compat_flag(args: &mut CompatToolArgs, flag: &str, value: &str) -> Result<()> {
    match flag {
        "--set" => set_compat_action(args, CompatAction::Set(value.to_string()), flag)?,
        "--steam-root" => args.steam_root = Some(PathBuf::from(value)),
        "--config" => args.config = Some(PathBuf::from(value)),
        "--backup-dir" => args.backup_dir = Some(PathBuf::from(value)),
        "--priority" => {
            let priority = value
                .parse::<i32>()
                .with_context(|| format!("`{value}` is not a priority number"))?;
            if priority < 0 {
                bail!("--priority must not be negative");
            }
            args.priority = Some(priority);
        }
        other => bail!("internal error: unknown flag `{other}`"),
    }
    Ok(())
}

/// The `config.vdf` to edit and the Steam root it belongs to (when known).
#[derive(Debug, Clone, PartialEq, Eq)]
struct CompatToolTarget {
    config: PathBuf,
    steam_root: Option<PathBuf>,
    app_id: String,
}

fn resolve_compat_config(args: &CompatToolArgs) -> Result<CompatToolTarget> {
    let app_id = args
        .app_id
        .clone()
        .context("missing Steam appid\n\nsee `pitagames-cli compat-tool --help`")?;
    if app_id.is_empty() || !app_id.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("`{app_id}` is not a numeric Steam appid");
    }

    let (config, steam_root) = match &args.config {
        Some(path) => {
            let path = path.clone();
            let root = derive_steam_root_from_config(&path);
            (path, root)
        }
        None => {
            let root = match &args.steam_root {
                Some(root) => root.clone(),
                None => discover_steam_root()
                    .context("no Steam root found; pass --steam-root or --config")?,
            };
            (root.join("config").join("config.vdf"), Some(root))
        }
    };

    if !config.is_file() {
        bail!(
            "{} does not exist; pass --config or a Steam root with a config.vdf",
            config.display()
        );
    }

    Ok(CompatToolTarget {
        config,
        steam_root,
        app_id,
    })
}

/// `<root>/config/config.vdf` → `<root>`.
fn derive_steam_root_from_config(config: &Path) -> Option<PathBuf> {
    let parent = config.parent()?;
    if parent.file_name().is_some_and(|name| name == "config") {
        return parent.parent().map(Path::to_path_buf);
    }
    None
}

fn print_compat(
    target: &CompatToolTarget,
    steam: &SteamProcessState,
    mapping: Option<&CompatTool>,
    json: bool,
) {
    if json {
        let payload = serde_json::json!({
            "config": target.config,
            "steam_root": target.steam_root,
            "app_id": target.app_id,
            "compat_tool": mapping,
            "steam": steam.describe(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).expect("a JSON value always serializes")
        );
        return;
    }

    println!("Steam root:   {}", display_compat_root(target));
    println!("App:          {}", target.app_id);
    println!("Steam:        {}", steam.describe());
    match mapping {
        Some(tool) => println!("Compat tool:  {} (priority {})", tool.name, tool.priority),
        None => println!("Compat tool:  (Steam decides)"),
    }
}

fn print_compat_write(
    target: &CompatToolTarget,
    steam: &SteamProcessState,
    report: &CompatWriteReport,
    json: bool,
) {
    if json {
        let payload = serde_json::json!({
            "config": report.config,
            "steam_root": target.steam_root,
            "app_id": report.app_id,
            "previous": report.previous,
            "applied": report.applied,
            "changed": report.changed,
            "backup": report.backup,
            "steam": steam.describe(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).expect("a JSON value always serializes")
        );
        return;
    }

    if !report.changed {
        println!(
            "Compat tool already set to {}",
            display_compat(report.applied.as_ref())
        );
        return;
    }

    println!("Updated {}", report.config.display());
    println!("App:          {}", report.app_id);
    println!(
        "Compat tool:  {} -> {}",
        display_compat(report.previous.as_ref()),
        display_compat(report.applied.as_ref())
    );
    if let Some(backup) = &report.backup {
        println!("Backup:       {}", backup.display());
    }
    println!(
        "Steam:        {} (quit it before the next Steam start if it was running)",
        steam.describe()
    );
}

fn print_compat_restore(target: &CompatToolTarget, report: &CompatRestoreReport, json: bool) {
    if json {
        let payload = serde_json::json!({
            "config": report.config,
            "steam_root": target.steam_root,
            "app_id": report.app_id,
            "restored": report.restored,
            "exact": report.exact,
            "backup": report.backup,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).expect("a JSON value always serializes")
        );
        return;
    }

    println!("Restored {}", report.config.display());
    println!("App:          {}", report.app_id);
    println!("Compat tool:  {}", display_compat(report.restored.as_ref()));
    println!(
        "Mode:         {}",
        if report.exact {
            "byte-identical file restore"
        } else {
            "merged (Steam changed other keys; only this app's mapping was put back)"
        }
    );
    println!("Backup:       {}", report.backup.display());
}

fn display_compat_root(target: &CompatToolTarget) -> String {
    target
        .steam_root
        .as_deref()
        .map(|root| root.display().to_string())
        .unwrap_or_else(|| "(derived from --config)".to_string())
}

fn display_compat(tool: Option<&CompatTool>) -> String {
    match tool {
        Some(tool) => format!("`{}`", tool.name),
        None => "(Steam decides)".to_string(),
    }
}

// ---------------------------------------------------------------------------
// compat
// ---------------------------------------------------------------------------

/// Read the shipped compatibility database.
///
/// With no arguments, list every entry. With `<store> <id>`, print one entry in
/// detail. `--shipped`/`--user` point at fixture directories instead of the
/// installed database, so the loader and its merge rules can be exercised
/// without a package install.
fn compat(args: &[String]) -> Result<()> {
    let parsed = parse_compat_args(args)?;
    if parsed.help {
        println!("{}", compat_usage());
        return Ok(());
    }

    let shipped = if parsed.shipped.is_empty() {
        pitagames_platform::compat::default_shipped_dirs()
    } else {
        parsed.shipped.clone()
    };
    let user = if parsed.user.is_empty() {
        pitagames_platform::compat::default_user_dirs()
    } else {
        parsed.user.clone()
    };

    let db = CompatDb::load_from(&shipped, &user);
    for warning in db.warnings() {
        eprintln!("warning: {warning}");
    }

    let identity = match parsed.positional.as_slice() {
        [] => None,
        [store, id] => Some(GameId::new(
            Store::from_str(store).map_err(|error| anyhow::anyhow!("{error}"))?,
            id.clone(),
        )),
        [_] => bail!(
            "expected `<store> <id>` or no positional arguments\n\n{}",
            compat_usage()
        ),
        _ => bail!("too many positional arguments\n\n{}", compat_usage()),
    };

    match identity {
        Some(id) => {
            let entry = db.get(&id).with_context(|| {
                format!(
                    "no compat entry for {}:{} in {} file(s)",
                    id.store.as_str(),
                    id.store_id,
                    db.files().len()
                )
            })?;
            if parsed.json {
                let payload = serde_json::json!({ "entry": entry, "warnings": db.warnings() });
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                for line in compat_detail_lines(entry) {
                    println!("{line}");
                }
            }
        }
        None => {
            if parsed.json {
                let entries: Vec<&CompatEntry> = db.entries().collect();
                let payload = serde_json::json!({ "entries": entries, "warnings": db.warnings() });
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                for line in compat_list_lines(&db) {
                    println!("{line}");
                }
            }
        }
    }

    Ok(())
}

/// Parsed `compat` arguments.
#[derive(Debug, Default, PartialEq, Eq)]
struct CompatArgs {
    positional: Vec<String>,
    shipped: Vec<PathBuf>,
    user: Vec<PathBuf>,
    json: bool,
    help: bool,
}

fn parse_compat_args(args: &[String]) -> Result<CompatArgs> {
    let mut parsed = CompatArgs::default();
    let mut index = 0;

    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--json" => parsed.json = true,
            "--shipped" | "--user" => {
                index += 1;
                let value = args
                    .get(index)
                    .with_context(|| format!("{arg} requires a path"))?;
                append_compat_dir(&mut parsed, arg, value)?;
            }
            "-h" | "--help" => parsed.help = true,
            _ if arg.starts_with("--shipped=") || arg.starts_with("--user=") => {
                let (flag, value) = arg
                    .split_once('=')
                    .expect("the guard above requires an `=`");
                append_compat_dir(&mut parsed, flag, value)?;
            }
            other if !other.starts_with('-') => parsed.positional.push(other.to_string()),
            other => bail!(
                "unexpected argument `{other}` for `compat`\n\n{}",
                compat_usage()
            ),
        }
        index += 1;
    }

    Ok(parsed)
}

fn append_compat_dir(parsed: &mut CompatArgs, flag: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{flag} requires a path");
    }
    match flag {
        "--shipped" => parsed.shipped.push(PathBuf::from(value)),
        "--user" => parsed.user.push(PathBuf::from(value)),
        other => bail!("internal error: unknown compat flag `{other}`"),
    }
    Ok(())
}

fn compat_list_lines(db: &CompatDb) -> Vec<String> {
    let mut lines = vec![format!(
        "Compat DB: {} entries from {} file(s)",
        db.len(),
        db.files().len()
    )];
    if db.is_empty() {
        lines.push("No entries loaded.".to_string());
        lines.push(
            "Pass --shipped <dir> to point at a checkout, or install the package data.".to_string(),
        );
        return lines;
    }

    let width = db
        .entries()
        .map(|entry| entry.title.chars().count())
        .max()
        .unwrap_or(0)
        .max(24);

    for entry in db.entries() {
        let verified = if entry.verified.is_empty() {
            "unverified".to_string()
        } else {
            entry
                .verified
                .iter()
                .map(|arch| arch.as_str())
                .collect::<Vec<_>>()
                .join(",")
        };
        let supported = if entry.upscaling.supported.is_empty() {
            "-".to_string()
        } else {
            entry
                .upscaling
                .supported
                .iter()
                .map(|api| api.as_str())
                .collect::<Vec<_>>()
                .join(",")
        };
        let rdna4 = entry
            .recommended(pitagames_platform::compat::Arch::Rdna4)
            .map(|id| id.as_str())
            .unwrap_or("-");
        lines.push(format!(
            "  {:<16} {:<width$}  {:<10}  {:<16}  {rdna4}",
            format!("{}:{}", entry.id.store.as_str(), entry.id.store_id),
            fit(&entry.title, width),
            verified,
            supported,
            width = width,
        ));
    }
    lines
}

fn compat_detail_lines(entry: &CompatEntry) -> Vec<String> {
    let mut lines = vec![format!(
        "{}  [{}:{}]",
        entry.title,
        entry.id.store.as_str(),
        entry.id.store_id
    )];

    lines.push(format!("Anti-cheat:   {}", entry.anticheat.as_str()));
    if let Some(engine) = &entry.engine {
        lines.push(format!("Engine:       {engine}"));
    }
    let verified = if entry.verified.is_empty() {
        "not tested by us".to_string()
    } else {
        entry
            .verified
            .iter()
            .map(|arch| arch.label())
            .collect::<Vec<_>>()
            .join(", ")
    };
    lines.push(format!("Verified:     {verified}"));

    lines.push(String::new());
    lines.push("Upscaling:".to_string());
    let supported = if entry.upscaling.supported.is_empty() {
        "(none listed)".to_string()
    } else {
        entry
            .upscaling
            .supported
            .iter()
            .map(|api| api.label())
            .collect::<Vec<_>>()
            .join(", ")
    };
    lines.push(format!("  supported:     {supported}"));
    if !entry.upscaling.recommended.is_empty() {
        lines.push("  recommended:".to_string());
        for (arch, id) in &entry.upscaling.recommended {
            lines.push(format!("    {} -> {}", arch.label(), id.as_str()));
        }
    }
    if let Some(dll) = entry.upscaling.preferred_dll {
        lines.push(format!("  preferred dll: {}", dll.file_name()));
    }
    if let Some(helixsr) = &entry.upscaling.helixsr {
        let modes = helixsr
            .modes
            .iter()
            .map(|mode| mode.label())
            .collect::<Vec<_>>()
            .join(", ");
        match helixsr.preferred {
            Some(preferred) => lines.push(format!(
                "  helixsr:       {modes} (preferred {})",
                preferred.label()
            )),
            None => lines.push(format!("  helixsr:       {modes}")),
        }
    }
    if let Some(notes) = &entry.upscaling.notes {
        lines.push(format!("  notes:         {notes}"));
    }

    if !entry.runtime.wrappers.is_empty()
        || !entry.runtime.env.is_empty()
        || !entry.runtime.known_issues.is_empty()
    {
        lines.push(String::new());
        lines.push("Runtime:".to_string());
        if !entry.runtime.wrappers.is_empty() {
            let wrappers = entry
                .runtime
                .wrappers
                .iter()
                .map(|wrapper| wrapper.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!("  wrappers:      {wrappers}"));
        }
        for (key, value) in &entry.runtime.env {
            lines.push(format!("  env:           {key}={value}"));
        }
        for issue in &entry.runtime.known_issues {
            lines.push(format!("  known issue:   {issue}"));
        }
    }

    if let Some(neural) = &entry.neural_rendering {
        lines.push(String::new());
        lines.push("Neural rendering:".to_string());
        lines.push(format!("  backend:       {}", neural.backend.label()));
        lines.push(format!("  route:         {}", neural.route.describe()));
        if let Some(mesa) = neural.min_mesa {
            lines.push(format!("  min mesa:      {mesa}"));
        }
        if let Some(version) = &neural.dlssnr_min_version {
            lines.push(format!("  dlssnr:        {version}"));
        }
        for (arch, scale) in &neural.model_scale_default {
            lines.push(format!("  model scale:   {} = {scale}%", arch.label()));
        }
        lines.push(format!(
            "  requires 64-bit: {}",
            if neural.requires_64bit { "yes" } else { "no" }
        ));
        if let Some(notes) = &neural.notes {
            lines.push(format!("  notes:         {notes}"));
        }
    }

    lines
}

// ---------------------------------------------------------------------------
// anticheat
// ---------------------------------------------------------------------------

/// Detect a game's anti-cheat runtime and show the injection gate.
///
/// Merges the markers found in the installed game directory with the compat
/// DB's curated value, then reports what the injection gate would decide for a
/// profile that asks for injection. Read-only: it never injects and never
/// writes the database.
fn anticheat(args: &[String]) -> Result<()> {
    let parsed = parse_anticheat_args(args)?;
    if parsed.help {
        println!("{}", anticheat_usage());
        return Ok(());
    }

    let app_id = parsed
        .app_id
        .clone()
        .with_context(|| format!("missing <appid>\n\n{}", anticheat_usage()))?;
    if app_id.parse::<u32>().is_err() {
        bail!("`{app_id}` is not a numeric Steam appid");
    }
    let id = GameId::new(Store::Steam, app_id.clone());

    // Resolve the install directory: `--dir` wins, else find it in the scan.
    let install_dir = match parsed.dir.clone() {
        Some(dir) => Some(dir),
        None => {
            let root = match parsed.steam_root.clone() {
                Some(root) => root,
                None => discover_steam_root().context(
                    "no Steam library found; pass --steam-root <path> or --dir <game dir>",
                )?,
            };
            let report = steam::scan_steam(&root, false);
            for warning in &report.warnings {
                eprintln!("warning: {warning}");
            }
            report
                .games
                .into_iter()
                .find(|game| game.id.store_id == app_id)
                .and_then(|game| game.install_dir)
        }
    };

    let scan = install_dir
        .as_deref()
        .map(pitagames_platform::anticheat::detect)
        .unwrap_or_default();

    let shipped = if parsed.shipped.is_empty() {
        pitagames_platform::compat::default_shipped_dirs()
    } else {
        parsed.shipped.clone()
    };
    let user = if parsed.user.is_empty() {
        pitagames_platform::compat::default_user_dirs()
    } else {
        parsed.user.clone()
    };
    let db = CompatDb::load_from(&shipped, &user);
    for warning in db.warnings() {
        eprintln!("warning: {warning}");
    }

    let curated = db
        .get(&id)
        .map(|entry| entry.anticheat)
        .unwrap_or(AntiCheat::Unknown);
    let merged = pitagames_platform::anticheat::effective(curated, &scan);
    // What the graphics UI will face: injection requested, not yet acknowledged.
    let decision = pitagames_platform::anticheat::gate(merged.kind, true, false);

    if parsed.json {
        let payload = serde_json::json!({
            "game": id,
            "install_dir": install_dir,
            "scan": scan,
            "curated": curated,
            "effective": merged,
            "gate": decision,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        for line in anticheat_lines(
            &app_id,
            install_dir.as_deref(),
            &scan,
            curated,
            merged,
            decision,
        ) {
            println!("{line}");
        }
    }

    Ok(())
}

fn anticheat_lines(
    app_id: &str,
    install_dir: Option<&Path>,
    scan: &AnticheatScan,
    curated: AntiCheat,
    merged: EffectiveAnticheat,
    decision: GateDecision,
) -> Vec<String> {
    let mut lines = vec![format!("Anti-cheat for steam:{app_id}")];

    match install_dir {
        Some(dir) => lines.push(format!("  install dir: {}", dir.display())),
        None => lines.push("  install dir: (not installed here)".to_string()),
    }

    lines.push(format!("  detected:    {}", detected_label(scan)));
    for finding in &scan.findings {
        lines.push(format!(
            "      {}  ({})",
            finding.path.display(),
            finding.kind.as_str()
        ));
    }

    lines.push(format!("  curated:     {}", curated.as_str()));
    let effective = match merged.source {
        AnticheatSource::Undetermined => merged.kind.as_str().to_string(),
        source => format!("{} ({})", merged.kind.as_str(), source_label(source)),
    };
    lines.push(format!("  effective:   {effective}"));

    match decision.confirmation() {
        Some(anticheat) => {
            lines.push(format!(
                "  gate:        confirmation required before {} injection",
                anticheat.as_str()
            ));
            if let Some(message) = decision.message() {
                lines.push(format!("      {message}"));
            }
        }
        None => {
            lines.push("  gate:        injection allowed (no anti-cheat hard gate)".to_string())
        }
    }

    lines
}

/// A short description of what a scan found, distinguishing "not scanned" from
/// "scanned and clean".
fn detected_label(scan: &AnticheatScan) -> String {
    if !scan.scanned {
        return "unknown (not scanned)".to_string();
    }

    let mut label = if scan.kind == AntiCheat::Unknown {
        "none (no markers found)".to_string()
    } else {
        format!(
            "{} ({} marker{})",
            scan.kind.as_str(),
            scan.findings.len(),
            if scan.findings.len() == 1 { "" } else { "s" }
        )
    };
    if scan.truncated {
        label.push_str(", walk truncated");
    }
    label
}

fn source_label(source: AnticheatSource) -> &'static str {
    match source {
        AnticheatSource::Detected => "detected",
        AnticheatSource::Curated => "compat DB",
        AnticheatSource::Undetermined => "unknown",
    }
}

/// Parsed `anticheat` arguments.
#[derive(Debug, Default, PartialEq, Eq)]
struct AnticheatArgs {
    app_id: Option<String>,
    dir: Option<PathBuf>,
    steam_root: Option<PathBuf>,
    shipped: Vec<PathBuf>,
    user: Vec<PathBuf>,
    json: bool,
    help: bool,
}

fn parse_anticheat_args(args: &[String]) -> Result<AnticheatArgs> {
    let mut parsed = AnticheatArgs::default();
    let mut index = 0;

    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--json" => parsed.json = true,
            "--dir" | "--steam-root" | "--shipped" | "--user" => {
                index += 1;
                let value = args
                    .get(index)
                    .with_context(|| format!("{arg} requires a path"))?;
                apply_anticheat_flag(&mut parsed, arg, value)?;
            }
            "-h" | "--help" => parsed.help = true,
            _ if arg.starts_with("--dir=")
                || arg.starts_with("--steam-root=")
                || arg.starts_with("--shipped=")
                || arg.starts_with("--user=") =>
            {
                let (flag, value) = arg
                    .split_once('=')
                    .expect("the guard above requires an `=`");
                apply_anticheat_flag(&mut parsed, flag, value)?;
            }
            other if !other.starts_with('-') => {
                if parsed.app_id.is_some() {
                    bail!("expected a single <appid>\n\n{}", anticheat_usage());
                }
                parsed.app_id = Some(other.to_string());
            }
            other => bail!(
                "unexpected argument `{other}` for `anticheat`\n\n{}",
                anticheat_usage()
            ),
        }
        index += 1;
    }

    Ok(parsed)
}

fn apply_anticheat_flag(parsed: &mut AnticheatArgs, flag: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{flag} requires a path");
    }
    match flag {
        "--dir" => parsed.dir = Some(PathBuf::from(value)),
        "--steam-root" => parsed.steam_root = Some(PathBuf::from(value)),
        "--shipped" => parsed.shipped.push(PathBuf::from(value)),
        "--user" => parsed.user.push(PathBuf::from(value)),
        other => bail!("internal error: unknown anticheat flag `{other}`"),
    }
    Ok(())
}

fn anticheat_usage() -> String {
    "USAGE:\n    pitagames-cli anticheat <appid> [options]\n\n\
     ARGS:\n    \
     <appid>                Numeric Steam appid\n\n\
     OPTIONS:\n    \
     --dir <path>           Scan this install directory directly (skips the appmanifest scan)\n    \
     --steam-root <path>    Steam root to find the install dir (default: auto-detect)\n    \
     --shipped <dir>        Read shipped compat entries from this directory (repeatable)\n    \
     --user <dir>           Read user compat overrides from this directory (repeatable)\n    \
     --json                 Print the report as JSON\n    \
     -h, --help             Show this help\n\n\
     Merges the anti-cheat markers found in the installed game directory with the\n\
     compat DB's curated value, then shows what the injection gate would decide for a\n\
     profile that asks for injection. Read-only: it never injects and never writes."
        .to_string()
}

fn compat_usage() -> String {
    "USAGE:\n    pitagames-cli compat [<store> <id>] [--shipped <dir>]... [--user <dir>]... [--json]\n\n\
     ARGS:\n    \
     <store> <id>           Show one entry, e.g. `compat steam 1091500`\n\n\
     OPTIONS:\n    \
     --shipped <dir>        Read shipped entries from this directory (repeatable)\n    \
     --user <dir>           Read user overrides from this directory (repeatable)\n    \
     --json                 Print entries as JSON\n    \
     -h, --help             Show this help\n\n\
     With no arguments every entry is listed. Without --shipped/--user the default\n\
     locations are used: /usr/share/pitagames/compat, a copy under the data\n\
     directory, then the user overrides in $XDG_CONFIG_HOME/pitagames/compat.\n\
     Set PITAGAMES_COMPAT_DIR to replace the shipped directories."
        .to_string()
}

/// Run a `LaunchSpec` without a shell.
fn spawn(spec: &LaunchSpec) -> std::io::Result<()> {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    command.envs(spec.env.iter().map(|(key, value)| (key, value)));
    // Steam's client detaches on its own; we never wait for it.
    command.spawn().map(|_| ())
}

fn parse_timeout(value: &str) -> Result<Duration> {
    let seconds = value
        .parse::<u64>()
        .with_context(|| format!("`{value}` is not a number of seconds"))?;
    if seconds == 0 {
        bail!("--timeout must be at least 1 second");
    }
    Ok(Duration::from_secs(seconds))
}

fn describe(sample: &InstallSample) -> String {
    match sample {
        InstallSample::WaitingForManifest => "no appmanifest yet".to_string(),
        InstallSample::InProgress(progress) => progress.phase.as_str().to_string(),
        InstallSample::Installed { .. } => "installed".to_string(),
        InstallSample::TimedOut => "timed out".to_string(),
    }
}

/// Prints a poll line only when it changes, so a 500 ms poll stays readable.
#[derive(Debug, Default)]
struct SamplePrinter {
    last: Option<String>,
}

impl SamplePrinter {
    fn render(&mut self, sample: &InstallSample) -> Option<String> {
        // The caller prints the terminal "installed" line itself (with the
        // manifest path), so the printer only reports in-flight states.
        if matches!(sample, InstallSample::Installed { .. }) {
            return None;
        }

        let (key, line) = match sample {
            InstallSample::Installed { .. } => return None,
            InstallSample::WaitingForManifest => (
                "waiting".to_string(),
                "  waiting for Steam to create the appmanifest…".to_string(),
            ),
            InstallSample::InProgress(progress) => {
                // Bucket percentages to 5% so a long download does not flood
                // the log.
                let bucket = progress
                    .progress
                    .and_then(DownloadProgress::percent)
                    .map_or(-1, |percent| i32::from(percent / 5));
                let key = format!("{}/{bucket}", progress.phase.as_str());
                let line = match progress.progress {
                    Some(bytes) => format!(
                        "  {:<16} {:>3}%  {} / {}",
                        progress.phase.as_str(),
                        bytes.percent().unwrap_or(0),
                        human_bytes(bytes.downloaded),
                        human_bytes(bytes.total)
                    ),
                    None => format!("  {}", progress.phase.as_str()),
                };
                (key, line)
            }
            InstallSample::TimedOut => ("timed out".to_string(), "  timed out".to_string()),
        };

        if self.last.as_deref() == Some(key.as_str()) {
            return None;
        }
        self.last = Some(key);
        Some(line)
    }
}

fn print_table(root: &Path, report: &ScanReport, elapsed: Duration) {
    println!("Steam root: {}", root.display());
    println!(
        "Found {} game{} in {:.1} ms",
        report.games.len(),
        if report.games.len() == 1 { "" } else { "s" },
        elapsed.as_secs_f64() * 1000.0
    );

    if report.games.is_empty() {
        println!();
        println!("No installed Steam games found under {}", root.display());
        println!("Check the path, or pass --include-tools to see runtimes.");
        return;
    }

    println!();
    println!(
        "{:<44} {:>7} {:>10} {:<10} STATE",
        "TITLE", "APPID", "SIZE", "UPDATED"
    );

    for game in &report.games {
        println!(
            "{:<44} {:>7} {:>10} {:<10} {}",
            fit(&game.title, 44),
            game.id.store_id,
            size(game),
            updated(game),
            if game.installed {
                "installed"
            } else {
                "incomplete"
            }
        );
    }
}

fn fit(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }

    let mut truncated: String = value.chars().take(width.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

fn size(game: &Game) -> String {
    game.size_bytes
        .map(human_bytes)
        .unwrap_or_else(|| "-".to_string())
}

fn updated(game: &Game) -> String {
    game.last_updated
        .map(|timestamp| timestamp.date().to_string())
        .unwrap_or_else(|| "-".to_string())
}

/// A compact base-1024 byte count, shared with `platform::disk`.
fn human_bytes(bytes: u64) -> String {
    disk::human_bytes(bytes)
}

fn usage() -> String {
    format!(
        "pitagames-cli — developer CLI for PitaGames\n\n{}\n\n{}",
        "USAGE:\n    pitagames-cli <command> [options]\n    pitagames-cli --version",
        "COMMANDS:\n    scan       Scan installed Steam games\n    install    Ask Steam to install a game, then poll the appmanifest\n    metadata   Fetch Steam store metadata for appids (live, uncached)\n    proton     List the Proton builds and the auto-selected runtime\n    compat     List or validate the shipped compatibility database\n    anticheat  Detect a game's anti-cheat and show the injection gate\n    profile    Show the compiled shim profile for one Steam appid\n    logs       List or tail the PitaGames log directory\n    import     List the games Heroic, Lutris and Bottles have installed\n    prefix     Show one non-Steam game's Wine/Proton prefix and umu env\n    disk       Report free space and check it against an install size\n    stores     Show each CLI store's status, updates, or sync its library\n    desktop-entry  Print, install or remove the launcher's .desktop entry\n    activate   Ask a running PitaGames to come to the front\n    launch-options  Read, set, shim or restore one game's Steam launch options\n    compat-tool  Read, force, clear or restore one game's compatibility tool\n    steam-account  Read or manage the Steam Web API key and the owned-games list\n    \
    epic       Epic Games Store through legendary (status, list, install, verify, launch)\n    \
    amazon     Amazon Games through nile (status, list, installed, sync, install, verify, launch)\n    \
    gog        GOG through lgogdownloader + gogdl (status, list, auth, install, verify, launch)\n\n\
         Run `pitagames-cli <command> --help` for command options."
    )
}

fn proton_usage() -> String {
    "USAGE:\n    pitagames-cli proton [--root <dir>]... [--steam-root <dir>]... [--json]\n\n\
     OPTIONS:\n    \
     --root <dir>           Scan a compatibility-tools directory (repeatable)\n    \
     --steam-root <dir>     Scan <dir>/compatibilitytools.d and <dir>/steamapps/common\n    \
     --json                 Print the report as JSON\n    \
     -h, --help             Show this help\n\n\
     Without --root/--steam-root the default locations are scanned:\n\
     system compatibilitytools.d, every Steam root, and umu's data directories."
        .to_string()
}

fn profile_usage() -> String {
    "USAGE:\n    pitagames-cli profile <appid> [--dir <path>] [--json]\n\n\
     ARGS:\n    \
     <appid>                Numeric Steam appid\n\n\
     OPTIONS:\n    \
     --dir <path>           Profiles directory (default: $XDG_DATA_HOME/pitagames/profiles)\n    \
     --json                 Print the profile as JSON\n    \
     -h, --help             Show this help\n\n\
     The GUI writes profiles/steam-<appid>.json on every profile save; the shim\n\
     reads it at launch. This command shows what is stored."
        .to_string()
}

fn logs_usage() -> String {
    "USAGE:\n    pitagames-cli logs [--dir <path>] [--file <name>] [--tail <n>] [--grep <text>] [--json]\n\n\
     OPTIONS:\n    \
     --dir <path>           Log directory (default: $XDG_DATA_HOME/pitagames/logs)\n    \
     --file <name|path>     Tail one file instead of listing the directory\n    \
     --tail <n>             Lines to keep from the end (default: 200)\n    \
     --grep <text>          Only show lines containing this text\n    \
     --json                 Print the listing or tail as JSON\n    \
     -h, --help             Show this help\n\n\
     The GUI's Logs screen reads the same files through platform::logs. shim.log\n\
     records one launch line per game; PROTON_LOG writes steam-<appid>.log; DXVK\n\
     logging writes <exe>_d3d11.log and friends. Read-only: nothing is edited or\n\
     deleted."
        .to_string()
}

fn import_usage() -> String {
    "USAGE:\n    pitagames-cli import [--heroic <dir>]... [--lutris <dir>]... [--bottles <dir>]... [--json]\n\n\
     OPTIONS:\n    \
     --heroic <dir>         A Heroic config folder (repeatable)\n    \
     --lutris <dir>         A Lutris data folder, holding pga.db (repeatable)\n    \
     --bottles <dir>        A Bottles folder, holding library.yml (repeatable)\n    \
     --json                 Print the report as JSON\n    \
     -h, --help             Show this help\n\n\
     Without the flags the default locations are read, native and Flatpak:\n\
     ~/.config/heroic, ~/.var/app/com.heroicgameslauncher.hgl/config/heroic,\n\
     ~/.local/share/lutris, ~/.var/app/net.lutris.Lutris/data/lutris,\n\
     ~/.local/share/bottles and ~/.var/app/com.usebottles.bottles/data/bottles.\n\n\
     Installed-only and read-only: the games another launcher already has are\n\
     listed with the identity their store uses, so a scan does not duplicate\n\
     them. Nothing is launched, downloaded or written."
        .to_string()
}

fn prefix_usage() -> String {
    "USAGE:\n    pitagames-cli prefix <store> <id> [options]\n\n\
     ARGS:\n    \
     <store>                One of steam, gog, epic, amazon, local\n    \
     <id>                   The store's own id (app name, product id, appid)\n\n\
     OPTIONS:\n    \
     --root <dir>           Prefix root (default: $PITAGAMES_PREFIX_DIR or\n\
     \x20                      $XDG_DATA_HOME/pitagames/prefixes)\n    \
     --proton <path>        Proton build for the umu environment (default: auto-detect)\n    \
     --json                 Print the report as JSON\n    \
     -h, --help             Show this help\n\n\
     Non-Steam games get one Wine/Proton prefix each, under\n\
     <root>/<store>-<id>; Steam keeps its own inside the Steam library. This is\n\
     read-only: it prints the path, whether Proton has created the prefix, its\n\
     size and the GAMEID/PROTONPATH/WINEPREFIX that umu-run will need."
        .to_string()
}

fn stores_usage() -> String {
    "USAGE:\n    pitagames-cli stores [status] [--store <name>] [--list] [--json]\n\
     \x20                 pitagames-cli stores updates [--steam-root <dir>] [--with-stores] [--json]\n\
     \x20                 pitagames-cli stores sync [--store <name>] [--db <path>] [--dry-run] [--json]\n\n\
     SUBCOMMANDS:\n    \
     status                 Each CLI store's row (default)\n    \
     updates                The unified 'update available' list\n    \
     sync                   Cache each CLI store's library in the database\n\n\
     OPTIONS:\n    \
     --store <name>         Limit to epic, amazon or gog (repeatable; status, sync)\n    \
     --list                 Also fetch owned/installed counts (status only)\n    \
     --program <path>       Override the CLI to probe (one --store; GOG: the library CLI)\n    \
     --game-program <path>  Override GOG's gogdl program\n    \
     --log-dir <dir>        Where per-run CLI logs go\n    \
     --steam-root <dir>     Steam root to scan for updates (updates only)\n    \
     --with-stores          Include the CLI stores' installs in the update list\n    \
     --db <path>            Database to write (sync only; default: the XDG one)\n    \
     --dry-run              Merge and report, but do not write (sync only)\n    \
     --json                 Print the result as JSON\n    \
     -h, --help             Show this help\n\n\
     A status row shows whether each store's CLI is on PATH, its version, the\n\
     signed-in account and (with --list) the game counts. A CLI that is missing or\n\
     signed out is a row state, not an error: the launcher keeps working for the\n\
     stores that are ready. updates unifies Steam's manifest state with each CLI\n\
     store's own install flag; Amazon and GOG expose no update signal yet.\n\n\
     sync folds each store's owned and installed listings into one round of game\n\
     rows and replaces that store's rows in the database. A listing that failed is\n\
     reported and leaves the cache untouched — an empty answer is not an empty\n\
     library — and other stores' rows are never deleted."
        .to_string()
}

fn disk_usage() -> String {
    "USAGE:\n    pitagames-cli disk <path> [--need <size>] [--json]\n\n\
     ARGS:\n    \
     <path>                 A file or directory (its filesystem is measured)\n\n\
     OPTIONS:\n    \
     --need <size>          Also check for this much room, e.g. 20G, 1.5GiB, 512M\n    \
     --json                 Print the result as JSON\n    \
     -h, --help             Show this help\n\n\
     Reports the filesystem's total, free and user-available bytes; --need prints\n\
     whether an install of that size would fit. A path that does not exist yet is\n\
     fine: the filesystem it would land on is what matters."
        .to_string()
}

fn desktop_entry_usage() -> String {
    "USAGE:\n    pitagames-cli desktop-entry [options]\n\n\
     ACTIONS (default: show the installed state):\n    \
     --print                Print the entry to stdout (no file is written)\n    \
     --write                Install it into the applications directory\n    \
     --remove               Delete the user-level entry\n\n\
     OPTIONS:\n    \
     --exec <path>          Exec target; repeat for arguments (default: pitagames)\n    \
     --path <file>          Entry file (default: $XDG_DATA_HOME/applications/pitagames.desktop)\n    \
     --json                 Print the result as JSON\n    \
     -h, --help             Show this help\n\n\
     The GUI regenerates the same entry on every start unless a packaged one is\n\
     installed in $XDG_DATA_DIRS. Regenerate the packaged file with:\n\
     \x20 pitagames-cli desktop-entry --print > packaging/pitagames.desktop"
        .to_string()
}

fn activate_usage() -> String {
    "USAGE:\n    pitagames-cli activate [--socket <path>] [--json]\n\n\
     OPTIONS:\n    \
     --socket <path>        Activation socket (default: $XDG_DATA_HOME/pitagames/instance.sock)\n    \
     --json                 Print the result as JSON\n    \
     -h, --help             Show this help\n\n\
     Installing a second launcher asks the running one to come to the front and\n\
     exits; this command sends the same request by hand. It fails when no\n\
     launcher is listening, which is the normal state when PitaGames is closed."
        .to_string()
}

fn launch_options_usage() -> String {
    "USAGE:\n    pitagames-cli launch-options <appid> [options]\n\n\
     ARGS:\n    \
     <appid>                Numeric Steam appid\n\n\
     TARGET:\n    \
     --steam-root <path>    Steam root (default: auto-detect)\n    \
     --user <id>            Steam user id under userdata/ (default: the only one with a config)\n    \
     --localconfig <path>   Edit this localconfig.vdf directly (skips --steam-root/--user)\n\n\
     ACTIONS (default: read):\n    \
     --set <options>        Set the launch options string verbatim\n    \
     --shim                 Put `pitagames-shim <appid>` in front of %command% (idempotent)\n    \
     --clear                Remove the launch options key\n    \
     --restore              Put the original value back from PitaGames' backup\n\n\
     OPTIONS:\n    \
     --backup-dir <path>    Where backups and the restore record live\n    \
     --dry-run              Print the planned value without writing\n    \
     --force                Write even if Steam looks running (Linux only)\n    \
     --json                 Print the result as JSON\n    \
     -h, --help             Show this help\n\n\
     The writer refuses while Steam is running, replaces the file atomically and\n\
     keeps a timestamped backup. --restore copies the backup back byte-for-byte\n\
     when the file is unchanged, or merges only this app's options when Steam\n\
     changed something else in the meantime."
        .to_string()
}

fn compat_tool_usage() -> String {
    "USAGE:\n    pitagames-cli compat-tool <appid> [options]\n\n\
     ARGS:\n    \
     <appid>                Numeric Steam appid\n\n\
     TARGET:\n    \
     --steam-root <path>    Steam root (default: auto-detect); reads <root>/config/config.vdf\n    \
     --config <path>        Edit this config.vdf directly (skips --steam-root)\n\n\
     ACTIONS (default: read):\n    \
     --set <name>           Force this compatibility tool, e.g. proton-cachyos, GE-Proton9-20\n    \
     --clear                Remove the mapping so Steam decides again\n    \
     --restore              Put the original mapping back from PitaGames' backup\n\n\
     OPTIONS:\n    \
     --priority <n>         Mapping priority for --set (default: 250)\n    \
     --backup-dir <path>    Where backups and the restore record live\n    \
     --dry-run              Print the planned mapping without writing\n    \
     --force                Write even if Steam looks running (Linux only)\n    \
     --json                 Print the result as JSON\n    \
     -h, --help             Show this help\n\n\
     This writes Steam's per-game CompatToolMapping, the same setting as Steam's\n\
     Libraries > Properties > Compatibility screen. Same guard rails as\n\
     launch-options: refuses while Steam runs, atomic replace, timestamped backup\n\
     and a byte-identical restore when Steam has not changed the file since."
        .to_string()
}

fn steam_account_usage() -> String {
    "USAGE:\n    pitagames-cli steam-account [options]\n\n\
     OPTIONS:\n    \
     --steam-root <path>    Steam root to read userdata from (default: auto-detect)\n    \
     --set-key <key>        Validate and store a Steam Web API key; `-` reads stdin\n    \
     --forget-key           Remove the stored key from every store\n    \
     --refresh              Fetch and print the account's owned games (live)\n    \
     --limit <n>            Titles to print with --refresh (default: 25)\n    \
     --json                 Print the account state as JSON\n    \
     -h, --help             Show this help\n\n\
     The key is validated as 32 hex characters, stored in the desktop keyring when\n\
     one is available (a 0600 file under the config directory otherwise) and read\n\
     back in the order environment, keyring, file. It is only ever sent to\n\
     api.steampowered.com. Create one at steamcommunity.com/dev/apikey."
        .to_string()
}

fn epic_usage() -> String {
    "USAGE:\n    pitagames-cli epic <subcommand> [options]\n\n\
     SUBCOMMANDS:\n    \
     status                 CLI path, version and account (offline)\n    \
     list                   Owned games (legendary list --json)\n    \
     installed              Installed games (legendary list-installed --json)\n    \
     auth                   Sign in with the Epic code-paste flow\n    \
     logout                 Forget the stored Epic session\n    \
     install <app>          Install a game, streaming legendary's progress\n    \
     verify <app>           Verify an installed game's files\n    \
     launch <app>           Launch a game through legendary\n\n\
     OPTIONS:\n    \
     --program <path>       legendary binary to drive (default: legendary from PATH)\n    \
     --dir <path>           Install directory (default: $XDG_DATA_HOME/pitagames/games/epic/<app>)\n    \
     --log-dir <path>       Per-run transcripts (default: $XDG_DATA_HOME/pitagames/logs)\n    \
     --dry-run              Print the planned argv instead of running it\n    \
     --json                 Print status/lists/verify as JSON\n    \
     -h, --help             Show this help\n\n\
     legendary is the Epic CLI PitaGames wraps; without it the store shows as\n\
     unavailable and read-only imports are the fallback (M2). `install` passes\n\
     --base-path/--game-folder from --dir, --yes and --disable-sdl so no prompt\n\
     needs a human. `verify` reads legendary's summary line, because it exits 0\n\
     even when files are corrupt or missing. A PTY echoes typed input, so an auth\n\
     transcript can contain the code: it stays local and is never printed."
        .to_string()
}

fn amazon_usage() -> String {
    "USAGE:\n    pitagames-cli amazon <subcommand> [options]\n\n\
     SUBCOMMANDS:\n    \
     status                 CLI path, version and session (offline)\n    \
     list                   Owned games (nile library list --json)\n    \
     installed              Installed games (nile library list --installed)\n    \
     sync                   Refresh nile's cached library from Amazon\n    \
     auth                   Sign in through the browser and a pasted URL\n    \
     logout                 Forget the stored Amazon session\n    \
     install <id>           Install a game, streaming nile's progress\n    \
     verify <id>            Repair an installed game (nile re-downloads)\n    \
     launch <id>            Launch a game through nile\n\n\
     OPTIONS:\n    \
     --program <path>       nile binary to drive (default: nile from PATH)\n    \
     --dir <path>           Install directory (default: $XDG_DATA_HOME/pitagames/games/amazon/<game>)\n    \
     --log-dir <path>       Per-run transcripts (default: $XDG_DATA_HOME/pitagames/logs)\n    \
     --dry-run              Print the planned argv instead of running it\n    \
     --json                 Print status/lists/verify as JSON\n    \
     -h, --help             Show this help\n\n\
     nile is the Amazon CLI PitaGames wraps; without it the store shows as\n\
     unavailable and read-only imports are the fallback (M2). nile exits 0 for\n\
     almost every failure, so verdicts come from its own log lines: `install`\n\
     passes --path so PitaGames' directory is authoritative, `verify` is a repair\n\
     (it re-downloads missing files), and `launch` needs nile's `Launching` marker\n\
     because a refusal also exits 0. The login is a browser sign-in (Amazon's 2FA\n\
     happens there) followed by pasting the redirected amazon.com URL; a PTY echoes\n\
     it, so the transcript stays local and is never printed."
        .to_string()
}

fn gog_usage() -> String {
    "USAGE:\n    pitagames-cli gog <subcommand> [options]\n\n\
     SUBCOMMANDS:\n    \
     status                 Both CLIs' paths, version and login status\n    \
     list                   Owned games (lgogdownloader --list json)\n    \
     auth                   Sign in through the browser and a pasted URL\n    \
     install <id>           Install a game with gogdl, streaming progress\n    \
     verify <id>            Repair an installed game (gogdl repair) — needs --dir\n    \
     launch <id>            Launch a game through gogdl — needs --dir\n\n\
     OPTIONS:\n    \
     --program <path>       lgogdownloader binary to drive (default: from PATH)\n    \
     --game-program <path>  gogdl binary to drive (default: gogdl from PATH)\n    \
     --auth-file <path>     gogdl's token file (default: $XDG_CONFIG_HOME/pitagames/gog/auth.json)\n    \
     --dir <path>           Install directory (default: $XDG_DATA_HOME/pitagames/games/gog/<game>)\n    \
     --log-dir <path>       Per-run transcripts (default: $XDG_DATA_HOME/pitagames/logs)\n    \
     --dry-run              Print the planned argv instead of running it\n    \
     --json                 Print status/lists/verify as JSON\n    \
     -h, --help             Show this help\n\n\
     GOG is driven by two CLIs because they do different jobs: lgogdownloader\n\
     lists the account's games with titles and signs in with a browser URL plus a\n\
     pasted redirect; gogdl installs a launchable game directory, repairs it and\n\
     launches it. Neither exposes a launch under lgogdownloader, and gogdl cannot\n\
     list the library, so install/verify/launch use gogdl and list/auth use\n\
     lgogdownloader. verify and launch need the game's directory (--dir), since\n\
     gogdl addresses a game by both id and path. lgogdownloader's --list json is\n\
     an account round-trip, so `install` looks the title up for the default path.\n\
     A PTY echoes typed input, so the auth transcript can contain the code: it\n\
     stays local and is never printed."
        .to_string()
}

fn metadata_usage() -> String {
    "USAGE:\n    pitagames-cli metadata <appid>... [--language <code>] [--json]\n\n\
     ARGS:\n    \
     <appid>...             Numeric Steam appids to fetch\n\n\
     OPTIONS:\n    \
     --language <code>     Steam language code (default: english)\n    \
     --json                Print the metadata as JSON\n    \
     -h, --help            Show this help"
        .to_string()
}

fn install_usage() -> String {
    "USAGE:\n    pitagames-cli install <appid> [--steam-root <path>] [--timeout <secs>] [--no-launch]\n\n\
     ARGS:\n    \
     <appid>                Numeric Steam appid to install or update\n\n\
     OPTIONS:\n    \
     --steam-root <path>    Steam root to watch (default: auto-detect)\n    \
     --timeout <secs>       Give up after this many seconds (default: 3600)\n    \
     --no-launch            Do not run `steam steam://install/<appid>`; only poll\n    \
     -h, --help             Show this help"
        .to_string()
}

fn scan_usage() -> String {
    "USAGE:\n    pitagames-cli scan [--steam-root <path>] [--json] [--include-tools]\n\n\
     OPTIONS:\n    \
     --steam-root <path>    Steam root to scan (default: auto-detect)\n    \
     --json                 Print games as JSON\n    \
     --include-tools        Include Proton and Steam Linux Runtime apps\n    \
     -h, --help             Show this help"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_stores::{InstallPhase, install::InstallProgress};

    #[test]
    fn parses_steam_account_actions_and_targets() {
        let parsed = parse_steam_account_args(&[
            "--steam-root=/steam".to_string(),
            "--set-key".to_string(),
            "-".to_string(),
            "--refresh".to_string(),
            "--limit".to_string(),
            "5".to_string(),
            "--json".to_string(),
        ])
        .unwrap();

        assert_eq!(parsed.steam_root, Some(PathBuf::from("/steam")));
        assert_eq!(parsed.set_key.as_deref(), Some("-"));
        assert!(parsed.refresh && parsed.json && !parsed.forget && !parsed.help);
        assert_eq!(parsed.limit, 5);

        let plain = parse_steam_account_args(&[]).unwrap();
        assert_eq!(
            plain,
            SteamAccountArgs {
                limit: 25,
                ..SteamAccountArgs::default()
            }
        );

        let forget =
            parse_steam_account_args(&["--forget-key".to_string(), "--help".to_string()]).unwrap();
        assert!(forget.forget && forget.help);

        let error = parse_steam_account_args(&["--limit".to_string(), "many".to_string()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a number"), "{error}");

        let error = parse_steam_account_args(&["extra".to_string()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("unexpected argument"), "{error}");
    }

    #[test]
    fn parses_epic_subcommands_and_shared_flags() {
        let parsed = parse_store_args(
            &[
                "--program=/opt/legendary".to_string(),
                "--dir".to_string(),
                "/games/epic".to_string(),
                "install".to_string(),
                "Gizmo".to_string(),
                "--json".to_string(),
                "--dry-run".to_string(),
            ],
            StoreCli::Epic,
        )
        .unwrap();

        assert_eq!(parsed.store, StoreCli::Epic);
        assert_eq!(parsed.program, "/opt/legendary");
        assert_eq!(parsed.dir, Some(PathBuf::from("/games/epic")));
        assert_eq!(parsed.positional, ["install", "Gizmo"]);
        assert!(parsed.json && parsed.dry_run && !parsed.help);

        let plain = parse_store_args(&[], StoreCli::Epic).unwrap();
        assert_eq!(plain.program, pitagames_stores::epic::PROGRAM);
        assert!(plain.positional.is_empty());

        let error = parse_store_args(&["--bogus".to_string()], StoreCli::Epic)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unexpected argument"), "{error}");
    }

    #[test]
    fn parses_amazon_subcommands_with_niles_own_defaults() {
        let parsed = parse_store_args(
            &[
                "--log-dir".to_string(),
                "/tmp/logs".to_string(),
                "install".to_string(),
                "amzn1.adg.product.1".to_string(),
            ],
            StoreCli::Amazon,
        )
        .unwrap();

        assert_eq!(parsed.store, StoreCli::Amazon);
        assert_eq!(parsed.program, pitagames_stores::amazon::PROGRAM);
        assert_eq!(parsed.log_dir, Some(PathBuf::from("/tmp/logs")));
        assert_eq!(parsed.positional, ["install", "amzn1.adg.product.1"]);

        let plain = parse_store_args(&[], StoreCli::Amazon).unwrap();
        assert_eq!(plain.program, "nile");

        // The usage text a parse error points at is the store's own.
        let error = parse_store_args(&["--bogus".to_string()], StoreCli::Amazon)
            .unwrap_err()
            .to_string();
        assert!(error.contains("for `amazon`"), "{error}");
        assert!(error.contains("nile library list --json"), "{error}");
    }

    #[test]
    fn parses_gog_subcommands_with_both_programs() {
        let parsed = parse_store_args(
            &[
                "--program".to_string(),
                "/opt/lgogdownloader".to_string(),
                "--game-program=/opt/gogdl".to_string(),
                "--auth-file".to_string(),
                "/tmp/auth.json".to_string(),
                "--dir=/games/gog".to_string(),
                "install".to_string(),
                "1207660623".to_string(),
                "--dry-run".to_string(),
            ],
            StoreCli::Gog,
        )
        .unwrap();

        assert_eq!(parsed.store, StoreCli::Gog);
        assert_eq!(parsed.program, "/opt/lgogdownloader");
        assert_eq!(parsed.game_program.as_deref(), Some("/opt/gogdl"));
        assert_eq!(parsed.auth_file, Some(PathBuf::from("/tmp/auth.json")));
        assert_eq!(parsed.dir, Some(PathBuf::from("/games/gog")));
        assert_eq!(parsed.positional, ["install", "1207660623"]);
        assert!(parsed.dry_run);

        // `--program` defaults to the library CLI; the game CLI defaults to gogdl.
        let plain = parse_store_args(&[], StoreCli::Gog).unwrap();
        assert_eq!(plain.program, "lgogdownloader");
        assert_eq!(plain.game_program, None);
        let backend = gog_backend(&plain);
        assert_eq!(backend.library_spec().program, "lgogdownloader");
        assert_eq!(backend.game_spec().program, "gogdl");

        let error = parse_store_args(&["--bogus".to_string()], StoreCli::Gog)
            .unwrap_err()
            .to_string();
        assert!(error.contains("for `gog`"), "{error}");
        assert!(error.contains("lgogdownloader --list json"), "{error}");
    }

    #[test]
    fn parses_launch_options_actions_and_targets() {
        let parsed = parse_launch_options_args(&[
            "620".to_string(),
            "--steam-root".to_string(),
            "/steam".to_string(),
            "--user=123".to_string(),
            "--set".to_string(),
            "DXVK_HUD=fps %command%".to_string(),
            "--dry-run".to_string(),
            "--json".to_string(),
        ])
        .unwrap();

        assert_eq!(parsed.app_id.as_deref(), Some("620"));
        assert_eq!(parsed.steam_root, Some(PathBuf::from("/steam")));
        assert_eq!(parsed.user.as_deref(), Some("123"));
        assert_eq!(
            parsed.action,
            Some(LaunchAction::Set("DXVK_HUD=fps %command%".to_string()))
        );
        assert!(parsed.dry_run && parsed.json && !parsed.force && !parsed.help);

        let shim = parse_launch_options_args(&["620".to_string(), "--shim".to_string()]).unwrap();
        assert_eq!(shim.action, Some(LaunchAction::Shim));
        let clear = parse_launch_options_args(&["620".to_string(), "--clear".to_string()]).unwrap();
        assert_eq!(clear.action, Some(LaunchAction::Clear));
        let restore =
            parse_launch_options_args(&["620".to_string(), "--restore".to_string()]).unwrap();
        assert_eq!(restore.action, Some(LaunchAction::Restore));
    }

    #[test]
    fn rejects_conflicting_or_malformed_launch_options_args() {
        for args in [
            vec!["620", "--set", "a", "--clear"],
            vec!["620", "--shim", "--restore"],
            vec!["620", "--set"],
            vec!["620", "--localconfig="],
            vec!["620", "440"],
            vec!["--bogus"],
        ] {
            let args: Vec<String> = args.into_iter().map(str::to_string).collect();
            assert!(
                parse_launch_options_args(&args).is_err(),
                "accepted {args:?}"
            );
        }

        let help = parse_launch_options_args(&["--help".to_string()]).unwrap();
        assert!(help.help);
    }

    #[test]
    fn derives_the_steam_root_from_a_localconfig_path() {
        assert_eq!(
            derive_steam_root(Path::new(
                "/games/Steam/userdata/123/config/localconfig.vdf"
            )),
            Some(PathBuf::from("/games/Steam"))
        );
        assert_eq!(derive_steam_root(Path::new("/tmp/localconfig.vdf")), None);
    }

    #[test]
    fn derives_the_steam_root_from_a_config_vdf_path() {
        assert_eq!(
            derive_steam_root_from_config(Path::new("/games/Steam/config/config.vdf")),
            Some(PathBuf::from("/games/Steam"))
        );
        assert_eq!(
            derive_steam_root_from_config(Path::new("/tmp/config.vdf")),
            None
        );
    }

    #[test]
    fn parses_compat_tool_actions_and_targets() {
        let parsed = parse_compat_tool_args(&[
            "620".to_string(),
            "--steam-root".to_string(),
            "/steam".to_string(),
            "--set=proton-cachyos".to_string(),
            "--priority".to_string(),
            "250".to_string(),
            "--dry-run".to_string(),
            "--json".to_string(),
        ])
        .unwrap();

        assert_eq!(parsed.app_id.as_deref(), Some("620"));
        assert_eq!(parsed.steam_root, Some(PathBuf::from("/steam")));
        assert_eq!(
            parsed.action,
            Some(CompatAction::Set("proton-cachyos".to_string()))
        );
        assert_eq!(parsed.priority(), 250);
        assert!(parsed.dry_run && parsed.json && !parsed.force && !parsed.help);

        let default_priority = parse_compat_tool_args(&[
            "620".to_string(),
            "--set".to_string(),
            "proton_9".to_string(),
        ])
        .unwrap();
        assert_eq!(default_priority.priority(), DEFAULT_PRIORITY);

        let clear = parse_compat_tool_args(&["620".to_string(), "--clear".to_string()]).unwrap();
        assert_eq!(clear.action, Some(CompatAction::Clear));
        let restore =
            parse_compat_tool_args(&["620".to_string(), "--restore".to_string()]).unwrap();
        assert_eq!(restore.action, Some(CompatAction::Restore));
    }

    #[test]
    fn parses_logs_arguments() {
        let parsed = parse_logs_args(&[
            "--dir".to_string(),
            "/logs".to_string(),
            "--file=shim.log".to_string(),
            "--tail".to_string(),
            "50".to_string(),
            "--grep".to_string(),
            "appid=620".to_string(),
            "--json".to_string(),
        ])
        .unwrap();

        assert_eq!(parsed.dir, Some(PathBuf::from("/logs")));
        assert_eq!(parsed.file.as_deref(), Some("shim.log"));
        assert_eq!(parsed.tail, 50);
        assert_eq!(parsed.grep.as_deref(), Some("appid=620"));
        assert!(parsed.json && !parsed.help);

        // The tail defaults to 200 lines and nothing else is set.
        let bare = parse_logs_args(&[]).unwrap();
        assert_eq!(bare, LogsArgs::default());
        assert_eq!(bare.tail, DEFAULT_LOG_TAIL);

        for args in [
            vec!["--nope".to_string()],
            vec!["--tail".to_string(), "many".to_string()],
            vec!["--dir".to_string()],
        ] {
            assert!(parse_logs_args(&args).is_err(), "accepted {args:?}");
        }

        assert!(parse_logs_args(&["--help".to_string()]).unwrap().help);
    }

    #[test]
    fn parses_import_arguments() {
        let parsed = parse_import_args(&[
            "--heroic".to_string(),
            "/cfg/heroic".to_string(),
            "--heroic=/other/heroic".to_string(),
            "--lutris".to_string(),
            "/data/lutris".to_string(),
            "--bottles=/data/bottles".to_string(),
            "--json".to_string(),
        ])
        .unwrap();

        assert_eq!(
            parsed.heroic,
            [PathBuf::from("/cfg/heroic"), PathBuf::from("/other/heroic")]
        );
        assert_eq!(parsed.lutris, [PathBuf::from("/data/lutris")]);
        assert_eq!(parsed.bottles, [PathBuf::from("/data/bottles")]);
        assert!(parsed.json && !parsed.help);

        // No flags means the default locations, which the report resolves.
        assert_eq!(parse_import_args(&[]).unwrap(), ImportArgs::default());

        for args in [
            vec!["--nope".to_string()],
            vec!["--heroic".to_string()],
            vec!["--lutris=".to_string()],
        ] {
            assert!(parse_import_args(&args).is_err(), "accepted {args:?}");
        }

        assert!(parse_import_args(&["--help".to_string()]).unwrap().help);
    }

    #[test]
    fn import_lines_and_summary_read_the_report() {
        let dir = tempfile::tempdir().unwrap();
        let heroic = dir.path().join("heroic");
        std::fs::create_dir_all(heroic.join("legendaryConfig/legendary")).unwrap();
        std::fs::write(
            heroic.join("legendaryConfig/legendary/installed.json"),
            r#"[{"app_name":"Fortnite","title":"Fortnite","install_path":"/games/Fortnite",
                 "platform":"Windows","is_dlc":false}]"#,
        )
        .unwrap();

        let report = Importer::with_roots(vec![heroic.clone()], vec![], vec![]).scan();
        assert_eq!(report_summary(&report), "1 game: epic 1");

        let legendary = &report.sources[0];
        assert_eq!(
            source_line(legendary),
            format!("{}  (1 game)", legendary.path.display())
        );
        let missing = report
            .sources
            .iter()
            .find(|source| !source.found())
            .expect("the other Heroic files are not there");
        assert!(source_line(missing).ends_with("  (missing)"));
    }

    #[test]
    fn logs_lists_and_tails_a_fixture_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shim.log"), "one\ntwo\n").unwrap();
        std::fs::write(dir.path().join("steam-620.log"), "proton\n").unwrap();
        let path = dir.path().to_string_lossy().into_owned();

        logs(&["--dir".to_string(), path.clone()]).unwrap();
        logs(&[
            "--dir".to_string(),
            path,
            "--file".to_string(),
            "shim.log".to_string(),
            "--tail".to_string(),
            "1".to_string(),
        ])
        .unwrap();
    }

    #[test]
    fn parses_prefix_arguments() {
        let parsed = parse_prefix_args(&[
            "gog".to_string(),
            "1207660623".to_string(),
            "--root=/data/prefixes".to_string(),
            "--proton".to_string(),
            "/opt/proton".to_string(),
            "--json".to_string(),
        ])
        .unwrap();

        assert_eq!(parsed.positional, ["gog", "1207660623"]);
        assert_eq!(parsed.root, Some(PathBuf::from("/data/prefixes")));
        assert_eq!(parsed.proton, Some(PathBuf::from("/opt/proton")));
        assert!(parsed.json && !parsed.help);

        assert!(parse_prefix_args(&["--root=".to_string()]).is_err());
        assert!(parse_prefix_args(&["--nope".to_string()]).is_err());
        assert!(parse_prefix_args(&["--help".to_string()]).unwrap().help);
    }

    #[test]
    fn prefix_resolves_the_layout_without_creating_it() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().to_string_lossy().into_owned();

        // With an explicit Proton the auto-discovery is skipped, so the test is
        // independent of what the machine has installed.
        prefix(&[
            "epic".to_string(),
            "Gizmo".to_string(),
            "--root".to_string(),
            root_path.clone(),
            "--proton".to_string(),
            "/opt/proton-cachyos/proton".to_string(),
            "--json".to_string(),
        ])
        .unwrap();

        // A prefix is created lazily by Proton on first launch.
        assert!(!root.path().join("epic-Gizmo").exists());

        assert!(prefix(&["epic".to_string()]).is_err(), "missing id");
        assert!(
            prefix(&[
                "nintendo".to_string(),
                "1".to_string(),
                "--root".to_string(),
                root_path
            ])
            .is_err(),
            "unknown store"
        );
    }

    #[test]
    fn parses_disk_arguments_and_sizes() {
        let parsed = parse_disk_args(&[
            "/".to_string(),
            "--need".to_string(),
            "20G".to_string(),
            "--json".to_string(),
        ])
        .unwrap();
        assert_eq!(parsed.path, Some(PathBuf::from("/")));
        assert_eq!(parsed.need, Some(20 * 1024 * 1024 * 1024));
        assert!(parsed.json);

        assert!(parse_disk_args(&["a".to_string(), "b".to_string()]).is_err());
        assert!(parse_disk_args(&["--need".to_string()]).is_err());

        assert_eq!(parse_size_bytes("1500").unwrap(), 1500);
        assert_eq!(parse_size_bytes("1K").unwrap(), 1024);
        assert_eq!(parse_size_bytes("1KiB").unwrap(), 1024);
        assert_eq!(parse_size_bytes("1.5GiB").unwrap(), 1_610_612_736);
        assert_eq!(parse_size_bytes("2T").unwrap(), 2 * 1024u64.pow(4));
        assert!(parse_size_bytes("1.5").is_err());
        assert!(parse_size_bytes("12x").is_err());
        assert!(parse_size_bytes("").is_err());
    }

    #[test]
    fn disk_reports_a_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_string_lossy().into_owned();

        disk(&[
            path.clone(),
            "--need".to_string(),
            "1".to_string(),
            "--json".to_string(),
        ])
        .unwrap();
        disk(&[path]).unwrap();
    }

    #[test]
    fn parses_store_status_and_update_arguments() {
        let parsed = parse_stores_args(&[
            "--store".to_string(),
            "epic".to_string(),
            "--store=gog".to_string(),
            "--list".to_string(),
            "--program=/opt/legendary".to_string(),
            "--json".to_string(),
        ])
        .unwrap();
        assert_eq!(parsed.subcommand, None);
        assert_eq!(parsed.filters, ["epic", "gog"]);
        assert!(parsed.list && parsed.json && !parsed.help);
        assert_eq!(parsed.program.as_deref(), Some("/opt/legendary"));

        let updates = parse_stores_args(&[
            "updates".to_string(),
            "--steam-root".to_string(),
            "/steam".to_string(),
            "--with-stores".to_string(),
        ])
        .unwrap();
        assert_eq!(updates.subcommand.as_deref(), Some("updates"));
        assert_eq!(updates.steam_root, Some(PathBuf::from("/steam")));
        assert!(updates.with_stores);

        let sync = parse_stores_args(&[
            "sync".to_string(),
            "--store=epic".to_string(),
            "--db=/tmp/x.db".to_string(),
            "--dry-run".to_string(),
        ])
        .unwrap();
        assert_eq!(sync.subcommand.as_deref(), Some("sync"));
        assert_eq!(sync.filters, ["epic"]);
        assert_eq!(sync.db, Some(PathBuf::from("/tmp/x.db")));
        assert!(sync.dry_run);

        assert!(parse_stores_args(&["status".to_string(), "extra".to_string()]).is_err());
        assert!(parse_stores_args(&["--store".to_string()]).is_err());
        assert!(parse_stores_args(&["--bogus".to_string()]).is_err());
        assert!(parse_stores_args(&["--help".to_string()]).unwrap().help);
    }

    #[test]
    fn stores_status_probes_one_store_with_an_explicit_program() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("legendary");
        std::fs::write(
            &program,
            "#!/bin/sh\n\
             if [ \"$1\" = \"--version\" ]; then printf 'legendary version \"0.20.35\"\\n'; exit 0; fi\n\
             if [ \"$1\" = \"status\" ]; then printf '{\"account\": \"<not logged in>\"}'; exit 0; fi\n\
             exit 2\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&program).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&program, permissions).unwrap();
        }
        let program = program.to_string_lossy().into_owned();

        // Both render paths run, and a signed-out store is a row state, not an
        // error: the command still exits cleanly.
        stores(&[
            "--store".to_string(),
            "epic".to_string(),
            "--program".to_string(),
            program.clone(),
            "--json".to_string(),
        ])
        .unwrap();
        stores(&[
            "--store".to_string(),
            "epic".to_string(),
            "--program".to_string(),
            program,
        ])
        .unwrap();
    }

    #[test]
    fn stores_rejects_flags_that_belong_to_the_other_subcommand() {
        // `--with-stores`/`--steam-root` are `updates` options.
        assert!(stores(&["--with-stores".to_string()]).is_err());
        // `--list` is a status option.
        assert!(stores(&["updates".to_string(), "--list".to_string()]).is_err());
        // `--db`/`--dry-run` are `sync` options.
        assert!(stores(&["--dry-run".to_string()]).is_err());
        assert!(stores(&["updates".to_string(), "--dry-run".to_string()]).is_err());
        assert!(stores(&["--db".to_string(), "/tmp/x.db".to_string()]).is_err());
        // `--list` and `--steam-root` are not sync options either.
        assert!(stores(&["sync".to_string(), "--list".to_string()]).is_err());
        // A program override needs exactly one store selected.
        assert!(stores(&["--program".to_string(), "/tmp/cli".to_string()]).is_err());
        assert!(
            stores(&[
                "sync".to_string(),
                "--program".to_string(),
                "/tmp/cli".to_string()
            ])
            .is_err()
        );
        // Steam has no CLI store backend.
        assert!(stores(&["--store".to_string(), "steam".to_string()]).is_err());
        // Only the three subcommands exist.
        assert!(stores(&["bogus".to_string()]).is_err());
    }

    /// A fake `legendary` answering both listings from the checked-in fixtures.
    fn fake_legendary(dir: &Path) -> String {
        let list = dir.join("list.json");
        let installed = dir.join("list-installed.json");
        std::fs::write(&list, include_str!("../../../fixtures/legendary/list.json")).unwrap();
        std::fs::write(
            &installed,
            include_str!("../../../fixtures/legendary/list-installed.json"),
        )
        .unwrap();

        let program = dir.join("legendary");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\n\
                 case \"$1\" in\n\
                 list-installed) cat '{installed}' ;;\n\
                 list) cat '{list}' ;;\n\
                 *) echo \"unexpected: $*\" >&2; exit 2 ;;\n\
                 esac\n",
                installed = installed.display(),
                list = list.display(),
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&program).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&program, permissions).unwrap();
        }
        program.to_string_lossy().into_owned()
    }

    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn stores_sync_writes_one_stores_rows_and_keeps_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_legendary(dir.path());
        let db_path = dir.path().join("pitagames.db");

        // A previous state: a Steam row, two Epic rows, one of which the store
        // no longer owns.
        let runtime = test_runtime();
        runtime.block_on(async {
            let db = Db::open(&db_path).await.unwrap();
            db.upsert_game(&Game::new(GameId::new(Store::Steam, "620"), "Portal 2"))
                .await
                .unwrap();
            db.upsert_game(&Game::new(GameId::new(Store::Epic, "Gone"), "Gone"))
                .await
                .unwrap();
        });

        stores(&[
            "sync".to_string(),
            "--store".to_string(),
            "epic".to_string(),
            "--program".to_string(),
            program.clone(),
            "--db".to_string(),
            db_path.to_string_lossy().into_owned(),
            "--log-dir".to_string(),
            dir.path().join("logs").to_string_lossy().into_owned(),
            "--json".to_string(),
        ])
        .unwrap();

        runtime.block_on(async {
            let db = Db::open(&db_path).await.unwrap();
            let epic = db.store_games(Store::Epic).await.unwrap();
            let titles: Vec<&str> = epic.iter().map(|game| game.title.as_str()).collect();
            assert_eq!(titles, ["Gizmo", "Sugar"], "Gone was dropped, no DLC row");
            assert!(epic[0].installed && epic[0].install_dir.is_some());
            assert!(!epic[1].installed, "an owned game is an Install row");

            assert!(
                db.game(&GameId::new(Store::Steam, "620"))
                    .await
                    .unwrap()
                    .is_some(),
                "another store's rows are never deleted"
            );
        });
    }

    #[test]
    fn stores_sync_dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_legendary(dir.path());
        let db_path = dir.path().join("pitagames.db");

        stores(&[
            "sync".to_string(),
            "--store=epic".to_string(),
            "--program".to_string(),
            program,
            "--db".to_string(),
            db_path.to_string_lossy().into_owned(),
            "--log-dir".to_string(),
            dir.path().join("logs").to_string_lossy().into_owned(),
            "--dry-run".to_string(),
        ])
        .unwrap();

        test_runtime().block_on(async {
            let db = Db::open(&db_path).await.unwrap();
            assert!(db.store_games(Store::Epic).await.unwrap().is_empty());
        });
    }

    #[test]
    fn stores_sync_leaves_an_incomplete_listing_alone() {
        let dir = tempfile::tempdir().unwrap();
        // A legendary that logs an error and exits 0 for both listings: the
        // classic exit-0 trap, which must never look like an empty library.
        let program = dir.path().join("legendary");
        std::fs::write(
            &program,
            "#!/bin/sh\n\
             echo '[cli] ERROR: No saved credentials' >&2\n\
             exit 0\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&program).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&program, permissions).unwrap();
        }
        let db_path = dir.path().join("pitagames.db");

        let runtime = test_runtime();
        runtime.block_on(async {
            let db = Db::open(&db_path).await.unwrap();
            db.upsert_game(&Game::new(GameId::new(Store::Epic, "Gizmo"), "Gizmo"))
                .await
                .unwrap();
        });

        stores(&[
            "sync".to_string(),
            "--store=epic".to_string(),
            "--program".to_string(),
            program.to_string_lossy().into_owned(),
            "--db".to_string(),
            db_path.to_string_lossy().into_owned(),
            "--log-dir".to_string(),
            dir.path().join("logs").to_string_lossy().into_owned(),
        ])
        .unwrap();

        runtime.block_on(async {
            let db = Db::open(&db_path).await.unwrap();
            let epic = db.store_games(Store::Epic).await.unwrap();
            assert_eq!(epic.len(), 1, "the failed sync wrote nothing");
            assert_eq!(epic[0].id.store_id, "Gizmo");
        });
    }

    #[test]
    fn stores_updates_reads_a_steam_library() {
        let root = tempfile::tempdir().unwrap();
        let steam = root.path().join("Steam");

        let folders = format!(
            "\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n}}\n",
            steam.display()
        );
        write_fixture(&steam.join("steamapps/libraryfolders.vdf"), folders);
        // StateFlags 12 = FULLY_INSTALLED | UPDATE_REQUIRED → update waiting.
        write_fixture(
            &steam.join("steamapps/appmanifest_620.acf"),
            acf(620, "Portal 2", 12),
        );
        // StateFlags 4 = FULLY_INSTALLED → up to date, not listed.
        write_fixture(
            &steam.join("steamapps/appmanifest_570.acf"),
            acf(570, "Dota 2", 4),
        );

        let parsed = parse_stores_args(&[
            "updates".to_string(),
            "--steam-root".to_string(),
            steam.to_string_lossy().into_owned(),
        ])
        .unwrap();
        let (found, notes) = collect_updates(&parsed, &stores_options(&parsed));

        assert_eq!(
            found.len(),
            1,
            "only the pending update is listed: {notes:?}"
        );
        assert_eq!(found[0].id.store_id, "620");
        assert_eq!(found[0].title, "Portal 2");
        assert_eq!(
            found[0].source,
            pitagames_stores::status::UpdateSource::SteamManifest
        );

        // The command renders the same list without erroring.
        stores(&[
            "updates".to_string(),
            "--steam-root".to_string(),
            steam.to_string_lossy().into_owned(),
            "--json".to_string(),
        ])
        .unwrap();
    }

    fn write_fixture(path: &Path, content: impl AsRef<[u8]>) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn acf(app_id: u32, name: &str, state_flags: u32) -> String {
        format!(
            "\"AppState\"\n{{\n\t\"appid\"\t\t\"{app_id}\"\n\t\"name\"\t\t\"{name}\"\n\t\"StateFlags\"\t\"{state_flags}\"\n\t\"installdir\"\t\"{name}\"\n\t\"SizeOnDisk\"\t\"1024\"\n}}\n"
        )
    }

    #[test]
    fn rejects_conflicting_or_malformed_compat_tool_args() {
        for args in [
            vec!["620", "--set", "a", "--clear"],
            vec!["620", "--clear", "--restore"],
            vec!["620", "--set"],
            vec!["620", "--config="],
            vec!["620", "--priority", "-5"],
            vec!["620", "--priority", "many"],
            vec!["620", "440"],
            vec!["--bogus"],
        ] {
            let args: Vec<String> = args.into_iter().map(str::to_string).collect();
            assert!(parse_compat_tool_args(&args).is_err(), "accepted {args:?}");
        }

        let help = parse_compat_tool_args(&["--help".to_string()]).unwrap();
        assert!(help.help);
    }

    #[test]
    fn compat_set_builds_the_planned_mapping() {
        let tool = planned_compat(&CompatAction::Set("GE-Proton9-20".to_string()), 300).unwrap();
        assert_eq!(tool.name, "GE-Proton9-20");
        assert_eq!(tool.priority, 300);
        assert_eq!(tool.config, "");
        assert_eq!(planned_compat(&CompatAction::Clear, 250), None);
    }

    #[test]
    fn shim_plan_replaces_an_existing_shim_and_keeps_other_options() {
        let next = planned_value(
            &LaunchAction::Shim,
            "620",
            Some("DXVK_HUD=fps pitagames-shim 440 %command% -novid"),
        );
        assert_eq!(
            next.as_deref(),
            Some("DXVK_HUD=fps pitagames-shim 620 %command% -novid")
        );

        let empty = planned_value(&LaunchAction::Shim, "620", None);
        assert_eq!(empty.as_deref(), Some("pitagames-shim 620 %command%"));
    }

    #[test]
    fn reads_are_returned_as_planned_writes_only_for_read_and_restore() {
        assert_eq!(
            planned_value(&LaunchAction::Read, "620", Some("-novid %command%")).as_deref(),
            Some("-novid %command%")
        );
        assert_eq!(planned_value(&LaunchAction::Clear, "620", Some("x")), None);
        assert_eq!(
            planned_value(&LaunchAction::Set("y".to_string()), "620", Some("x")).as_deref(),
            Some("y")
        );
    }

    #[test]
    fn picks_the_only_steam_user_and_warns_when_several_exist() {
        let dir = tempfile::tempdir().unwrap();
        let userdata = dir.path().join("userdata");
        std::fs::create_dir_all(userdata.join("111/config")).unwrap();
        std::fs::write(userdata.join("111/config/localconfig.vdf"), "{}").unwrap();

        assert_eq!(pick_user(&userdata).unwrap(), "111");

        std::fs::create_dir_all(userdata.join("222/config")).unwrap();
        std::fs::write(userdata.join("222/config/localconfig.vdf"), "{}").unwrap();
        // Deterministic first-by-name; the warning goes to stderr.
        assert_eq!(pick_user(&userdata).unwrap(), "111");

        std::fs::remove_dir_all(userdata.join("111")).unwrap();
        std::fs::remove_dir_all(userdata.join("222")).unwrap();
        assert!(pick_user(&userdata).is_err());
    }

    #[test]
    fn truncates_long_titles_to_width() {
        let fitted = fit("A very long game title that does not fit in the column", 20);
        assert_eq!(fitted.chars().count(), 20);
        assert!(fitted.ends_with('…'));
    }

    #[test]
    fn formats_binary_sizes() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(48_318_382_080), "45.0 GiB");
    }

    #[test]
    fn printer_emits_each_state_once_and_buckets_percentages() {
        let mut printer = SamplePrinter::default();
        let waiting = InstallSample::WaitingForManifest;
        let downloading = |percent: u64| {
            InstallSample::InProgress(InstallProgress {
                phase: InstallPhase::Downloading,
                progress: Some(DownloadProgress {
                    downloaded: percent * 10,
                    total: 1000,
                }),
            })
        };

        assert!(printer.render(&waiting).is_some());
        assert!(printer.render(&waiting).is_none(), "repeated state");

        assert_eq!(
            printer.render(&downloading(50)).as_deref(),
            Some("  downloading       50%  500 B / 1000 B")
        );
        assert!(printer.render(&downloading(51)).is_none(), "same 5% bucket");
        assert!(printer.render(&downloading(55)).is_some(), "next bucket");

        let installed = InstallSample::Installed {
            manifest: PathBuf::from("/steam/steamapps/appmanifest_620.acf"),
        };
        assert!(
            printer.render(&installed).is_none(),
            "the caller prints the terminal line"
        );
    }

    #[test]
    fn rejects_non_numeric_or_zero_timeouts() {
        assert!(parse_timeout("0").is_err());
        assert!(parse_timeout("soon").is_err());
        assert_eq!(parse_timeout("90").unwrap(), Duration::from_secs(90));
    }

    #[test]
    fn lists_available_platforms() {
        assert_eq!(
            platforms_line(Platforms {
                windows: true,
                mac: false,
                linux: true
            }),
            "Windows, Linux"
        );
        assert_eq!(platforms_line(Platforms::default()), "none listed");
    }

    #[test]
    fn parses_proton_roots_and_rejects_unknown_flags() {
        let args = parse_proton_args(&[
            "--root".to_string(),
            "/compat".to_string(),
            "--root=/other".to_string(),
            "--steam-root".to_string(),
            "/steam".to_string(),
            "--json".to_string(),
        ])
        .unwrap();

        assert!(args.json);
        assert!(!args.help);
        assert_eq!(
            args.roots,
            [
                ProtonRoot::new("/compat", ProtonSource::UserCompatibilityTools),
                ProtonRoot::new("/other", ProtonSource::UserCompatibilityTools),
                ProtonRoot::new(
                    "/steam/compatibilitytools.d",
                    ProtonSource::UserCompatibilityTools
                ),
                ProtonRoot::new("/steam/steamapps/common", ProtonSource::SteamCommon),
            ]
        );

        assert!(parse_proton_args(&["--bogus".to_string()]).is_err());
        assert!(parse_proton_args(&["--root".to_string()]).is_err());
        assert!(parse_proton_args(&["--root=".to_string()]).is_err());
    }

    #[test]
    fn renders_proton_report_with_the_auto_choice() {
        use pitagames_platform::proton::ProtonKind;

        let install = |name: &str, kind: ProtonKind| ProtonInstall {
            path: PathBuf::from(format!("/r/{name}")),
            name: name.to_string(),
            kind,
            version: None,
            source: ProtonSource::UserCompatibilityTools,
        };
        let report = DiscoveryReport {
            installs: vec![
                install("GE-Proton9-20", ProtonKind::Ge),
                install("Proton 9.0", ProtonKind::Valve),
            ],
            roots: vec![ProtonRoot::new("/r", ProtonSource::UserCompatibilityTools)],
            warnings: Vec::new(),
        };

        let lines = proton_lines(&report);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Searched 1 location"))
        );
        assert!(lines.iter().any(|line| line.contains("GE-Proton9-20")));
        assert!(lines.iter().any(|line| line.contains("Steam Proton")));
        assert!(
            lines
                .iter()
                .any(|line| line == "Auto selects: GE-Proton9-20 (/r/GE-Proton9-20)")
        );
    }

    #[test]
    fn explains_an_empty_proton_report() {
        let report = DiscoveryReport {
            roots: vec![ProtonRoot::new("/r", ProtonSource::UserCompatibilityTools)],
            ..DiscoveryReport::default()
        };
        let lines = proton_lines(&report);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("No Proton installations"))
        );
        assert!(!lines.iter().any(|line| line.contains("Auto selects")));
    }

    #[test]
    fn parses_compat_arguments() {
        let parsed = parse_compat_args(&[
            "steam".to_string(),
            "1091500".to_string(),
            "--shipped".to_string(),
            "/compat".to_string(),
            "--user=/overrides".to_string(),
            "--json".to_string(),
        ])
        .unwrap();

        assert_eq!(parsed.positional, ["steam", "1091500"]);
        assert_eq!(parsed.shipped, vec![PathBuf::from("/compat")]);
        assert_eq!(parsed.user, vec![PathBuf::from("/overrides")]);
        assert!(parsed.json && !parsed.help);

        let list = parse_compat_args(&[]).unwrap();
        assert_eq!(list, CompatArgs::default());

        for args in [vec!["--shipped"], vec!["--shipped="], vec!["--bogus"]] {
            let args: Vec<String> = args.into_iter().map(str::to_string).collect();
            assert!(parse_compat_args(&args).is_err(), "accepted {args:?}");
        }

        assert!(parse_compat_args(&["--help".to_string()]).unwrap().help);
    }

    #[test]
    fn renders_one_compat_entry() {
        let entry = CompatEntry::from_toml(
            r#"
[game]
store = "steam"
store_id = "1091500"
title = "Cyberpunk 2077"
engine = "REDengine"
verified = ["rdna4"]

[upscaling]
supported = ["dlss", "fsr3"]
recommended = { rdna4 = "dlss_to_fsr4" }
preferred_dll = "dbghelp.dll"

[neural_rendering]
min_mesa = "26.2"
model_scale_default = { rdna4 = 100 }
"#,
        )
        .unwrap();

        let lines = compat_detail_lines(&entry);
        let joined = lines.join("\n");
        assert!(
            joined.contains("Cyberpunk 2077  [steam:1091500]"),
            "{joined}"
        );
        assert!(joined.contains("Engine:       REDengine"), "{joined}");
        assert!(
            joined.contains("RDNA 4 (RX 9000) -> dlss_to_fsr4"),
            "{joined}"
        );
        assert!(joined.contains("preferred dll: dbghelp.dll"), "{joined}");
        assert!(joined.contains("Neural rendering:"), "{joined}");
        assert!(joined.contains("min mesa:      26.2"), "{joined}");
    }

    #[test]
    fn lists_a_compat_database_and_reports_an_empty_one() {
        let dir = tempfile::tempdir().unwrap();
        let shipped = dir.path().join("shipped");
        std::fs::create_dir_all(&shipped).unwrap();
        std::fs::write(
            shipped.join("a.toml"),
            r#"
[game]
store = "steam"
store_id = "1"
title = "Game"
verified = ["rdna4"]
"#,
        )
        .unwrap();

        let db = CompatDb::load_from(&[shipped], &[]);
        let lines = compat_list_lines(&db);
        assert!(lines[0].contains("1 entries from 1 file(s)"), "{lines:?}");
        assert!(lines.iter().any(|line| line.contains("steam:1")));

        let empty = CompatDb::load_from(&[dir.path().join("missing")], &[]);
        assert!(
            compat_list_lines(&empty)
                .iter()
                .any(|line| line.contains("No entries loaded"))
        );
    }

    #[test]
    fn parses_anticheat_arguments() {
        let parsed = parse_anticheat_args(&[
            "1091500".to_string(),
            "--dir".to_string(),
            "/games/cp2077".to_string(),
            "--shipped=/compat".to_string(),
            "--user".to_string(),
            "/overrides".to_string(),
            "--json".to_string(),
        ])
        .unwrap();

        assert_eq!(parsed.app_id.as_deref(), Some("1091500"));
        assert_eq!(parsed.dir, Some(PathBuf::from("/games/cp2077")));
        assert_eq!(parsed.shipped, vec![PathBuf::from("/compat")]);
        assert_eq!(parsed.user, vec![PathBuf::from("/overrides")]);
        assert!(parsed.json && !parsed.help);

        for args in [
            vec!["--dir"],
            vec!["--dir="],
            vec!["1", "2"],
            vec!["--bogus"],
        ] {
            let args: Vec<String> = args.into_iter().map(str::to_string).collect();
            assert!(parse_anticheat_args(&args).is_err(), "accepted {args:?}");
        }

        assert!(parse_anticheat_args(&["--help".to_string()]).unwrap().help);
    }

    #[test]
    fn renders_the_anticheat_gate() {
        use pitagames_platform::anticheat::{AnticheatFinding, AnticheatScan};

        let scan = AnticheatScan {
            kind: AntiCheat::Eac,
            findings: vec![AnticheatFinding {
                kind: AntiCheat::Eac,
                marker: "easyanticheat",
                path: PathBuf::from("/games/Game/EasyAntiCheat"),
            }],
            scanned: true,
            truncated: false,
        };
        let merged = EffectiveAnticheat {
            kind: AntiCheat::Eac,
            source: AnticheatSource::Detected,
        };
        let decision = GateDecision::ConfirmationRequired {
            anticheat: AntiCheat::Eac,
        };

        let lines = anticheat_lines(
            "1091500",
            Some(Path::new("/games/Game")),
            &scan,
            AntiCheat::None,
            merged,
            decision,
        );
        let joined = lines.join("\n");
        assert!(joined.contains("Anti-cheat for steam:1091500"), "{joined}");
        assert!(joined.contains("eac (1 marker)"), "{joined}");
        assert!(joined.contains("curated:     none"), "{joined}");
        assert!(joined.contains("effective:   eac (detected)"), "{joined}");
        assert!(joined.contains("confirmation required"), "{joined}");
        assert!(joined.contains("Easy Anti-Cheat"), "{joined}");

        // A clean scan with no compat entry allows injection.
        let clean = AnticheatScan {
            scanned: true,
            ..AnticheatScan::default()
        };
        let merged = pitagames_platform::anticheat::effective(AntiCheat::Unknown, &clean);
        let lines = anticheat_lines(
            "1",
            None,
            &clean,
            AntiCheat::Unknown,
            merged,
            GateDecision::Allowed,
        );
        let joined = lines.join("\n");
        assert!(joined.contains("none (no markers found)"), "{joined}");
        assert!(joined.contains("(not installed here)"), "{joined}");
        assert!(joined.contains("injection allowed"), "{joined}");
    }

    #[test]
    fn parses_desktop_entry_actions_and_exec_argv() {
        let parsed = parse_desktop_entry_args(&[
            "--exec".to_string(),
            "/opt/Pita Games/pitagames".to_string(),
            "--exec=--console".to_string(),
            "--path".to_string(),
            "/tmp/pitagames.desktop".to_string(),
            "--write".to_string(),
            "--json".to_string(),
        ])
        .unwrap();

        assert_eq!(
            parsed.exec,
            [
                "/opt/Pita Games/pitagames".to_string(),
                "--console".to_string()
            ]
        );
        assert_eq!(parsed.path, Some(PathBuf::from("/tmp/pitagames.desktop")));
        assert!(parsed.write && parsed.json && !parsed.print && !parsed.remove);

        let printed = parse_desktop_entry_args(&["--print".to_string()]).unwrap();
        assert!(printed.print && !printed.write && !printed.remove);

        for args in [
            vec!["--print", "--write"],
            vec!["--write", "--remove"],
            vec!["--exec"],
            vec!["--path="],
            vec!["extra"],
        ] {
            let args: Vec<String> = args.into_iter().map(str::to_string).collect();
            assert!(
                parse_desktop_entry_args(&args).is_err(),
                "accepted {args:?}"
            );
        }

        let help = parse_desktop_entry_args(&["--help".to_string()]).unwrap();
        assert!(help.help);
    }

    #[test]
    fn desktop_entry_writes_reads_back_and_removes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("applications").join("pitagames.desktop");

        let args = |action: &str| -> Vec<String> {
            vec![
                "--exec".to_string(),
                "pitagames".to_string(),
                "--path".to_string(),
                path.display().to_string(),
                action.to_string(),
            ]
        };

        desktop_entry(&args("--write")).unwrap();
        assert!(path.is_file());
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("Exec=pitagames\n"), "{contents}");

        // Writing again is a no-op rather than an error.
        desktop_entry(&args("--write")).unwrap();

        desktop_entry(&args("--remove")).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn parses_activate_targets() {
        let parsed = activate(&["--help".to_string()]);
        assert!(parsed.is_ok());

        let error = activate(&[
            "--socket".to_string(),
            "/nonexistent/pitagames/instance.sock".to_string(),
        ])
        .unwrap_err()
        .to_string();
        assert!(error.contains("no running PitaGames"), "{error}");
    }
}
