//! `ftc serve` — the download manager UI and its JSON API.
//!
//! One thread per connection, one thread per download. Jobs hold the same
//! `Arc<Progress>` the CLI prints from, so the UI polls real counters instead of
//! a second bookkeeping layer that could disagree with the engine.

use crate::http::{self, Request};
use crate::parse_rate;
use ftc_core::error::Error;
use ftc_core::magnet;
use ftc_core::session::{make_peer_id, Options};
use ftc_core::{Progress, Session, Torrent};
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const UI: &str = include_str!("ui.html");

/// Served at `/favicon.ico` so a browser never logs a failed icon request.
const ICON: &str = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'>\
<rect width='32' height='32' rx='8' fill='#111318'/>\
<g fill='none' stroke='#ff7a2f' stroke-width='3' stroke-linecap='round' stroke-linejoin='round'>\
<path d='M16 7v13'/><path d='M10 15l6 6 6-6'/><path d='M8 26h16'/></g></svg>";

pub struct Server {
    listener: TcpListener,
    state: Arc<State>,
}

struct State {
    jobs: Mutex<Vec<Job>>,
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

impl Server {
    pub fn bind(bind: &str, port: u16, output: PathBuf, options: Options) -> io::Result<Server> {
        let listener = TcpListener::bind((bind, port))?;
        Ok(Server {
            listener,
            state: Arc::new(State {
                jobs: Mutex::new(Vec::new()),
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
        ("POST", "/api/cancel") => cancel(state, &request.body),
        ("POST", "/api/pause") => pause(state, &request.body, true),
        ("POST", "/api/resume") => pause(state, &request.body, false),
        ("POST", "/api/remove") => remove(state, &request.body),
        ("POST", "/api/retry") => retry(state, &request.body),
        ("POST", "/api/reveal") => reveal(state, &request.body),
        (_, "/favicon.ico") => (200, "image/svg+xml", ICON.as_bytes().to_vec()),
        (_, "/api/add")
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
    state.jobs.lock().unwrap().push(Job {
        id,
        target: target.clone(),
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
    spawn_download(state.clone(), id, target, output, limit, paused);
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

    let before = jobs.len();
    jobs.retain(|job| !(scope.covers(job.id) && is_terminal(job.status)));
    let removed = before - jobs.len();
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
        let jobs = state.jobs.lock().unwrap();
        let Some(job) = jobs.iter().find(|job| job.id == id) else {
            return (404, "text/plain; charset=utf-8", b"no such job".to_vec());
        };
        if !is_terminal(job.status) {
            return bad("only stopped or failed downloads can be retried");
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
    let dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.to_path_buf())
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        if path.is_file() {
            command.arg("-R").arg(path);
        } else {
            command.arg(&dir);
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
        let torrent = match resolve(&target, &state.options) {
            Ok(torrent) => torrent,
            Err(message) => return finish(&state, id, "error", Some(message)),
        };
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

        match session.run(None) {
            Ok(_) => finish(&state, id, "done", None),
            Err(Error::Cancelled) => finish(&state, id, "cancelled", None),
            Err(err) => finish(&state, id, "error", Some(err.to_string())),
        }
    });
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
}

fn view(job: &Job) -> View<'_> {
    let snapshot = job.progress.as_ref().map(|progress| progress.snapshot());
    let speed = job
        .progress
        .as_ref()
        .map(|progress| progress.bytes_per_second())
        .unwrap_or(0.0);
    let (total, downloaded, pieces, verified, peers, elapsed) = match snapshot {
        Some(snapshot) => (
            snapshot.total,
            snapshot.downloaded,
            snapshot.pieces,
            snapshot.verified,
            snapshot.active_peers,
            snapshot.elapsed.as_secs_f64(),
        ),
        None => (job.total, 0, 0, 0, 0, 0.0),
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
             \"added\":{},\"finished\":{finished},\"error\":{error}}}",
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
        ));
    }
    out.push_str("]}");
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
}
