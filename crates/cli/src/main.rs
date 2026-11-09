mod http;
mod serve;

use ftc_core::session::Options;
use ftc_core::{magnet, Session, Snapshot, Torrent};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

const USAGE: &str = "\
Footorrent — a BitTorrent client in Rust (command: ftc)

USAGE:
    ftc <file.torrent | magnet:?xt=urn:btih:...> [options]

OPTIONS:
    -o, --output <dir>    where files are written (default: current directory)
        --peers <list>    comma-separated host:port peers, skipping the tracker
        --max-peers <n>   simultaneous peer connections (default: 6)
        --no-dht          skip DHT peer discovery (trackers only)
        --port <n>        port to announce over trackers and the DHT (default: 6881)
        --seed <secs>     after finishing, serve pieces to peers for <secs>s
        --limit <rate>    cap the download rate, e.g. 500k or 2M (default: unlimited)
    -h, --help            show this help
    -V, --version         show the version

SUBCOMMANDS:
    swarm <magnet|file>   report tracker, DHT and reachable peer counts
    serve                 run the web ui on http://127.0.0.1:8080
    gui                   start the web ui and open it in your browser
";

const SWARM_USAGE: &str = "\
ftc swarm — is this swarm alive?

USAGE:
    ftc swarm <magnet:?xt=urn:btih:... | file.torrent> [options]

OPTIONS:
        --port <n>        port to advertise to trackers and the DHT (default: 6881)
        --no-dht          trackers only
    -h, --help            show this help

Prints how many trackers answered, how many unique peers were discovered, and
how many of them accept a TCP connection. A high peer count with zero reachable
means the swarm is dead, not that the client is broken.
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
        --swarm-port <n>  port for trackers and the DHT (default: 6881)
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
        Some("swarm") => return run_swarm(&argv[1..]),
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
                println!("Footorrent {} (ftc)", env!("CARGO_PKG_VERSION"));
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
            "--port" => {
                options.port = args
                    .next()
                    .ok_or("--port needs a number")?
                    .parse()
                    .map_err(|err| format!("bad --port: {err}"))?;
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

        // While the metadata exchange blocks, an animated sweep owns the next
        // line — the terminal twin of the web ui's indeterminate metadata bar.
        let sweep_stop = Arc::new(AtomicBool::new(false));
        let sweep = if colors_enabled() {
            Some({
                let stop = sweep_stop.clone();
                std::thread::spawn(move || {
                    use std::io::Write;
                    let mut tick = 0;
                    while !stop.load(Ordering::Relaxed) {
                        print_metadata_sweep(tick);
                        tick += 1;
                        std::thread::sleep(Duration::from_millis(120));
                    }
                    // done with the row the sweep owned
                    print!("\r\x1b[0K");
                    let _ = std::io::stdout().flush();
                })
            })
        } else {
            None
        };

        // The fetch blocks; the sweep keeps the line alive until it returns.
        let torrent = match ftc_core::metadata::fetch(
            &link.trackers,
            &link.info_hash,
            &peer_id,
            options.port,
            options.use_dht,
        ) {
            Ok(info) => {
                sweep_stop.store(true, Ordering::Relaxed);
                if let Some(sweep) = sweep {
                    let _ = sweep.join();
                }
                Torrent::from_info(&info, &link.trackers).map_err(|err| err.to_string())?
            }
            Err(err) => {
                sweep_stop.store(true, Ordering::Relaxed);
                if let Some(sweep) = sweep {
                    let _ = sweep.join();
                }
                return Err(format!("metadata exchange failed: {err}"));
            }
        };
        torrent
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
    let final_state = Arc::new(AtomicU8::new(0));
    let reporter = {
        let stop = stop.clone();
        let state = final_state.clone();
        let progress = progress.clone();
        std::thread::spawn(move || {
            let mut tick = 0;
            while !stop.load(Ordering::Relaxed) {
                let snapshot = progress.snapshot();
                print_progress(
                    &snapshot,
                    progress.bytes_per_second(),
                    bar_state(state.load(Ordering::Relaxed)),
                    tick,
                );
                tick += 1;
                std::thread::sleep(Duration::from_millis(120));
            }
            // the closing frame: one final snapshot in the final state
            let snapshot = progress.snapshot();
            print_progress(
                &snapshot,
                progress.bytes_per_second(),
                bar_state(state.load(Ordering::Relaxed)),
                tick,
            );
            println!();
        })
    };

    let result = session.run(peers);
    final_state.store(if result.is_ok() { 1 } else { 2 }, Ordering::Relaxed);
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

fn run_swarm(args: &[String]) -> Result<(), String> {
    let mut target: Option<String> = None;
    let mut use_dht = true;
    let mut port: u16 = 6881;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{SWARM_USAGE}");
                return Ok(());
            }
            "--no-dht" => use_dht = false,
            "--port" => {
                port = iter
                    .next()
                    .ok_or("--port needs a number")?
                    .parse()
                    .map_err(|err| format!("bad --port: {err}"))?;
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option {other}\n\n{SWARM_USAGE}"))
            }
            other => {
                if target.is_some() {
                    return Err("only one magnet or torrent may be given".into());
                }
                target = Some(other.to_string());
            }
        }
    }

    let target = target.ok_or_else(|| format!("no magnet or torrent given\n\n{SWARM_USAGE}"))?;
    let (info_hash, trackers) = if target.starts_with("magnet:") {
        let link = magnet::parse(&target).map_err(|err| err.to_string())?;
        (link.info_hash, link.trackers)
    } else {
        let bytes = std::fs::read(&target).map_err(|err| format!("cannot read {target}: {err}"))?;
        let torrent = Torrent::parse(&bytes).map_err(|err| err.to_string())?;
        (torrent.info_hash, torrent.trackers.clone())
    };

    let hash = hex::encode(info_hash);
    println!("Footorrent swarm report — {hash}");
    println!("  announce list     {} tracker(s)", trackers.len());
    println!("  asking trackers and the DHT (this can take a minute)…");
    let peer_id = ftc_core::session::make_peer_id();
    let report = ftc_core::metadata::swarm(&trackers, &info_hash, &peer_id, port, use_dht);
    println!("  trackers answered {}", report.trackers_ok);
    if report.fallback_trackers {
        println!("  own list dead     yes — public trackers were used");
    }
    for error in report.tracker_errors.iter().take(6) {
        println!("    ! {error}");
    }
    println!("  DHT peers         {}", report.dht_peers);
    println!("  unique peers      {}", report.peers.len());
    println!("  reachable (2 s)   {}", report.reachable);
    Ok(())
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
            "--swarm-port" => {
                options.port = args
                    .get(index + 1)
                    .ok_or("--swarm-port needs a number")?
                    .parse()
                    .map_err(|err| format!("bad --swarm-port: {err}"))?;
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

// ---------- terminal bar — mirrors the web ui's `.prog` bar -----------------

const BAR_WIDTH: usize = 30;

// the exact tokens of the web ui: active blue gradient, done green, failed
// red; the track dots; the dim separators.
const BLUE_L: (u8, u8, u8) = (0x4c, 0x86, 0xbc);
const BLUE_D: (u8, u8, u8) = (0x2f, 0x6f, 0xa8);
const GREEN_D: (u8, u8, u8) = (0x2e, 0x7d, 0x5b);
const GREEN_L: (u8, u8, u8) = (0x4f, 0xa3, 0x7c);
const RED_D: (u8, u8, u8) = (0x9c, 0x4a, 0x42);
const RED_L: (u8, u8, u8) = (0xc0, 0x6a, 0x60);
const TRACK: (u8, u8, u8) = (0x84, 0x92, 0xa0);
const DIM: (u8, u8, u8) = (0x84, 0x92, 0xa0);

const SPINNER: [&str; 10] = [
    "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{287c}", "\u{2874}", "\u{2866}", "\u{2867}",
    "\u{2887}", "\u{288f}",
];

// eighth-block steps: the fill grows inside a cell, like the web ui's
// `width` transition.
const PARTIALS: [&str; 8] = [
    "", "\u{258f}", "\u{258e}", "\u{258d}", "\u{258c}", "\u{258b}", "\u{258a}", "\u{2589}",
];

fn colors_enabled() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && !matches!(std::env::var("TERM").as_deref(), Ok("dumb"))
        && (cfg!(not(target_os = "windows"))
            || std::env::var_os("WT_SESSION").is_some()
            || std::env::var("ConEmuANSI").is_ok_and(|v| v == "ON"))
}

#[derive(Clone, Copy)]
enum BarState {
    Active,
    Done,
    Failed,
}

fn bar_state(raw: u8) -> BarState {
    match raw {
        1 => BarState::Done,
        2 => BarState::Failed,
        _ => BarState::Active,
    }
}

fn mix(a: (u8, u8, u8), b: (u8, u8, u8), t: f64) -> String {
    let ch: fn(f64) -> u8 = |x: f64| x.round() as u8;
    format!(
        "\x1b[38;2;{};{};{}m",
        ch(a.0 as f64 + (b.0 as f64 - a.0 as f64) * t),
        ch(a.1 as f64 + (b.1 as f64 - a.1 as f64) * t),
        ch(a.2 as f64 + (b.2 as f64 - a.2 as f64) * t),
    )
}

// the web ui's byte formatting: B / KB / MB / GB, one decimal below 100
fn fmt_bytes(mut value: f64) -> String {
    if value < 1.0 {
        return "0 B".into();
    }
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut index = 0;
    while value >= 1024.0 && index < UNITS.len() - 1 {
        value /= 1024.0;
        index += 1;
    }
    let digits = if index == 0 || value >= 100.0 { 0 } else { 1 };
    format!("{:.*} {}", digits, value, UNITS[index])
}

fn fmt_speed(value: f64) -> String {
    if value > 0.5 {
        format!("{}/s", fmt_bytes(value))
    } else {
        "—".into()
    }
}

fn fmt_eta(remaining: f64, speed: f64) -> String {
    if speed <= 0.5 {
        return "—".into();
    }
    let seconds = (remaining / speed).round() as u64;
    if seconds > 86_400 {
        return ">1d".into();
    }
    if seconds >= 3_600 {
        return format!("{}h {:02}m", seconds / 3_600, (seconds % 3_600) / 60);
    }
    if seconds >= 60 {
        return format!("{}m {:02}s", seconds / 60, seconds % 60);
    }
    format!("{}s", seconds.max(1))
}

fn print_progress(snapshot: &Snapshot, speed: f64, state: BarState, tick: usize) {
    use std::io::Write;
    let colors = colors_enabled();
    let finished = snapshot.pieces > 0 && snapshot.verified >= snapshot.pieces;
    let percent = if finished { 100 } else { percent_of(snapshot) };
    let (left, right, marker, label) = match state {
        BarState::Active => (BLUE_L, BLUE_D, SPINNER[tick % SPINNER.len()], ""),
        BarState::Done => (GREEN_D, GREEN_L, "\u{2713}", "done"),
        BarState::Failed => (RED_D, RED_L, "\u{2715}", "failed"),
    };

    let mut line = String::new();
    if colors {
        line += &mix(left, right, 0.0);
    }
    line += marker;
    if colors {
        line += "\x1b[0m";
    }
    line.push(' ');

    if colors {
        let exact = (percent as f64 / 100.0) * BAR_WIDTH as f64;
        let mut filled = exact.floor() as usize;
        let mut frac = ((exact - filled as f64) * 8.0).round() as usize;
        if frac == 8 {
            filled += 1;
            frac = 0;
        }
        let filled = filled.min(BAR_WIDTH);
        // the gradient spans the fill's own width, exactly like `linear-gradient`
        // on the web ui's fill element
        let span = (exact - 1.0).max(0.9);
        for i in 0..filled {
            line += &mix(left, right, (i as f64 / span).min(1.0));
            line.push('\u{2588}');
            line += "\x1b[0m";
        }
        if filled < BAR_WIDTH && frac > 0 {
            line += &mix(left, right, (filled as f64 / span).min(1.0));
            line += PARTIALS[frac];
            line += "\x1b[0m";
        }
        if filled < BAR_WIDTH || frac > 0 {
            line += &mix(TRACK, TRACK, 0.0);
            let painted = filled + usize::from(filled < BAR_WIDTH && frac > 0);
            for _ in painted..BAR_WIDTH {
                line.push('\u{2591}');
            }
            line += "\x1b[0m";
        }
    } else {
        let filled = (percent as usize * BAR_WIDTH / 100).min(BAR_WIDTH);
        line.push('[');
        for i in 0..BAR_WIDTH {
            line.push(if i < filled { '\u{2588}' } else { '\u{2591}' });
        }
        line.push(']');
    }

    // the stats to the bar's right, in the web ui's own units
    let stats = match state {
        BarState::Active => {
            let remaining = snapshot.total.saturating_sub(snapshot.downloaded) as f64;
            let eta = fmt_eta(remaining, speed);
            format!(
                " · {} · eta {} · {} peers",
                fmt_speed(speed),
                eta,
                snapshot.active_peers
            )
        }
        BarState::Done => format!(" · {} · {}", label, fmt_bytes(snapshot.total as f64)),
        BarState::Failed => format!(" · {}", label),
    };
    if colors {
        for part in stats.split(" · ") {
            if part.is_empty() {
                continue;
            }
            line += &mix(DIM, DIM, 0.0);
            line += " · ";
            line += "\x1b[0m";
            line += part;
        }
    } else {
        line += &stats;
    }

    print!("\r{line}\x1b[0K");
    let _ = std::io::stdout().flush();
}

// indeterminate metadata bar: a 40%-wide blue window sweeps the track, the
// terminal twin of the web ui's `metadata` state.
fn print_metadata_sweep(tick: usize) {
    use std::io::Write;
    let window = (BAR_WIDTH * 2 / 5).max(1);
    let cycle = BAR_WIDTH + window;
    let pos = (tick % cycle) as i64 - window as i64;
    let mut line = String::new();
    line += &mix(BLUE_L, BLUE_D, 0.0);
    line += SPINNER[tick % SPINNER.len()];
    line += "\x1b[0m ";
    for i in 0..BAR_WIDTH {
        let inside = pos <= i as i64 && (i as i64) < pos + window as i64;
        if inside {
            let t = if window > 1 {
                (i as i64 - pos) as f64 / (window - 1) as f64
            } else {
                0.0
            };
            line += &mix(BLUE_L, BLUE_D, t);
            line.push('\u{2588}');
        } else {
            line += &mix(TRACK, TRACK, 0.0);
            line.push('\u{2591}');
        }
        line += "\x1b[0m";
    }
    line += &mix(DIM, DIM, 0.0);
    line += " · fetching metadata\x1b[0m";
    print!("\r{line}\x1b[0K");
    let _ = std::io::stdout().flush();
}
