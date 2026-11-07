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
    -h, --help            show this help
    -V, --version         show the version
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
    let mut args = std::env::args().skip(1);
    let mut target: Option<String> = None;
    let mut output = PathBuf::from(".");
    let mut peers: Option<Vec<SocketAddr>> = None;
    let mut options = Options::default();

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
        let info =
            ftc_core::metadata::fetch(&link.trackers, &link.info_hash, &peer_id, options.port)
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
            Ok(())
        }
        Err(err) => Err(err.to_string()),
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
