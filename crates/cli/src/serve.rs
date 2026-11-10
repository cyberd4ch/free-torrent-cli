//! `ftc serve` — the download manager UI and its JSON API.
//!
//! One thread per connection, one thread per download. Jobs hold the same
//! `Arc<Progress>` the CLI prints from, so the UI polls real counters instead of
//! a second bookkeeping layer that could disagree with the engine.

use crate::http::{self, Request};
use crate::parse_rate;
use ftc_core::error::Error;
use ftc_core::magnet;
use ftc_core::session::{make_peer_id, Options, PeerView};
use ftc_core::{Progress, Session, Torrent};
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const UI: &str = include_str!("ui.html");

/// Served at `/favicon.ico` so a browser never logs a failed icon request.
///
/// The same mark as `assets/brand/footorrent-mark.svg`: cream tile, blue
/// footprint, arrow carved in the tile colour. Keep the two in step.
const ICON: &str = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'>\
<rect x='.5' y='.5' width='31' height='31' rx='7' fill='#f1e9da' stroke='#d2c5ae'/>\
<g fill='#2f6fa8'>\
<circle cx='10' cy='9' r='2.1'/><circle cx='15' cy='7' r='2.3'/><circle cx='20' cy='7.6' r='2.1'/>\
<circle cx='24' cy='10' r='1.8'/>\
<path d='M11.5 13c2.6-1.2 6.6-1.2 9.1 0 1.7.8 2.3 2.7 1.8 5.1-.6 3-1.8 4.9-3.7 6.3-.9.7-1.4.7-2.3 0-1.9-1.4-3.1-3.3-3.7-6.3-.5-2.4.1-4.3 1.8-5.1z'/>\
</g>\
<path d='M16 15.5v5m0 0-2.6-2.6M16 20.5l2.6-2.6' fill='none' stroke='#f1e9da' stroke-width='1.8' stroke-linecap='round' stroke-linejoin='round'/></svg>";

pub struct Server {
    listener: TcpListener,
    state: Arc<State>,
}

struct State {
    jobs: Mutex<Vec<Job>>,
    /// Resolved magnets waiting for the user to confirm a destination.
    pending: Mutex<Vec<Pending>>,
    next_id: AtomicU64,
    settings: Mutex<Settings>,
    options: Options,
}

/// Everything the UI may change at runtime. Networking options are fixed at
/// startup; these two are the ones a download manager exposes as preferences.
#[derive(Clone)]
struct Settings {
    output: PathBuf,
    limit: Option<u64>,
}

struct Job {
    id: u64,
    target: String,
    name: String,
    output: PathBuf,
    status: &'static str,
    total: u64,
    limit: Option<u64>,
    added: u64,
    finished: Option<u64>,
    progress: Option<Arc<Progress>>,
    stop: Option<Arc<AtomicBool>>,
    pause: Option<Arc<AtomicBool>>,
    error: Option<String>,
}

/// A magnet whose metadata has been fetched, but whose destination the user has
/// not confirmed yet. Nothing is written to disk until `/api/start` runs.
struct Pending {
    token: u64,
    /// The original magnet, kept so Retry can re-add the job after a stop.
    target: String,
    torrent: Torrent,
    /// Peers the metadata exchange already reached. Handing them to the
    /// session matters when the magnet's own trackers are dead: re-announcing
    /// to that same list found nobody and the download sat at zero.
    peers: Vec<SocketAddr>,
}

impl Server {
    pub fn bind(bind: &str, port: u16, output: PathBuf, options: Options) -> io::Result<Server> {
        let listener = TcpListener::bind((bind, port))?;
        Ok(Server {
            listener,
            state: Arc::new(State {
                jobs: Mutex::new(Vec::new()),
                pending: Mutex::new(Vec::new()),
                next_id: AtomicU64::new(1),
                settings: Mutex::new(Settings {
                    output,
                    limit: options.limit_bps,
                }),
                options,
            }),
        })
    }

    pub fn addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub fn serve(self) -> io::Result<()> {
        for incoming in self.listener.incoming() {
            let Ok(stream) = incoming else { continue };
            let state = self.state.clone();
            std::thread::spawn(move || handle(stream, state));
        }
        Ok(())
    }
}

fn handle(mut stream: TcpStream, state: Arc<State>) {
    let request = match Request::read(&stream) {
        Ok(request) => request,
        Err(err) => {
            let _ = http::respond(
                &mut stream,
                400,
                "text/plain; charset=utf-8",
                err.to_string().as_bytes(),
            );
            return;
        }
    };
    let (status, content_type, body) = route(&state, &request);
    let _ = http::respond(&mut stream, status, content_type, &body);
}

fn route(state: &Arc<State>, request: &Request) -> (u16, &'static str, Vec<u8>) {
    const JSON: &str = "application/json";
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => (200, "text/html; charset=utf-8", UI.as_bytes().to_vec()),
        ("GET", "/api/state") => (200, JSON, state_json(state).into_bytes()),
        ("GET", "/api/settings") => (200, JSON, settings_json(state).into_bytes()),
        ("POST", "/api/settings") => change_settings(state, &request.body),
        ("POST", "/api/add") => add(state, &request.body),
        ("POST", "/api/resolve") => resolve_target(state, &request.body),
        ("POST", "/api/start") => start_pending(state, &request.body),
        ("POST", "/api/discard") => discard_pending(state, &request.body),
        ("POST", "/api/add-torrent") => add_torrent(state, &request.body),
        ("POST", "/api/cancel") => cancel(state, &request.body),
        ("POST", "/api/pause") => pause(state, &request.body, true),
        ("POST", "/api/resume") => pause(state, &request.body, false),
        ("POST", "/api/remove") => remove(state, &request.body),
        ("POST", "/api/retry") => retry(state, &request.body),
        ("POST", "/api/reveal") => reveal(state, &request.body),
        (_, "/favicon.ico") => (200, "image/svg+xml", ICON.as_bytes().to_vec()),
        (_, "/api/add")
        | (_, "/api/add-torrent")
        | (_, "/api/resolve")
        | (_, "/api/start")
        | (_, "/api/discard")
        | (_, "/api/cancel")
        | (_, "/api/pause")
        | (_, "/api/resume")
        | (_, "/api/remove")
        | (_, "/api/retry")
        | (_, "/api/reveal")
        | (_, "/api/settings")
        | (_, "/api/state") => (
            405,
            "text/plain; charset=utf-8",
            b"method not allowed".to_vec(),
        ),
        _ => (404, "text/plain; charset=utf-8", b"not found".to_vec()),
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn bad(message: &str) -> (u16, &'static str, Vec<u8>) {
    (
        400,
        "text/plain; charset=utf-8",
        message.as_bytes().to_vec(),
    )
}

fn json(status: u16, body: String) -> (u16, &'static str, Vec<u8>) {
    (status, "application/json", body.into_bytes())
}

fn field(form: &[(String, String)], name: &str) -> Option<String> {
    form.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Which jobs a button press applies to: one id, an explicit list, or everything.
struct Scope {
    all: bool,
    ids: Vec<u64>,
}

impl Scope {
    fn covers(&self, id: u64) -> bool {
        self.all || self.ids.contains(&id)
    }

    /// A single explicit id gets precise errors (404/400); batches tolerate jobs
    /// that do not apply so "pause all" does not fail because one job finished.
    fn is_strict(&self) -> bool {
        !self.all && self.ids.len() == 1
    }
}

fn scope(form: &[(String, String)]) -> Result<Scope, (u16, &'static str, Vec<u8>)> {
    if field(form, "all").is_some_and(|value| value == "1" || value == "true") {
        return Ok(Scope {
            all: true,
            ids: Vec::new(),
        });
    }
    if let Some(list) = field(form, "ids") {
        let ids: Vec<u64> = list
            .split(',')
            .filter_map(|part| part.trim().parse().ok())
            .collect();
        if ids.is_empty() {
            return Err(bad("ids needs at least one number"));
        }
        return Ok(Scope { all: false, ids });
    }
    match field(form, "id").and_then(|value| value.parse::<u64>().ok()) {
        Some(id) => Ok(Scope {
            all: false,
            ids: vec![id],
        }),
        None => Err(bad("missing id")),
    }
}

fn is_terminal(status: &str) -> bool {
    matches!(status, "done" | "error" | "cancelled")
}

fn add(state: &Arc<State>, body: &str) -> (u16, &'static str, Vec<u8>) {
    let form = http::form_pairs(body);

    let Some(target) = field(&form, "target") else {
        return bad("missing target");
    };
    let settings = state.settings.lock().unwrap().clone();
    let output = field(&form, "output")
        .map(PathBuf::from)
        .unwrap_or(settings.output);
    // A browser hands over the directory the user typed; make it instead of
    // failing the whole download because it did not exist yet.
    if let Err(err) = std::fs::create_dir_all(&output) {
        return bad(&format!("cannot create {}: {err}", output.display()));
    }

    // An unparseable cap is a typo, not a reason to start an unlimited job.
    let limit = match field(&form, "limit") {
        Some(value) => match parse_rate(&value) {
            Ok(rate) => Some(rate),
            Err(message) => return bad(&message),
        },
        None => settings.limit,
    };
    let paused = matches!(field(&form, "paused").as_deref(), Some("1" | "true" | "on"));

    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    // A magnet's display name lets the row read as a title while the metadata
    // is still being fetched, instead of showing 300 characters of URI.
    let display_name = magnet::parse(&target)
        .ok()
        .and_then(|link| link.name)
        .unwrap_or_default();
    state.jobs.lock().unwrap().push(Job {
        id,
        target: target.clone(),
        name: display_name,
        output: output.clone(),
        status: if paused { "paused" } else { "metadata" },
        total: 0,
        limit,
        added: now_secs(),
        finished: None,
        progress: None,
        stop: None,
        pause: None,
        error: None,
    });
    spawn_download(state.clone(), id, target, output, limit, paused);
    json(200, format!("{{\"id\":{id}}}"))
}

/// A `.torrent` dropped on the page: the browser cannot hand us a filesystem
/// path, so it base64-encodes the bytes and we park them in the temp folder long
/// enough for the worker to parse them.
fn add_torrent(state: &Arc<State>, body: &str) -> (u16, &'static str, Vec<u8>) {
    let form = http::form_pairs(body);
    let (Some(name), Some(data)) = (field(&form, "name"), field(&form, "data")) else {
        return bad("missing name or data");
    };
    let Some(bytes) = base64_decode(&data) else {
        return bad("data is not base64");
    };
    if bytes.is_empty() {
        return bad("the torrent is empty");
    }
    if bytes.len() > 8 * 1024 * 1024 {
        return bad("the torrent is larger than 8 MB");
    }

    let settings = state.settings.lock().unwrap().clone();
    let output = field(&form, "output")
        .map(PathBuf::from)
        .unwrap_or(settings.output);
    if let Err(err) = std::fs::create_dir_all(&output) {
        return bad(&format!("cannot create {}: {err}", output.display()));
    }
    let limit = match field(&form, "limit") {
        Some(value) => match parse_rate(&value) {
            Ok(rate) => Some(rate),
            Err(message) => return bad(&message),
        },
        None => settings.limit,
    };
    let paused = matches!(field(&form, "paused").as_deref(), Some("1" | "true" | "on"));

    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    let safe_name: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let safe_name = if safe_name.trim_matches('.').is_empty() {
        "upload.torrent".to_string()
    } else {
        safe_name
    };

    let uploads = std::env::temp_dir().join("ftc-uploads");
    if let Err(err) = std::fs::create_dir_all(&uploads) {
        return bad(&format!("cannot store the upload: {err}"));
    }
    let path = uploads.join(format!("{id}-{safe_name}"));
    if let Err(err) = std::fs::write(&path, &bytes) {
        return bad(&format!("cannot store the upload: {err}"));
    }

    state.jobs.lock().unwrap().push(Job {
        id,
        target: path.display().to_string(),
        name: String::new(),
        output: output.clone(),
        status: if paused { "paused" } else { "metadata" },
        total: 0,
        limit,
        added: now_secs(),
        finished: None,
        progress: None,
        stop: None,
        pause: None,
        error: None,
    });
    spawn_download(
        state.clone(),
        id,
        path.display().to_string(),
        output,
        limit,
        paused,
    );
    json(200, format!("{{\"id\":{id}}}"))
}

fn cancel(state: &Arc<State>, body: &str) -> (u16, &'static str, Vec<u8>) {
    let form = http::form_pairs(body);
    let scope = match scope(&form) {
        Ok(scope) => scope,
        Err(error) => return error,
    };
    let mut jobs = state.jobs.lock().unwrap();
    if scope.is_strict() && !jobs.iter().any(|job| job.id == scope.ids[0]) {
        return (404, "text/plain; charset=utf-8", b"no such job".to_vec());
    }
    let mut affected = 0;
    let mut skipped = 0;
    for job in jobs.iter_mut() {
        if !scope.covers(job.id) {
            continue;
        }
        if is_terminal(job.status) {
            if scope.is_strict() {
                return bad("job already finished");
            }
            skipped += 1;
            continue;
        }
        // Metadata exchange cannot be interrupted; marking the job cancelled
        // makes the worker drop whatever comes back.
        if let Some(stop) = &job.stop {
            stop.store(true, Ordering::Relaxed);
        }
        job.status = "cancelled";
        job.finished = Some(now_secs());
        affected += 1;
    }
    json(
        200,
        format!("{{\"affected\":{affected},\"skipped\":{skipped}}}"),
    )
}

fn pause(state: &Arc<State>, body: &str, paused: bool) -> (u16, &'static str, Vec<u8>) {
    let form = http::form_pairs(body);
    let scope = match scope(&form) {
        Ok(scope) => scope,
        Err(error) => return error,
    };
    let mut jobs = state.jobs.lock().unwrap();
    if scope.is_strict() && !jobs.iter().any(|job| job.id == scope.ids[0]) {
        return (404, "text/plain; charset=utf-8", b"no such job".to_vec());
    }
    let mut affected = 0;
    let mut skipped = 0;
    for job in jobs.iter_mut() {
        if !scope.covers(job.id) {
            continue;
        }
        if is_terminal(job.status) {
            if scope.is_strict() {
                return bad("job already finished");
            }
            skipped += 1;
            continue;
        }
        let Some(flag) = job.pause.clone() else {
            if scope.is_strict() {
                return bad("job is still resolving metadata");
            }
            skipped += 1;
            continue;
        };
        flag.store(paused, Ordering::Relaxed);
        job.status = if paused { "paused" } else { "downloading" };
        affected += 1;
    }
    json(
        200,
        format!("{{\"affected\":{affected},\"skipped\":{skipped},\"paused\":{paused}}}"),
    )
}

fn remove(state: &Arc<State>, body: &str) -> (u16, &'static str, Vec<u8>) {
    let form = http::form_pairs(body);
    let everything_finished = field(&form, "finished").is_some_and(|value| value == "1");
    let scope = if everything_finished {
        Scope {
            all: true,
            ids: Vec::new(),
        }
    } else {
        match scope(&form) {
            Ok(scope) => scope,
            Err(error) => return error,
        }
    };

    let mut jobs = state.jobs.lock().unwrap();
    if scope.is_strict() {
        let Some(job) = jobs.iter().find(|job| job.id == scope.ids[0]) else {
            return (404, "text/plain; charset=utf-8", b"no such job".to_vec());
        };
        if !is_terminal(job.status) {
            return bad("only stopped or finished downloads can be removed");
        }
    }

    let dropped: Vec<String> = jobs
        .iter()
        .filter(|job| scope.covers(job.id) && is_terminal(job.status))
        .map(|job| job.target.clone())
        .collect();
    let before = jobs.len();
    jobs.retain(|job| !(scope.covers(job.id) && is_terminal(job.status)));
    let removed = before - jobs.len();
    // Uploads live until their job is removed; Retry needs the file to still exist.
    for target in dropped {
        remove_upload(&target);
    }
    if scope.is_strict() && removed == 0 {
        return bad("only stopped or finished downloads can be removed");
    }
    json(200, format!("{{\"removed\":{removed}}}"))
}

fn retry(state: &Arc<State>, body: &str) -> (u16, &'static str, Vec<u8>) {
    let form = http::form_pairs(body);
    let id = match field(&form, "id").and_then(|value| value.parse::<u64>().ok()) {
        Some(id) => id,
        None => return bad("missing id"),
    };

    let (target, output, limit) = {
        let mut jobs = state.jobs.lock().unwrap();
        let Some(job) = jobs.iter_mut().find(|job| job.id == id) else {
            return (404, "text/plain; charset=utf-8", b"no such job".to_vec());
        };
        // A paused job has not failed: Retry is the natural resume, and doing it
        // in place avoids a second job racing over the same files.
        if job.status == "paused" {
            if let Some(flag) = &job.pause {
                flag.store(false, Ordering::Relaxed);
            }
            job.status = "downloading";
            return json(
                200,
                format!("{{\"id\":{id},\"affected\":1,\"resumed\":true}}"),
            );
        }
        if !is_terminal(job.status) {
            return bad("only paused, stopped or failed downloads can be retried");
        }
        (job.target.clone(), job.output.clone(), job.limit)
    };

    let new_id = state.next_id.fetch_add(1, Ordering::Relaxed);
    state.jobs.lock().unwrap().push(Job {
        id: new_id,
        target: target.clone(),
        name: String::new(),
        output: output.clone(),
        status: "metadata",
        total: 0,
        limit,
        added: now_secs(),
        finished: None,
        progress: None,
        stop: None,
        pause: None,
        error: None,
    });
    spawn_download(state.clone(), new_id, target, output, limit, false);
    json(200, format!("{{\"id\":{new_id}}}"))
}

/// Show the finished file (or its folder) in the desktop file manager. A local
/// UI is the only caller, and the path always comes from a job we created.
fn reveal(state: &Arc<State>, body: &str) -> (u16, &'static str, Vec<u8>) {
    let form = http::form_pairs(body);
    let id = match field(&form, "id").and_then(|value| value.parse::<u64>().ok()) {
        Some(id) => id,
        None => return bad("missing id"),
    };
    let path = {
        let jobs = state.jobs.lock().unwrap();
        let Some(job) = jobs.iter().find(|job| job.id == id) else {
            return (404, "text/plain; charset=utf-8", b"no such job".to_vec());
        };
        if job.name.is_empty() {
            job.output.clone()
        } else {
            job.output.join(&job.name)
        }
    };
    match open_path(&path) {
        Ok(()) => json(
            200,
            format!(
                "{{\"path\":\"{}\"}}",
                http::json_escape(&path.display().to_string())
            ),
        ),
        Err(err) => (
            500,
            "text/plain; charset=utf-8",
            format!("could not open {}: {err}", path.display()).into_bytes(),
        ),
    }
}

fn open_path(path: &Path) -> io::Result<()> {
    use std::process::Command;
    // `open` and `explorer` take the path itself; xdg-open wants the folder, so
    // the fallback is computed only in the branch that uses it (Windows clippy
    // rightly called the shared version an unused variable).
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        if path.is_file() {
            command.arg("-R").arg(path);
        } else {
            command.arg(path);
        }
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("explorer");
        command.arg(format!("/select,{}", path.display()));
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = {
        let dir = if path.is_dir() {
            path.to_path_buf()
        } else {
            path.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| path.to_path_buf())
        };
        let mut command = Command::new("xdg-open");
        command.arg(&dir);
        command
    };
    command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|_| ())
}

/// Uploaded .torrent files live in the temp folder; drop the copy once parsed.
fn remove_upload(target: &str) {
    let uploads = std::env::temp_dir().join("ftc-uploads");
    let path = Path::new(target);
    if path.parent() == Some(uploads.as_path()) {
        let _ = std::fs::remove_file(path);
    }
}

/// Minimal standard-alphabet base64, enough for an uploaded .torrent.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits = 0u8;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return None,
        } as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

fn settings_json(state: &State) -> String {
    let settings = state.settings.lock().unwrap();
    let limit = match settings.limit {
        Some(rate) => rate.to_string(),
        None => "null".to_string(),
    };
    format!(
        "{{\"output\":\"{}\",\"limit\":{limit},\"version\":\"{}\"}}",
        http::json_escape(&settings.output.display().to_string()),
        env!("CARGO_PKG_VERSION")
    )
}

fn change_settings(state: &Arc<State>, body: &str) -> (u16, &'static str, Vec<u8>) {
    let form = http::form_pairs(body);
    if let Some(output) = field(&form, "output") {
        let output = PathBuf::from(output);
        if let Err(err) = std::fs::create_dir_all(&output) {
            return bad(&format!("cannot create {}: {err}", output.display()));
        }
        state.settings.lock().unwrap().output = output;
    }
    if let Some(value) = field(&form, "limit") {
        let limit = if matches!(value.as_str(), "none" | "unlimited" | "0") {
            None
        } else {
            match parse_rate(&value) {
                Ok(rate) => Some(rate),
                Err(message) => return bad(&message),
            }
        };
        state.settings.lock().unwrap().limit = limit;
    }
    json(200, settings_json(state))
}

fn spawn_download(
    state: Arc<State>,
    id: u64,
    target: String,
    output: PathBuf,
    limit: Option<u64>,
    start_paused: bool,
) {
    std::thread::spawn(move || {
        let resolved = resolve(&target, &state.options);
        // Keep an uploaded .torrent around: Retry re-parses the same path, and
        // deleting it here made every retry of an upload fail with "cannot read".
        let torrent = match resolved {
            Ok(torrent) => torrent,
            Err(message) => return finish(&state, id, "error", Some(message)),
        };
        start_torrent(state, id, torrent, output, limit, start_paused, Vec::new());
    });
}

/// First half of the two-step add: contact the swarm, fetch and verify the
/// metadata, and report the name, exact size and file list. The torrent is
/// parked under a token; no directory is created and no byte is written until
/// `/api/start` names a folder.
fn resolve_target(state: &Arc<State>, body: &str) -> (u16, &'static str, Vec<u8>) {
    let form = http::form_pairs(body);
    let Some(target) = field(&form, "target") else {
        return bad("missing target");
    };

    let (torrent, peers, fallback) = if target.starts_with("magnet:") {
        let link = match magnet::parse(&target) {
            Ok(link) => link,
            Err(err) => return bad(&err.to_string()),
        };
        let peer_id = make_peer_id();
        let fetched = match ftc_core::metadata::fetch_all(
            &link.trackers,
            &link.info_hash,
            &peer_id,
            state.options.port,
            state.options.use_dht,
        ) {
            Ok(fetched) => fetched,
            // Include the full hash: the add dialog shortens the magnet in its
            // own message, which made every failure report impossible to look up.
            Err(err) => {
                return bad(&format!(
                    "metadata exchange failed for {}: {err}",
                    hex::encode(link.info_hash)
                ))
            }
        };
        let peers = fetched.peers.clone();
        let fallback = fetched.fallback_trackers;
        match Torrent::from_info(&fetched.info, &link.trackers) {
            Ok(torrent) => (torrent, peers, fallback),
            Err(err) => return bad(&err.to_string()),
        }
    } else {
        let bytes = match std::fs::read(&target) {
            Ok(bytes) => bytes,
            Err(err) => return bad(&format!("cannot read {target}: {err}")),
        };
        match Torrent::parse(&bytes) {
            Ok(torrent) => (torrent, Vec::new(), false),
            Err(err) => return bad(&err.to_string()),
        }
    };

    let token = state.next_id.fetch_add(1, Ordering::Relaxed);
    let body = format!(
        "{{\"token\":{token},\"name\":\"{}\",\"total\":{},\"pieces\":{},\"peers\":{},\"fallback_trackers\":{},\
         \"info_hash\":\"{}\",\"files\":{}}}",
        http::json_escape(&torrent.name),
        torrent.total_length,
        torrent.piece_count(),
        peers.len(),
        fallback,
        hex::encode(torrent.info_hash),
        files_json(&torrent)
    );
    state.pending.lock().unwrap().push(Pending {
        token,
        target: target.clone(),
        torrent,
        peers,
    });
    json(200, body)
}

fn files_json(torrent: &Torrent) -> String {
    let mut out = String::from("[");
    for (index, file) in torrent.files.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"path\":\"{}\",\"length\":{}}}",
            http::json_escape(&file.path.display().to_string()),
            file.length
        ));
    }
    out.push(']');
    out
}

/// Second half: the user picked a folder, so the download may touch the disk.
fn start_pending(state: &Arc<State>, body: &str) -> (u16, &'static str, Vec<u8>) {
    let form = http::form_pairs(body);
    let Some(token) = field(&form, "token").and_then(|value| value.parse::<u64>().ok()) else {
        return bad("missing token");
    };
    let settings = state.settings.lock().unwrap().clone();
    let output = field(&form, "output")
        .map(PathBuf::from)
        .unwrap_or(settings.output);
    if let Err(err) = std::fs::create_dir_all(&output) {
        return bad(&format!("cannot create {}: {err}", output.display()));
    }
    let limit = match field(&form, "limit") {
        Some(value) => match parse_rate(&value) {
            Ok(rate) => Some(rate),
            Err(message) => return bad(&message),
        },
        None => settings.limit,
    };
    let paused = matches!(field(&form, "paused").as_deref(), Some("1" | "true" | "on"));

    let position = state
        .pending
        .lock()
        .unwrap()
        .iter()
        .position(|entry| entry.token == token);
    let Some(position) = position else {
        return (
            404,
            "text/plain; charset=utf-8",
            b"no such pending download".to_vec(),
        );
    };
    let pending = state.pending.lock().unwrap().remove(position);

    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    let name = pending.torrent.name.clone();
    let source = pending.target.clone();
    let total = pending.torrent.total_length;
    state.jobs.lock().unwrap().push(Job {
        id,
        target: source,
        name,
        output: output.clone(),
        status: if paused { "paused" } else { "metadata" },
        total,
        limit,
        added: now_secs(),
        finished: None,
        progress: None,
        stop: None,
        pause: None,
        error: None,
    });
    let peers = pending.peers;
    let peer_count = peers.len();
    let worker_state = state.clone();
    // Run the download on its own thread: this response has to return as soon as
    // the job is registered, not when the last byte lands.
    std::thread::spawn(move || {
        start_torrent(
            worker_state,
            id,
            pending.torrent,
            output,
            limit,
            paused,
            peers,
        );
    });
    json(200, format!("{{\"id\":{id},\"peers\":{peer_count}}}"))
}

fn discard_pending(state: &Arc<State>, body: &str) -> (u16, &'static str, Vec<u8>) {
    let form = http::form_pairs(body);
    let Some(token) = field(&form, "token").and_then(|value| value.parse::<u64>().ok()) else {
        return bad("missing token");
    };
    state
        .pending
        .lock()
        .unwrap()
        .retain(|entry| entry.token != token);
    json(200, "{\"discarded\":true}".to_string())
}

fn start_torrent(
    state: Arc<State>,
    id: u64,
    torrent: Torrent,
    output: PathBuf,
    limit: Option<u64>,
    start_paused: bool,
    peers: Vec<SocketAddr>,
) {
    if is_cancelled(&state, id) {
        return;
    }

    let mut options = state.options.clone();
    options.limit_bps = limit;
    let (session, progress) = match Session::open(torrent, &output, options) {
        Ok(opened) => opened,
        Err(err) => return finish(&state, id, "error", Some(err.to_string())),
    };
    let stop = session.stop_handle();
    let pause = session.pause_handle();
    if start_paused {
        pause.store(true, Ordering::Relaxed);
    }
    let name = session.torrent().name.clone();
    let total = session.torrent().total_length;
    {
        let mut jobs = state.jobs.lock().unwrap();
        let Some(job) = jobs.iter_mut().find(|job| job.id == id) else {
            return;
        };
        if job.status == "cancelled" {
            return;
        }
        job.name = name;
        job.total = total;
        job.progress = Some(progress);
        job.stop = Some(stop);
        job.pause = Some(pause);
        job.status = if start_paused {
            "paused"
        } else {
            "downloading"
        };
    }

    // Use what the resolve already found instead of announcing to the same
    // dead tracker list again.
    let explicit = if peers.is_empty() { None } else { Some(peers) };
    match session.run(explicit) {
        Ok(_) => finish(&state, id, "done", None),
        Err(Error::Cancelled) => finish(&state, id, "cancelled", None),
        Err(err) => finish(&state, id, "error", Some(err.to_string())),
    }
}

fn resolve(target: &str, options: &Options) -> Result<Torrent, String> {
    if target.starts_with("magnet:") {
        let link = magnet::parse(target).map_err(|err| err.to_string())?;
        let peer_id = make_peer_id();
        let info = ftc_core::metadata::fetch(
            &link.trackers,
            &link.info_hash,
            &peer_id,
            options.port,
            options.use_dht,
        )
        .map_err(|err| format!("metadata exchange failed: {err}"))?;
        Torrent::from_info(&info, &link.trackers).map_err(|err| err.to_string())
    } else {
        let bytes = std::fs::read(target).map_err(|err| format!("cannot read {target}: {err}"))?;
        Torrent::parse(&bytes).map_err(|err| err.to_string())
    }
}

fn is_cancelled(state: &State, id: u64) -> bool {
    state
        .jobs
        .lock()
        .unwrap()
        .iter()
        .find(|job| job.id == id)
        .map(|job| job.status == "cancelled")
        .unwrap_or(true)
}

/// Record a terminal state, unless the user already cancelled the job.
fn finish(state: &State, id: u64, status: &'static str, error: Option<String>) {
    let mut jobs = state.jobs.lock().unwrap();
    let Some(job) = jobs.iter_mut().find(|job| job.id == id) else {
        return;
    };
    if job.status == "cancelled" {
        return;
    }
    job.status = status;
    job.error = error;
    job.finished = Some(now_secs());
}

/// One job with the counters the table needs, already resolved.
struct View<'a> {
    job: &'a Job,
    total: u64,
    downloaded: u64,
    pieces: usize,
    verified: usize,
    peers: usize,
    speed: f64,
    elapsed: f64,
    percent: u64,
    path: PathBuf,
    peer_list: Vec<PeerView>,
}

fn view(job: &Job) -> View<'_> {
    let snapshot = job.progress.as_ref().map(|progress| progress.snapshot());
    // Average throughput only means "running" while bytes are moving. A paused,
    // stopped or finished job used to keep reporting its lifetime average, so
    // the status bar looked like a download was still going after Remove.
    let speed = if job.status == "downloading" {
        job.progress
            .as_ref()
            .map(|progress| progress.bytes_per_second())
            .unwrap_or(0.0)
    } else {
        0.0
    };
    let (total, downloaded, pieces, verified, peers, elapsed, peer_list) = match snapshot {
        Some(snapshot) => (
            snapshot.total,
            snapshot.downloaded,
            snapshot.pieces,
            snapshot.verified,
            snapshot.active_peers,
            snapshot.elapsed.as_secs_f64(),
            snapshot.peers,
        ),
        None => (job.total, 0, 0, 0, 0, 0.0, Vec::new()),
    };
    let percent = if job.status == "done" {
        100
    } else {
        downloaded
            .saturating_mul(100)
            .checked_div(total)
            .map_or(0, |value| value.min(100))
    };
    let path = if job.name.is_empty() {
        job.output.clone()
    } else {
        job.output.join(&job.name)
    };
    View {
        job,
        total,
        downloaded,
        pieces,
        verified,
        peers,
        speed,
        elapsed,
        percent,
        path,
        peer_list,
    }
}

fn state_json(state: &State) -> String {
    let settings = state.settings.lock().unwrap().clone();
    let jobs = state.jobs.lock().unwrap();
    let views: Vec<View> = jobs.iter().map(view).collect();

    let mut total_speed = 0.0;
    let mut total_downloaded = 0u64;
    let mut active = 0usize;
    for view in &views {
        total_speed += view.speed;
        total_downloaded += view.downloaded;
        if view.job.status == "downloading" {
            active += 1;
        }
    }

    let limit = match settings.limit {
        Some(rate) => rate.to_string(),
        None => "null".to_string(),
    };
    let mut out = format!(
        "{{\"version\":\"{}\",\"settings\":{{\"output\":\"{}\",\"limit\":{limit}}},\
         \"totals\":{{\"speed\":{total_speed:.1},\"downloaded\":{total_downloaded},\
         \"active\":{active},\"count\":{}}},\"jobs\":[",
        env!("CARGO_PKG_VERSION"),
        http::json_escape(&settings.output.display().to_string()),
        views.len()
    );

    for (index, view) in views.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let job = view.job;
        let error = match &job.error {
            Some(message) => format!("\"{}\"", http::json_escape(message)),
            None => "null".to_string(),
        };
        let limit = match job.limit {
            Some(rate) => rate.to_string(),
            None => "null".to_string(),
        };
        let finished = match job.finished {
            Some(seconds) => seconds.to_string(),
            None => "null".to_string(),
        };
        out.push_str(&format!(
            "{{\"id\":{},\"target\":\"{}\",\"name\":\"{}\",\"output\":\"{}\",\"path\":\"{}\",\
             \"status\":\"{}\",\"total\":{},\"downloaded\":{},\"percent\":{},\"limit\":{limit},\
             \"pieces\":{},\"verified\":{},\"peers\":{},\"speed\":{:.1},\"elapsed\":{:.1},\
             \"added\":{},\"finished\":{finished},\"error\":{error},\"peer_list\":{}}}",
            job.id,
            http::json_escape(&job.target),
            http::json_escape(&job.name),
            http::json_escape(&job.output.display().to_string()),
            http::json_escape(&view.path.display().to_string()),
            job.status,
            view.total,
            view.downloaded,
            view.percent,
            view.pieces,
            view.verified,
            view.peers,
            view.speed,
            view.elapsed,
            job.added,
            peer_list_json(&view.peer_list),
        ));
    }
    out.push_str("]}");
    out
}

fn peer_list_json(peers: &[PeerView]) -> String {
    let mut out = String::from("[");
    for (index, peer) in peers.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"address\":\"{}\",\"state\":\"{}\",\"bytes\":{}}}",
            peer.address, peer.state, peer.bytes
        ));
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> (SocketAddr, Arc<State>) {
        let server = Server::bind("127.0.0.1", 0, PathBuf::from("."), Options::default()).unwrap();
        let addr = server.addr().unwrap();
        let state = server.state.clone();
        std::thread::spawn(move || server.serve().unwrap());
        (addr, state)
    }

    fn spawn() -> SocketAddr {
        server().0
    }

    /// A job with no engine behind it, so the API tests do not depend on a swarm.
    fn stub(state: &State, id: u64, status: &'static str) -> Arc<AtomicBool> {
        let pause = Arc::new(AtomicBool::new(false));
        state.jobs.lock().unwrap().push(Job {
            id,
            target: "/tmp/stub.torrent".to_string(),
            name: "stub.iso".to_string(),
            output: PathBuf::from("/tmp"),
            status,
            total: 1000,
            limit: None,
            added: 1,
            finished: None,
            progress: None,
            stop: Some(Arc::new(AtomicBool::new(false))),
            pause: Some(pause.clone()),
            error: None,
        });
        pause
    }

    fn post(addr: SocketAddr, path: &str, body: &str) -> ureq::Response {
        ureq::post(&format!("http://{addr}{path}"))
            .set("Content-Type", "application/x-www-form-urlencoded")
            .send_string(body)
            .unwrap_or_else(|err| err.into_response().unwrap())
    }

    fn state_body(addr: SocketAddr) -> String {
        ureq::get(&format!("http://{addr}/api/state"))
            .call()
            .unwrap()
            .into_string()
            .unwrap()
    }

    #[test]
    fn serves_the_ui_and_an_empty_state() {
        let addr = spawn();
        let page = ureq::get(&format!("http://{addr}/")).call().unwrap();
        assert_eq!(page.status(), 200);
        let html = page.into_string().unwrap();
        assert!(html.to_lowercase().contains("<!doctype html"));
        assert!(html.contains("/api/state"));

        let state = state_body(addr);
        assert!(state.contains("\"jobs\":[]"), "state was {state}");
        assert!(state.contains("\"version\""), "state was {state}");
        assert!(state.contains("\"totals\""), "state was {state}");
    }

    #[test]
    fn rejects_an_add_without_a_target() {
        let addr = spawn();
        let result = ureq::post(&format!("http://{addr}/api/add"))
            .set("Content-Type", "application/x-www-form-urlencoded")
            .send_string("output=/tmp");
        let err = result.expect_err("a 400 should surface as an error");
        assert_eq!(err.into_response().unwrap().status(), 400);
    }

    #[test]
    fn serves_a_real_favicon_instead_of_a_failed_request() {
        let addr = spawn();
        let icon = ureq::get(&format!("http://{addr}/favicon.ico"))
            .call()
            .unwrap();
        assert_eq!(icon.status(), 200);
        assert_eq!(icon.content_type(), "image/svg+xml");
        assert!(icon.into_string().unwrap().contains("<svg"));
    }

    #[test]
    fn pause_and_resume_reject_unknown_jobs() {
        let addr = spawn();
        for endpoint in ["pause", "resume"] {
            let missing = ureq::post(&format!("http://{addr}/api/{endpoint}"))
                .set("Content-Type", "application/x-www-form-urlencoded")
                .send_string("");
            assert_eq!(missing.unwrap_err().into_response().unwrap().status(), 400);

            let unknown = ureq::post(&format!("http://{addr}/api/{endpoint}"))
                .set("Content-Type", "application/x-www-form-urlencoded")
                .send_string("id=404");
            assert_eq!(unknown.unwrap_err().into_response().unwrap().status(), 404);
        }
    }

    #[test]
    fn retry_on_a_paused_job_resumes_it_in_place() {
        let (addr, state) = server();
        let pause = stub(&state, 1, "paused");
        let body = post(addr, "/api/retry", "id=1").into_string().unwrap();
        assert!(body.contains("\"resumed\":true"), "body was {body}");
        assert!(
            !pause.load(Ordering::Relaxed),
            "retry should clear the pause flag"
        );
        let after = state_body(addr);
        assert!(
            after.contains("\"status\":\"downloading\""),
            "state was {after}"
        );
        assert_eq!(
            after.matches("\"id\":1").count(),
            1,
            "no duplicate job: {after}"
        );
    }

    #[test]
    fn retry_keeps_the_magnet_for_a_stopped_job() {
        let (addr, state) = server();
        state.jobs.lock().unwrap().push(Job {
            id: 7,
            // An invalid hash fails fast, so the test never touches the network.
            target: "magnet:?xt=urn:btih:not-a-hash".to_string(),
            name: "example".to_string(),
            output: PathBuf::from("/tmp"),
            status: "cancelled",
            total: 0,
            limit: None,
            added: 1,
            finished: Some(2),
            progress: None,
            stop: None,
            pause: None,
            error: Some("stopped".to_string()),
        });
        let body = post(addr, "/api/retry", "id=7").into_string().unwrap();
        assert!(body.starts_with("{\"id\":"), "body was {body}");
        let after = state_body(addr);
        assert!(
            after.contains("\"target\":\"magnet:?xt=urn:btih:not-a-hash\""),
            "retry must re-add the magnet, state was {after}"
        );
        assert!(!after.contains("cannot read example"), "state was {after}");
    }

    #[test]
    fn rejects_an_unparseable_rate_limit() {
        let addr = spawn();
        let err = ureq::post(&format!("http://{addr}/api/add"))
            .set("Content-Type", "application/x-www-form-urlencoded")
            .send_string("target=/nonexistent/file.torrent&limit=fast")
            .expect_err("a bad limit should be a 400, not a silent unlimited job");
        assert_eq!(err.into_response().unwrap().status(), 400);
    }

    #[test]
    fn a_job_keeps_the_limit_it_was_added_with() {
        let addr = spawn();
        ureq::post(&format!("http://{addr}/api/add"))
            .set("Content-Type", "application/x-www-form-urlencoded")
            .send_string("target=/nonexistent/file.torrent&limit=2M&output=/tmp")
            .unwrap();
        let mut body = String::new();
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(25));
            body = state_body(addr);
            if body.contains("\"limit\":2097152") {
                break;
            }
        }
        assert!(body.contains("\"limit\":2097152"), "state was {body}");
    }

    #[test]
    fn reports_a_torrent_that_cannot_be_read() {
        let addr = spawn();
        ureq::post(&format!("http://{addr}/api/add"))
            .set("Content-Type", "application/x-www-form-urlencoded")
            .send_string("target=/nonexistent/file.torrent&output=/tmp")
            .unwrap();

        // The default output directory is created on demand, so the error the
        // UI shows is about the torrent, not about a missing folder.
        let mut body = String::new();
        for _ in 0..80 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            body = state_body(addr);
            if body.contains("\"status\":\"error\"") {
                break;
            }
        }
        assert!(body.contains("\"status\":\"error\""), "state was {body}");
        assert!(body.contains("cannot read"), "state was {body}");
    }

    #[test]
    fn settings_round_trip_through_the_api() {
        let (addr, _state) = server();
        let initial = ureq::get(&format!("http://{addr}/api/settings"))
            .call()
            .unwrap()
            .into_string()
            .unwrap();
        assert!(
            initial.contains("\"limit\":null"),
            "settings were {initial}"
        );

        let changed = post(addr, "/api/settings", "output=/tmp&limit=2M");
        assert_eq!(changed.status(), 200);
        let body = changed.into_string().unwrap();
        assert!(body.contains("\"limit\":2097152"), "settings were {body}");

        let state = state_body(addr);
        assert!(state.contains("\"limit\":2097152"), "state was {state}");

        let cleared = post(addr, "/api/settings", "limit=none")
            .into_string()
            .unwrap();
        assert!(
            cleared.contains("\"limit\":null"),
            "settings were {cleared}"
        );
        assert!(
            cleared.contains("\"output\":\"/tmp\""),
            "settings were {cleared}"
        );
    }

    #[test]
    fn pause_all_leaves_finished_jobs_alone_and_can_resume_one() {
        let (addr, state) = server();
        let first = stub(&state, 1, "downloading");
        let second = stub(&state, 2, "downloading");
        stub(&state, 3, "done");

        let body = post(addr, "/api/pause", "all=1").into_string().unwrap();
        assert!(body.contains("\"affected\":2"), "response was {body}");
        assert!(first.load(Ordering::Relaxed) && second.load(Ordering::Relaxed));

        let body = post(addr, "/api/resume", "id=2").into_string().unwrap();
        assert!(body.contains("\"affected\":1"), "response was {body}");
        assert!(
            first.load(Ordering::Relaxed),
            "the other job was resumed too"
        );
        assert!(!second.load(Ordering::Relaxed));
    }

    #[test]
    fn remove_refuses_a_running_job_and_clears_finished_ones() {
        let (addr, state) = server();
        stub(&state, 1, "downloading");
        stub(&state, 2, "done");

        let refused = post(addr, "/api/remove", "id=1");
        assert_eq!(refused.status(), 400);

        let missing = post(addr, "/api/remove", "id=99");
        assert_eq!(missing.status(), 404);

        let removed = post(addr, "/api/remove", "finished=1")
            .into_string()
            .unwrap();
        assert!(removed.contains("\"removed\":1"), "response was {removed}");
        assert_eq!(state.jobs.lock().unwrap().len(), 1);
        assert!(state_job_ids(addr).contains(&1));
    }

    fn state_job_ids(addr: SocketAddr) -> Vec<u64> {
        state_body(addr)
            .split("\"id\":")
            .skip(1)
            .filter_map(|rest| rest.split(',').next()?.parse().ok())
            .collect()
    }

    #[test]
    fn retry_queues_a_new_copy_of_a_finished_job() {
        let (addr, state) = server();
        stub(&state, 7, "error");
        let body = post(addr, "/api/retry", "id=7").into_string().unwrap();
        let new_id: u64 = body
            .split("\"id\":")
            .nth(1)
            .and_then(|rest| rest.split('}').next()?.parse().ok())
            .unwrap_or(0);
        assert_ne!(new_id, 7, "retry reused the old job id: {body}");
        assert_eq!(state.jobs.lock().unwrap().len(), 2);

        stub(&state, 9, "downloading");
        let running = post(addr, "/api/retry", "id=9");
        assert_eq!(running.status(), 400, "an active job must not be retried");
    }

    #[test]
    fn reveal_reports_a_missing_job_instead_of_opening_anything() {
        let addr = spawn();
        assert_eq!(post(addr, "/api/reveal", "id=42").status(), 404);
        assert_eq!(post(addr, "/api/reveal", "").status(), 400);
    }

    #[test]
    fn base64_decodes_what_a_browser_sends() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGVs\nbG8=").unwrap(), b"hello");
        assert!(base64_decode("!!!!").is_none());
    }

    #[test]
    fn add_torrent_needs_base64_data() {
        let addr = spawn();
        assert_eq!(
            post(addr, "/api/add-torrent", "name=x.torrent").status(),
            400
        );
        let bad = post(addr, "/api/add-torrent", "name=x.torrent&data=!!!!");
        assert_eq!(bad.status(), 400);
        let empty = post(addr, "/api/add-torrent", "name=x.torrent&data=");
        assert_eq!(empty.status(), 400);
    }

    #[test]
    fn add_torrent_queues_the_upload_instead_of_failing_the_request() {
        let (addr, _state) = server();
        // Not a real .torrent: the upload succeeds and the parser reports the
        // error on the job, which is what the table shows.
        let body = post(addr, "/api/add-torrent", "name=notes.torrent&data=aGVsbG8=")
            .into_string()
            .unwrap();
        assert!(body.contains("\"id\":1"), "response was {body}");

        let mut state = String::new();
        for _ in 0..60 {
            std::thread::sleep(std::time::Duration::from_millis(25));
            state = state_body(addr);
            if state.contains("\"status\":\"error\"") {
                break;
            }
        }
        assert!(state.contains("\"status\":\"error\""), "state was {state}");
    }
}
