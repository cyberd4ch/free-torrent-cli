//! Footorrent's native window, drawn by Rune.
//!
//! It reads the same local JSON API the web UI uses, so one engine feeds both
//! surfaces. The palette is the product one: warm blue on warm off-white.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rune::tok::{motion, radius, space, type_};
use rune::{AppSpec, Color, Cx, El, Tx, Waker, col, el, oklch, row, text};
use serde_json::Value;

/// The engine address. The macOS host passes `FTC_URL`; the default matches
/// `ftc serve`'s documented port so a hand-started engine just works.
fn base_url() -> &'static str {
    static URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    URL.get_or_init(|| {
        std::env::var("FTC_URL").unwrap_or_else(|_| "http://127.0.0.1:8123".to_string())
    })
}

/* ---------- palette ---------- */

fn paper() -> Color {
    oklch(0.968, 0.012, 82.0)
}
fn card() -> Color {
    oklch(0.995, 0.006, 82.0)
}
fn ink() -> Color {
    oklch(0.255, 0.028, 250.0)
}
fn muted() -> Color {
    oklch(0.56, 0.02, 250.0)
}
fn hair() -> Color {
    oklch(0.905, 0.014, 82.0)
}
fn blue() -> Color {
    oklch(0.53, 0.115, 250.0)
}
fn blue_ink() -> Color {
    oklch(0.42, 0.11, 250.0)
}
fn blue_wash() -> Color {
    oklch(0.945, 0.022, 250.0)
}
fn green() -> Color {
    oklch(0.52, 0.09, 155.0)
}
fn amber() -> Color {
    oklch(0.6, 0.1, 75.0)
}
fn red() -> Color {
    oklch(0.52, 0.13, 25.0)
}

/* ---------- data ---------- */

#[derive(Clone, Debug)]
struct Job {
    id: u64,
    name: String,
    total: u64,
    downloaded: u64,
    speed: f64,
    peers: u64,
    status: String,
    error: Option<String>,
}

impl Job {
    fn percent(&self) -> f32 {
        if self.status == "done" {
            return 100.0;
        }
        if self.total == 0 {
            return 0.0;
        }
        ((self.downloaded as f64 / self.total as f64) * 100.0).clamp(0.0, 100.0) as f32
    }

    fn state_color(&self) -> Color {
        match self.status.as_str() {
            "downloading" => blue(),
            "done" => green(),
            "paused" => amber(),
            "error" => red(),
            _ => muted(),
        }
    }
}

#[derive(Clone, Default, Debug)]
struct Snapshot {
    jobs: Vec<Job>,
    speed: f64,
    downloaded: u64,
    active: u64,
    version: u64,
    error: Option<String>,
    /// False until the first successful read, so the window never claims the
    /// engine is live before it has answered.
    live: bool,
}

struct State {
    shared: Arc<Mutex<Snapshot>>,
    seen: u64,
    selected: Option<u64>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            shared: Arc::new(Mutex::new(Snapshot::default())),
            seen: 0,
            selected: None,
        }
    }
}

fn number(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_u64).unwrap_or(0)
}

fn parse(body: &str) -> Result<Snapshot, String> {
    let root: Value = serde_json::from_str(body).map_err(|err| err.to_string())?;
    let mut jobs = Vec::new();
    for entry in root["jobs"].as_array().into_iter().flatten() {
        jobs.push(Job {
            id: number(entry.get("id")),
            name: entry["name"].as_str().unwrap_or("download").to_string(),
            total: number(entry.get("total")),
            downloaded: number(entry.get("downloaded")),
            speed: entry["speed"].as_f64().unwrap_or(0.0),
            peers: number(entry.get("peers")),
            status: entry["status"].as_str().unwrap_or("paused").to_string(),
            error: entry["error"].as_str().map(str::to_string),
        });
    }
    let totals = &root["totals"];
    Ok(Snapshot {
        jobs,
        speed: totals["speed"].as_f64().unwrap_or(0.0),
        downloaded: number(totals.get("downloaded")),
        active: number(totals.get("active")),
        version: 0,
        error: None,
        live: true,
    })
}

fn fetch() -> Result<Snapshot, String> {
    let body = ureq::get(&format!("{}/api/state", base_url()))
        .timeout(Duration::from_secs(3))
        .call()
        .map_err(|_| "engine not reachable on 127.0.0.1:8123".to_string())?
        .into_string()
        .map_err(|err| err.to_string())?;
    parse(&body)
}

fn act(path: &str, id: Option<u64>) {
    let url = format!("{}/api/{path}", base_url());
    let result = match id {
        Some(id) => {
            let value = id.to_string();
            ureq::post(&url).send_form(&[("id", value.as_str())])
        }
        None => ureq::post(&url).send_form(&[]),
    };
    if let Err(err) = result {
        eprintln!("footorrent: {path} failed: {err}");
    }
}

/* ---------- formatting ---------- */

fn bytes(value: u64) -> String {
    if value == 0 {
        return "0 B".into();
    }
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut size = value as f64;
    let mut index = 0;
    while size >= 1024.0 && index < units.len() - 1 {
        size /= 1024.0;
        index += 1;
    }
    if index == 0 || size >= 100.0 {
        format!("{size:.0} {}", units[index])
    } else {
        format!("{size:.1} {}", units[index])
    }
}

fn rate(value: f64) -> String {
    if value < 0.5 {
        "—".into()
    } else {
        format!("{}/s", bytes(value.max(0.0) as u64))
    }
}

/* ---------- pieces ---------- */

fn chip(label: &str, color: Color, wash: Color) -> El {
    el().center()
        .h(24.0)
        .px(10.0)
        .radius(12.0)
        .fill(wash)
        .child(text(label).font(type_::LABEL).color(color))
}

fn button(label: &str, primary: bool) -> El {
    let base = el()
        .center()
        .h(32.0)
        .px(14.0)
        .radius(radius::MD)
        .role("button")
        .label(label)
        .focusable(true)
        .child(
            text(label)
                .font(type_::LABEL)
                .color(if primary { card() } else { ink() }),
        );
    if primary {
        base.fill(blue()).hover(blue_ink()).motion(motion::SNAPPY)
    } else {
        base.fill(card())
            .border(1.0, hair())
            .hover(blue_wash())
            .motion(motion::SNAPPY)
    }
}

fn bar(job: &Job) -> El {
    let width = 150.0;
    let filled = width * (job.percent() / 100.0);
    row()
        .key(format!("track-{}", job.id))
        .h(6.0)
        .w(width)
        .radius(3.0)
        .fill(hair())
        .child(
            el().h_full()
                .w(filled)
                .radius(3.0)
                .fill(job.state_color())
                .motion(motion::CALM),
        )
}

fn job_row(job: &Job, selected: bool) -> El {
    let name = if job.name.is_empty() {
        format!("#{}", job.id)
    } else {
        job.name.clone()
    };
    let sub = if let Some(error) = &job.error {
        error.clone()
    } else {
        format!(
            "{} of {} · {} peers",
            bytes(job.downloaded),
            bytes(job.total),
            job.peers
        )
    };
    let status = match job.status.as_str() {
        "metadata" => "resolving",
        "downloading" => "downloading",
        "paused" => "paused",
        "done" => "complete",
        "error" => "failed",
        "cancelled" => "stopped",
        other => other,
    };

    let id = job.id;
    row()
        .key(format!("job-{id}"))
        .h(58.0)
        .w_full()
        .px(space::S6)
        .gap(18.0)
        .center_y()
        .fill(if selected { blue_wash() } else { paper() })
        .hover(if selected { blue_wash() } else { card() })
        .motion(motion::SNAPPY)
        .role("listitem")
        .label(format!("{name}, {status}, {} percent", job.percent()))
        .focusable(true)
        .on_press(move |s: &mut State| s.selected = Some(id))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(2.0)
                .child(
                    text(name)
                        .key("name")
                        .font(type_::BODY)
                        .color(ink())
                        .max_lines(1),
                )
                .child(
                    text(sub)
                        .font(type_::LABEL)
                        .color(if job.error.is_some() { red() } else { muted() })
                        .max_lines(1),
                ),
        )
        .child(
            col().w(150.0).gap(6.0).child(bar(job)).child(
                text(format!("{:.0}%", job.percent()))
                    .key("pct")
                    .font(type_::LABEL)
                    .color(muted()),
            ),
        )
        .child(
            col()
                .w(84.0)
                .gap(2.0)
                .child(text(rate(job.speed)).font(type_::LABEL).color(ink())),
        )
        .child(
            el().w(74.0)
                .center()
                .child(text(status).font(type_::LABEL).color(job.state_color())),
        )
}

fn empty() -> El {
    col()
        .w_full()
        .grow(1.0)
        .center()
        .gap(6.0)
        .child(text("Nothing downloading").font(type_::TITLE).color(ink()))
        .child(
            text("Add a magnet link in the browser window to start.")
                .font(type_::LABEL)
                .color(muted()),
        )
}

/* ---------- view ---------- */

fn view(state: &State, _: &mut Cx<State>) -> El {
    let snap = state.shared.lock().unwrap().clone();
    let doubled = state.selected;

    let mut list = col().w_full().min_h(0.0);
    if snap.jobs.is_empty() {
        list = list.child(empty());
    } else {
        for job in snap.jobs.iter().take(7) {
            list = list.child(job_row(job, doubled == Some(job.id)));
        }
        if snap.jobs.len() > 7 {
            list = list.child(
                row().h(34.0).w_full().center().child(
                    text(format!(
                        "+{} more in the browser window",
                        snap.jobs.len() - 7
                    ))
                    .font(type_::LABEL)
                    .color(muted()),
                ),
            );
        }
    }

    let online = snap.error.is_none();
    let live = online && snap.live;
    let (engine_label, engine_tone) = if !online {
        ("engine offline", red())
    } else if snap.live {
        ("engine live", green())
    } else {
        ("engine starting", amber())
    };
    let status_text = snap
        .error
        .clone()
        .unwrap_or_else(|| format!("{} active · {} total", snap.active, snap.jobs.len()));

    col()
        .key("window")
        .w_full()
        .h_full()
        .fill(paper())
        .child(
            row()
                .key("titlebar")
                .h(64.0)
                .w_full()
                .px(space::S6)
                .center_y()
                .gap(12.0)
                .fill(paper())
                .child(
                    el().size(30.0, 30.0)
                        .radius(9.0)
                        .fill(blue())
                        .center()
                        .child(text("↓").font(type_::TITLE).color(card())),
                )
                .child(
                    col()
                        .gap(1.0)
                        .child(
                            text("footorrent")
                                .key("wordmark")
                                .font(type_::TITLE)
                                .color(ink()),
                        )
                        .child(
                            text("Small footprint. Big downloads.")
                                .font(type_::LABEL)
                                .color(muted()),
                        ),
                )
                .child(el().grow(1.0))
                .child(chip(
                    engine_label,
                    engine_tone,
                    if online { blue_wash() } else { hair() },
                ))
                .child(chip(&rate(snap.speed), blue_ink(), blue_wash())),
        )
        .child(el().h(1.0).w_full().fill(hair()))
        .child(
            row()
                .h(56.0)
                .w_full()
                .px(space::S6)
                .gap(8.0)
                .center_y()
                .child(button("Start", true).on_press(|s: &mut State| {
                    if let Some(id) = s.selected {
                        act("resume", Some(id));
                    }
                }))
                .child(button("Pause", false).on_press(|s: &mut State| {
                    if let Some(id) = s.selected {
                        act("pause", Some(id));
                    }
                }))
                .child(button("Stop", false).on_press(|s: &mut State| {
                    if let Some(id) = s.selected {
                        act("cancel", Some(id));
                    }
                }))
                .child(button("Remove", false).on_press(|s: &mut State| {
                    if let Some(id) = s.selected {
                        act("remove", Some(id));
                    }
                }))
                .child(el().grow(1.0))
                .child(button("Add download…", false).on_press(|_: &mut State| {
                    let _ = std::process::Command::new("open")
                        .arg(format!("{}/", base_url()))
                        .status();
                }))
                .child(
                    text(status_text)
                        .key("engine-status")
                        .font(type_::LABEL)
                        .color(if live { muted() } else { red() }),
                ),
        )
        .child(el().h(1.0).w_full().fill(hair()))
        .child(
            row()
                .h(30.0)
                .w_full()
                .px(space::S6)
                .gap(18.0)
                .center_y()
                .child(text("DOWNLOAD").font(type_::SECTION).color(muted()))
                .child(el().grow(1.0))
                .child(
                    text("PROGRESS")
                        .w(150.0)
                        .font(type_::SECTION)
                        .color(muted()),
                )
                .child(text("SPEED").w(84.0).font(type_::SECTION).color(muted()))
                .child(text("STATUS").w(74.0).font(type_::SECTION).color(muted())),
        )
        .child(list)
        .child(el().grow(1.0))
        .child(
            row()
                .h(40.0)
                .w_full()
                .px(space::S6)
                .center_y()
                .gap(16.0)
                .border(0.0, paper())
                .fill(card())
                .child(
                    text(format!(
                        "{} downloaded · {} active",
                        bytes(snap.downloaded),
                        snap.active
                    ))
                    .font(type_::LABEL)
                    .color(muted()),
                )
                .child(el().grow(1.0))
                .child(
                    text("127.0.0.1:8123 · warm blue edition")
                        .font(type_::LABEL)
                        .color(muted()),
                ),
        )
}

/* ---------- wiring ---------- */

fn setup(state: &mut State, waker: Waker) {
    // A headless render has no engine to talk to; its scene is the whole story.
    if rune::is_headless() {
        return;
    }
    let shared = state.shared.clone();
    std::thread::spawn(move || {
        loop {
            match fetch() {
                Ok(mut next) => {
                    let mut current = shared.lock().unwrap();
                    let changed = current.jobs.len() != next.jobs.len()
                        || current.downloaded != next.downloaded
                        || current.active != next.active
                        || current.error.is_some()
                        || current.jobs.iter().zip(&next.jobs).any(|(a, b)| {
                            a.id != b.id
                                || a.downloaded != b.downloaded
                                || a.status != b.status
                                || a.speed != b.speed
                        });
                    if changed {
                        next.version = current.version + 1;
                        *current = next;
                        drop(current);
                        waker();
                    }
                }
                Err(message) => {
                    let mut current = shared.lock().unwrap();
                    if current.error.as_deref() != Some(message.as_str()) {
                        current.error = Some(message);
                        current.version += 1;
                        drop(current);
                        waker();
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

fn poll(state: &mut State, _now: f64, _tx: &mut Tx) -> bool {
    let version = state.shared.lock().unwrap().version;
    if version != state.seen {
        state.seen = version;
        true
    } else {
        false
    }
}

fn demo(state: &mut State) {
    let mut snap = Snapshot {
        speed: 6_400_000.0,
        downloaded: 812_000_000,
        active: 2,
        version: 1,
        error: None,
        live: true,
        jobs: vec![
            Job {
                id: 1,
                name: "ubuntu-24.04.4-desktop-amd64.iso".into(),
                total: 6_100_000_000,
                downloaded: 812_000_000,
                speed: 6_400_000.0,
                peers: 14,
                status: "downloading".into(),
                error: None,
            },
            Job {
                id: 2,
                name: "sintel-2010-1080p".into(),
                total: 762_000_000,
                downloaded: 762_000_000,
                speed: 0.0,
                peers: 0,
                status: "done".into(),
                error: None,
            },
            Job {
                id: 3,
                name: "debian-13-netinst.iso".into(),
                total: 690_000_000,
                downloaded: 0,
                speed: 0.0,
                peers: 0,
                status: "paused".into(),
                error: None,
            },
        ],
    };
    snap.version = 1;
    *state.shared.lock().unwrap() = snap;
    state.seen = 1;
    state.selected = Some(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_json_becomes_jobs() {
        let body = r#"{"jobs":[{"id":7,"name":"ubuntu.iso","total":100,"downloaded":25,"speed":12.5,"peers":4,"status":"downloading"}],"totals":{"speed":12.5,"downloaded":25,"active":1}}"#;
        let snap = parse(body).unwrap();
        assert_eq!(snap.jobs[0].id, 7);
        assert_eq!(snap.jobs[0].percent(), 25.0);
        assert_eq!(snap.active, 1);
        assert!(snap.live);
    }

    #[test]
    fn sizes_and_rates_read_like_the_page() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1536), "1.5 KB");
        assert_eq!(rate(0.0), "—");
        assert_eq!(rate(6_400_000.0), "6.1 MB/s");
    }
}

fn main() {
    rune::set_app_id("dev.footorrent.webui");
    rune::run(AppSpec::<State> {
        scenes: &[("demo", demo)],
        setup: Some(setup),
        poll: Some(poll),
        min_size: Some((760.0, 480.0)),
        ..AppSpec::new("Footorrent", (980.0, 640.0), State::default, view)
    });
}
