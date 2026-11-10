//! Headless developer CLI.
//!
//! This is not the shipping `pitagames` binary (that will be the Dioxus app);
//! it exists so the scan layer can be exercised without a GUI, in CI, and on
//! machines where the webview stack is unavailable.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use pitagames_core::{
    Game, GameId, GameMetadata, LaunchSpec, Platforms, Store, read_shim_profile,
    shim_launch_options, shim_profile_path, steam_install,
};
use pitagames_platform::compat::{CompatDb, CompatEntry};
use pitagames_platform::proton::{self, DiscoveryReport, ProtonInstall, ProtonRoot, ProtonSource};
use pitagames_secrets::{SteamApiKey, SteamKeyStore};
use pitagames_stores::compat_tool::CompatTool;
use pitagames_stores::install::{DEFAULT_POLL_INTERVAL, InstallMonitor, InstallSample};
use pitagames_stores::steam::{self, ScanReport};
use pitagames_stores::steam_config::{
    self, CompatRestoreReport, CompatWriteReport, SteamProcessState, WriteOptions, WriteReport,
};
use pitagames_stores::{DEFAULT_PRIORITY, DownloadProgress};

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
        "profile" => profile(&args.collect::<Vec<_>>()),
        "launch-options" => launch_options(&args.collect::<Vec<_>>()),
        "compat-tool" => compat_tool(&args.collect::<Vec<_>>()),
        "steam-account" => steam_account(&args.collect::<Vec<_>>()),
        "-h" | "--help" | "help" => {
            println!("{}", usage());
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

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn usage() -> String {
    format!(
        "pitagames-cli — developer CLI for PitaGames\n\n{}\n\n{}",
        "USAGE:\n    pitagames-cli <command> [options]",
        "COMMANDS:\n    scan       Scan installed Steam games\n    install    Ask Steam to install a game, then poll the appmanifest\n    metadata   Fetch Steam store metadata for appids (live, uncached)\n    proton     List the Proton builds and the auto-selected runtime\n    compat     List or validate the shipped compatibility database\n    profile    Show the compiled shim profile for one Steam appid\n    launch-options  Read, set, shim or restore one game's Steam launch options\n    compat-tool  Read, force, clear or restore one game's compatibility tool\n    steam-account  Read or manage the Steam Web API key and the owned-games list\n\n\
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
}
