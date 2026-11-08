//! `ftc serve` — the web UI and its JSON API.
//!
//! One thread per connection, one thread per download. Jobs hold the same
//! `Arc<Progress>` the CLI prints from, so the UI polls real counters instead of
//! a second bookkeeping layer that could disagree with the engine.

use crate::http::{self, Request};
use ftc_core::error::Error;
use ftc_core::magnet;
use ftc_core::session::{make_peer_id, Options};
use ftc_core::{Progress, Session, Torrent};
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const UI: &str = include_str!("ui.html");

pub struct Server {
    listener: TcpListener,
    state: Arc<State>,
}

struct State {
    jobs: Mutex<Vec<Job>>,
    next_id: AtomicU64,
    default_output: PathBuf,
    options: Options,
}

struct Job {
    id: u64,
    target: String,
    name: String,
    output: PathBuf,
    status: &'static str,
    total: u64,
    progress: Option<Arc<Progress>>,
    stop: Option<Arc<AtomicBool>>,
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
                default_output: output,
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
        ("POST", "/api/add") => add(state, &request.body),
        ("POST", "/api/cancel") => cancel(state, &request.body),
        ("GET", "/favicon.ico") => (204, "image/x-icon", Vec::new()),
        (_, "/api/add") | (_, "/api/cancel") | (_, "/api/state") => (
            405,
            "text/plain; charset=utf-8",
            b"method not allowed".to_vec(),
        ),
        _ => (404, "text/plain; charset=utf-8", b"not found".to_vec()),
    }
}

fn add(state: &Arc<State>, body: &str) -> (u16, &'static str, Vec<u8>) {
    let form = http::form_pairs(body);
    let field = |name: &str| {
        form.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };

    let Some(target) = field("target") else {
        return (400, "text/plain; charset=utf-8", b"missing target".to_vec());
    };
    let output = field("output")
        .map(PathBuf::from)
        .unwrap_or_else(|| state.default_output.clone());

    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    state.jobs.lock().unwrap().push(Job {
        id,
        target: target.clone(),
        name: String::new(),
        output: output.clone(),
        status: "metadata",
        total: 0,
        progress: None,
        stop: None,
        error: None,
    });
    spawn_download(state.clone(), id, target, output);
    (
        200,
        "application/json",
        format!("{{\"id\":{id}}}").into_bytes(),
    )
}

fn cancel(state: &Arc<State>, body: &str) -> (u16, &'static str, Vec<u8>) {
    let form = http::form_pairs(body);
    let id = form
        .iter()
        .find(|(key, _)| key == "id")
        .and_then(|(_, value)| value.parse::<u64>().ok());
    let Some(id) = id else {
        return (400, "text/plain; charset=utf-8", b"missing id".to_vec());
    };

    let mut jobs = state.jobs.lock().unwrap();
    let Some(job) = jobs.iter_mut().find(|job| job.id == id) else {
        return (404, "text/plain; charset=utf-8", b"no such job".to_vec());
    };
    if matches!(job.status, "done" | "error" | "cancelled") {
        return (
            400,
            "text/plain; charset=utf-8",
            b"job already finished".to_vec(),
        );
    }
    // Metadata exchange cannot be interrupted; marking the job cancelled makes
    // the worker drop whatever comes back.
    if let Some(stop) = &job.stop {
        stop.store(true, Ordering::Relaxed);
    }
    job.status = "cancelled";
    (
        200,
        "application/json",
        format!("{{\"id\":{id}}}").into_bytes(),
    )
}

fn spawn_download(state: Arc<State>, id: u64, target: String, output: PathBuf) {
    std::thread::spawn(move || {
        let torrent = match resolve(&target, &state.options) {
            Ok(torrent) => torrent,
            Err(message) => return finish(&state, id, "error", Some(message)),
        };
        if is_cancelled(&state, id) {
            return;
        }

        let (session, progress) = match Session::open(torrent, &output, state.options.clone()) {
            Ok(opened) => opened,
            Err(err) => return finish(&state, id, "error", Some(err.to_string())),
        };
        let stop = session.stop_handle();
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
            job.status = "downloading";
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
}

fn state_json(state: &State) -> String {
    let jobs = state.jobs.lock().unwrap();
    let mut out = String::from("{\"jobs\":[");
    for (index, job) in jobs.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
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
        let error = match &job.error {
            Some(message) => format!("\"{}\"", http::json_escape(message)),
            None => "null".to_string(),
        };
        out.push_str(&format!(
            "{{\"id\":{},\"target\":\"{}\",\"name\":\"{}\",\"output\":\"{}\",\"status\":\"{}\",\
             \"total\":{},\"downloaded\":{},\"pieces\":{},\"verified\":{},\"peers\":{},\
             \"speed\":{:.1},\"elapsed\":{:.1},\"error\":{}}}",
            job.id,
            http::json_escape(&job.target),
            http::json_escape(&job.name),
            http::json_escape(&job.output.display().to_string()),
            job.status,
            total,
            downloaded,
            pieces,
            verified,
            peers,
            speed,
            elapsed,
            error
        ));
    }
    out.push_str("]}");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn() -> SocketAddr {
        let server = Server::bind("127.0.0.1", 0, PathBuf::from("."), Options::default()).unwrap();
        let addr = server.addr().unwrap();
        std::thread::spawn(move || server.serve().unwrap());
        addr
    }

    #[test]
    fn serves_the_ui_and_an_empty_state() {
        let addr = spawn();
        let page = ureq::get(&format!("http://{addr}/")).call().unwrap();
        assert_eq!(page.status(), 200);
        let html = page.into_string().unwrap();
        assert!(html.to_lowercase().contains("<!doctype html"));
        assert!(html.contains("/api/state"));

        let state = ureq::get(&format!("http://{addr}/api/state"))
            .call()
            .unwrap();
        assert_eq!(state.status(), 200);
        assert!(state.into_string().unwrap().contains("\"jobs\":[]"));
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
    fn reports_a_torrent_that_cannot_be_read() {
        let addr = spawn();
        ureq::post(&format!("http://{addr}/api/add"))
            .set("Content-Type", "application/x-www-form-urlencoded")
            .send_string("target=/nonexistent/file.torrent&output=/tmp")
            .unwrap();

        let mut body = String::new();
        for _ in 0..80 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            body = ureq::get(&format!("http://{addr}/api/state"))
                .call()
                .unwrap()
                .into_string()
                .unwrap();
            if body.contains("\"status\":\"error\"") {
                break;
            }
        }
        assert!(body.contains("\"status\":\"error\""), "state was {body}");
        assert!(body.contains("cannot read"), "state was {body}");
    }
}
