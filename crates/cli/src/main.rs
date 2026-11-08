mod http;
mod serve;

use ftc_core::session::Options;
use ftc_core::{magnet, Session, Snapshot, Torrent};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const USAGE: &str = "\
free-torrent-cli — a BitTorrent client in Rust

USAGE:
    ftc <file.torrent | magnet:?xt=urn:btih:...> [options]

OPTIONS:
    -o, --output <dir>    where files are written (default: current directory)
        --peers <list>    comma-separated host:port peers, skipping the tracker
        --max-peers <n>   simultaneous peer connections (default: 6)
        --no-dht          skip DHT peer discovery (trackers only)
        --seed <secs>     after finishing, serve pieces to peers for <secs>s
        --limit <rate>    cap the download rate, e.g. 500k or 2M (default: unlimited)
    -h, --help            show this help
    -V, --version         show the version

SUBCOMMANDS:
    serve                 run the web ui on http://127.0.0.1:8080
    gui                   start the web ui and open it in your browser
";

const SERVE_USAGE: &str = "\
ftc serve / ftc gui — the web ui

USAGE:
    ftc serve [options]     headless, prints the url
    ftc gui [options]       opens the page in your browser

OPTIONS:
        --bind <addr>     interface to listen on (default: 127.0.0.1)
        --port <n>        port to listen on (default: 8080, 0 picks a free one)
    -o, --output <dir>    default download directory (default: current directory)
        --max-peers <n>   simultaneous peer connections (default: 6)
        --no-dht          skip DHT peer discovery (trackers only)
        --limit <rate>    default download cap for new jobs, e.g. 2M
        --open            also open the page in your browser
    -h, --help            show this help
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("ftc: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("serve") => return run_server(&argv[1..], false),
        Some("gui") => return run_server(&argv[1..], true),
        _ => {}
    }
    let mut args = argv.into_iter();
    let mut target: Option<String> = None;
    let mut output = PathBuf::from(".");
    let mut peers: Option<Vec<SocketAddr>> = None;
    let mut options = Options::default();
    let mut seed_secs: u64 = 0;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("free-torrent-cli {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-o" | "--output" => {
                output = PathBuf::from(args.next().ok_or("--output needs a directory")?);
            }
            "--peers" => {
                let value = args.next().ok_or("--peers needs a list")?;
                let mut parsed = Vec::new();
                for entry in value.split(',') {
                    parsed.push(
                        entry
                            .trim()
                            .parse::<SocketAddr>()
                            .map_err(|err| format!("bad peer {entry:?}: {err}"))?,
                    );
                }
                peers = Some(parsed);
            }
            "--max-peers" => {
                options.max_peers = args
                    .next()
                    .ok_or("--max-peers needs a number")?
                    .parse()
                    .map_err(|err| format!("bad --max-peers: {err}"))?;
                if options.max_peers == 0 {
                    return Err("--max-peers must be at least 1".into());
                }
            }
            "--no-dht" => {
                options.use_dht = false;
            }
            "--limit" => {
                let value = args.next().ok_or("--limit needs a rate")?;
                options.limit_bps = Some(parse_rate(&value)?);
            }
            "--seed" => {
                let value = args.next().ok_or("--seed needs seconds")?;
                seed_secs = value.parse().map_err(|err| format!("bad --seed: {err}"))?;
                if seed_secs == 0 {
                    return Err("--seed must be at least 1".into());
                }
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option {other}\n\n{USAGE}"))
            }
            other => {
                if target.is_some() {
                    return Err("only one torrent or magnet may be given".into());
                }
                target = Some(other.to_string());
            }
        }
    }

    let target = target.ok_or_else(|| format!("no torrent given\n\n{USAGE}"))?;
    let torrent = if target.starts_with("magnet:") {
        let link = magnet::parse(&target).map_err(|err| err.to_string())?;
        println!(
            "magnet {} — {} trackers, fetching metadata",
            hex::encode(link.info_hash),
            link.trackers.len()
        );
        let peer_id = ftc_core::session::make_peer_id();
        let info = ftc_core::metadata::fetch(
            &link.trackers,
            &link.info_hash,
            &peer_id,
            options.port,
            options.use_dht,
        )
        .map_err(|err| format!("metadata exchange failed: {err}"))?;
        Torrent::from_info(&info, &link.trackers).map_err(|err| err.to_string())?
    } else {
        let bytes = std::fs::read(&target).map_err(|err| format!("cannot read {target}: {err}"))?;
        Torrent::parse(&bytes).map_err(|err| err.to_string())?
    };

    println!(
        "{} — {} files, {} bytes, {} pieces",
        torrent.name,
        torrent.files.len(),
        torrent.total_length,
        torrent.piece_count()
    );

    // The Session takes ownership of the torrent and the options; "--seed"
    // needs those after the download, so they are captured by value first.
    let seed_setup = if seed_secs > 0 {
        Some((Arc::new(torrent.clone()), options.port, options.max_peers))
    } else {
        None
    };

    let (session, progress) =
        Session::open(torrent, &output, options).map_err(|err| err.to_string())?;

    let stop = Arc::new(AtomicBool::new(false));
    let reporter = {
        let stop = stop.clone();
        let progress = progress.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let snapshot = progress.snapshot();
                print_progress(&snapshot, progress.bytes_per_second());
                std::thread::sleep(Duration::from_millis(500));
            }
            println!();
        })
    };

    let result = session.run(peers);
    stop.store(true, Ordering::Relaxed);
    let _ = reporter.join();

    match result {
        Ok(stats) => {
            println!(
                "done: {} bytes in {:.1}s ({} resumed pieces)",
                stats.downloaded,
                stats.elapsed.as_secs_f64(),
                stats.resumed
            );
            if let Some((seed_torrent, seed_port, seed_max_peers)) = seed_setup {
                // A fresh Storage over the finished files; serve() refuses
                // to offer anything it cannot re-verify on disk.
                let storage = match ftc_core::storage::Storage::open(&output, &seed_torrent) {
                    Ok(storage) => storage,
                    Err(err) => {
                        eprintln!("ftc: could not open files for seeding: {err}");
                        return Ok(());
                    }
                };
                match ftc_core::seed::serve(
                    Arc::new(storage),
                    seed_torrent,
                    seed_port,
                    seed_max_peers,
                    Duration::from_secs(seed_secs),
                ) {
                    Ok(report) => {
                        println!("seeded {} bytes to {} peer(s)", report.bytes, report.peers)
                    }
                    Err(err) => eprintln!("ftc: seeding failed: {err}"),
                }
            }
            Ok(())
        }
        Err(err) => Err(err.to_string()),
    }
}

fn run_server(args: &[String], gui: bool) -> Result<(), String> {
    let mut bind = String::from("127.0.0.1");
    // A double-clicked GUI must not collide with something already on 8080.
    let mut port: u16 = if gui { 0 } else { 8080 };
    let mut open = gui;
    let mut output = default_output(gui);
    let mut options = Options::default();
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "-h" | "--help" => {
                print!("{SERVE_USAGE}");
                return Ok(());
            }
            "--bind" => {
                bind = args
                    .get(index + 1)
                    .ok_or("--bind needs an address")?
                    .clone();
                index += 2;
            }
            "--port" => {
                port = args
                    .get(index + 1)
                    .ok_or("--port needs a number")?
                    .parse()
                    .map_err(|err| format!("bad --port: {err}"))?;
                index += 2;
            }
            "-o" | "--output" => {
                output = PathBuf::from(args.get(index + 1).ok_or("--output needs a directory")?);
                index += 2;
            }
            "--max-peers" => {
                options.max_peers = args
                    .get(index + 1)
                    .ok_or("--max-peers needs a number")?
                    .parse()
                    .map_err(|err| format!("bad --max-peers: {err}"))?;
                if options.max_peers == 0 {
                    return Err("--max-peers must be at least 1".into());
                }
                index += 2;
            }
            "--no-dht" => {
                options.use_dht = false;
                index += 1;
            }
            "--limit" => {
                let value = args.get(index + 1).ok_or("--limit needs a rate")?.clone();
                options.limit_bps = Some(parse_rate(&value)?);
                index += 2;
            }
            "--open" => {
                open = true;
                index += 1;
            }
            other => return Err(format!("unknown serve option {other}\n\n{SERVE_USAGE}")),
        }
    }

    let server = serve::Server::bind(&bind, port, output.clone(), options)
        .map_err(|err| format!("cannot listen on {bind}:{port}: {err}"))?;
    let addr = server
        .addr()
        .map_err(|err| format!("cannot read the listening address: {err}"))?;
    let url = format!("http://{addr}");
    println!("ftc web ui -> {url}");
    println!("downloads -> {}", output.display());
    if open {
        open_browser(&url);
    } else {
        println!("Ctrl-C to stop");
    }
    server.serve().map_err(|err| err.to_string())
}

/// `2M`, `500k`, `1048576` — a rate in bytes per second, 1024-based.
pub(crate) fn parse_rate(value: &str) -> Result<u64, String> {
    let trimmed = value.trim();
    let (digits, multiplier) = match trimmed.chars().last() {
        Some('k' | 'K') => (&trimmed[..trimmed.len() - 1], 1024u64),
        Some('m' | 'M') => (&trimmed[..trimmed.len() - 1], 1024 * 1024),
        Some('g' | 'G') => (&trimmed[..trimmed.len() - 1], 1024 * 1024 * 1024),
        _ => (trimmed, 1),
    };
    let number: u64 = digits
        .parse()
        .map_err(|_| format!("bad rate {value:?} — try 500k, 2M or 1048576"))?;
    number
        .checked_mul(multiplier)
        .filter(|rate| *rate > 0)
        .ok_or_else(|| format!("bad rate {value:?} — must be at least 1 byte/s"))
}

/// Where a double-clicked GUI should put files: never the working directory,
/// which for a desktop launcher is `/` on macOS.
fn default_output(gui: bool) -> PathBuf {
    if !gui {
        return PathBuf::from(".");
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let Some(home) = home else {
        return PathBuf::from(".");
    };
    let downloads = PathBuf::from(&home).join("Downloads");
    if downloads.is_dir() {
        downloads
    } else {
        PathBuf::from(&home)
    }
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(url);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", "", url]);
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(url);
        command
    };

    match command.status() {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("ftc: could not open a browser ({status}) — open {url}"),
        Err(err) => eprintln!("ftc: could not open a browser ({err}) — open {url}"),
    }
}

fn percent_of(snapshot: &Snapshot) -> u32 {
    if snapshot.total == 0 {
        return 100;
    }
    ((snapshot.downloaded as f64 / snapshot.total as f64) * 100.0) as u32
}

fn print_progress(snapshot: &Snapshot, speed: f64) {
    let percent = percent_of(snapshot);
    let bar_width = 30usize;
    let filled = (percent as usize * bar_width / 100).min(bar_width);
    let bar: String = "█".repeat(filled) + &"░".repeat(bar_width - filled);
    print!(
        "\r[{bar}] {percent:3}%  {}/{}  {:.1} MiB/s  {} peers",
        snapshot.verified,
        snapshot.pieces,
        speed / (1024.0 * 1024.0),
        snapshot.active_peers
    );
    use std::io::Write;
    let _ = std::io::stdout().flush();
}
