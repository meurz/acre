//! Frame timing: read GPUI frame records (`profiler`) and pair operations with their visible results.
//!
//! - Draw: the start/end of `Window::draw` (element construction, layout, and paint commands).
//! - Present: the start/end of submitting the frame to the system (DirectX submission and Present).
//!
//! RDP limits the Present cadence in remote desktop sessions, so its frame rate is only an upper bound.
//! Draw duration is a more reliable measure of the CPU cost per frame.

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui_kit::{FrameEvent, FrameTimingCollector};
use serde_json::{Value, json};

pub struct Clock {
    pub start: Instant,
    pub start_unix_us: u128,
}

static CLOCK: OnceLock<Clock> = OnceLock::new();
static PHASES: std::sync::Mutex<Vec<(&'static str, Instant)>> = std::sync::Mutex::new(Vec::new());

/// Record each startup phase once.
pub fn phase(name: &'static str) {
    let mut v = PHASES.lock().unwrap();
    if !v.iter().any(|(n, _)| *n == name) {
        v.push((name, Instant::now()));
    }
}

pub fn phases_json() -> Value {
    let v = PHASES.lock().unwrap();
    Value::Array(v.iter().map(|(n, t)| json!({"phase": n, "ms": ms_since_start(*t)})).collect())
}

pub fn mark_process_start() {
    let _ = CLOCK.set(Clock {
        start: Instant::now(),
        start_unix_us: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros()).unwrap_or(0),
    });
}

pub fn clock() -> &'static Clock {
    CLOCK.get_or_init(|| Clock {
        start: Instant::now(),
        start_unix_us: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros()).unwrap_or(0),
    })
}

pub fn ms_since_start(t: Instant) -> f64 {
    t.saturating_duration_since(clock().start).as_secs_f64() * 1000.0
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[derive(Clone, Copy, Debug)]
pub struct Span {
    pub start: Instant,
    pub end: Instant,
}

impl Span {
    fn ms(&self) -> f64 {
        ms(self.end.saturating_duration_since(self.start))
    }
}

/// An operation starts at t0; at ready its result is in UI state, followed by the first draw and present.
#[derive(Clone, Debug)]
pub struct Marker {
    pub name: String,
    pub t0: Instant,
    pub ready: Option<Instant>,
    pub extra: Value,
}

pub struct Recording {
    pub name: String,
    pub start: Instant,
    pub draws: Vec<Span>,
    pub presents: Vec<Span>,
    /// Cumulative GPUI input-to-present histogram at the start; subtract it at the end for this recording.
    pub input_before: Option<gpui_kit::InputLatencySnapshot>,
}

pub struct Perf {
    collector: FrameTimingCollector,
    pub draws: VecDeque<Span>,
    pub presents: VecDeque<Span>,
    pub recording: Option<Recording>,
    pub markers: Vec<Marker>,
    pub ops: VecDeque<Value>,
    pub total_draws: u64,
    pub first_draw: Option<Span>,
    /// First frame with content: the first completed draw after data becomes ready.
    pub content_marker: Option<Instant>,
    pub first_content: Option<(Span, Option<Span>)>,
}

const KEEP: usize = 4096;

impl Perf {
    pub fn new() -> Self {
        gpui_kit::set_trace_enabled(true);
        Self {
            collector: FrameTimingCollector::new(),
            draws: VecDeque::with_capacity(KEEP),
            presents: VecDeque::with_capacity(KEEP),
            recording: None,
            markers: Vec::new(),
            ops: VecDeque::new(),
            total_draws: 0,
            first_draw: None,
            content_marker: None,
            first_content: None,
        }
    }

    /// Collect new frame events. Poll at least every 100 ms to avoid overrunning the 16 MiB ring buffer.
    pub fn poll(&mut self) {
        for ev in self.collector.collect_unseen() {
            match ev {
                FrameEvent::Draw(t) => {
                    let s = Span { start: t.draw_start, end: t.draw_end };
                    self.total_draws += 1;
                    if self.first_draw.is_none() {
                        self.first_draw = Some(s);
                    }
                    if self.draws.len() == KEEP {
                        self.draws.pop_front();
                    }
                    self.draws.push_back(s);
                    if let Some(r) = &mut self.recording {
                        r.draws.push(s);
                    }
                }
                FrameEvent::Present(t) => {
                    let s = Span { start: t.present_start, end: t.present_end };
                    if self.presents.len() == KEEP {
                        self.presents.pop_front();
                    }
                    self.presents.push_back(s);
                    if let Some(r) = &mut self.recording {
                        r.presents.push(s);
                    }
                }
            }
        }
        self.resolve();
    }

    fn first_draw_after(&self, t: Instant) -> Option<Span> {
        self.draws.iter().find(|d| d.start >= t).copied()
    }

    fn first_present_after(&self, t: Instant) -> Option<Span> {
        self.presents.iter().find(|p| p.start >= t).copied()
    }

    fn resolve(&mut self) {
        if self.first_content.is_none()
            && let Some(t) = self.content_marker
            && let Some(d) = self.first_draw_after(t)
        {
            let p = self.first_present_after(d.end);
            self.first_content = Some((d, p));
        }
        let now = Instant::now();
        let mut keep = Vec::new();
        for m in std::mem::take(&mut self.markers) {
            let Some(ready) = m.ready else {
                if now.duration_since(m.t0) < Duration::from_secs(60) {
                    keep.push(m);
                }
                continue;
            };
            let draw = self.first_draw_after(ready);
            let present = draw.and_then(|d| self.first_present_after(d.end));
            if let (Some(d), Some(p)) = (draw, present) {
                let mut v = json!({
                    "name": m.name,
                    "at_ms": ms_since_start(m.t0),
                    "ready_ms": ms(ready - m.t0),
                    "draw_end_ms": ms(d.end.saturating_duration_since(m.t0)),
                    "draw_ms": d.ms(),
                    "present_end_ms": ms(p.end.saturating_duration_since(m.t0)),
                });
                if let (Value::Object(o), Value::Object(extra)) = (&mut v, &m.extra) {
                    for (k, val) in extra {
                        o.insert(k.clone(), val.clone());
                    }
                }
                self.push_op(v);
            } else if now.duration_since(ready) < Duration::from_secs(5) {
                keep.push(m);
            }
        }
        self.markers = keep;
    }

    pub fn push_op(&mut self, v: Value) {
        if self.ops.len() >= 5000 {
            self.ops.pop_front();
        }
        self.ops.push_back(v);
    }

    pub fn begin(&mut self, name: &str, t0: Instant, extra: Value) -> usize {
        self.markers.push(Marker { name: name.into(), t0, ready: None, extra });
        self.markers.len() - 1
    }

    /// Mark an operation's result as available in UI state.
    pub fn ready(&mut self, name: &str, extra: Value) {
        let now = Instant::now();
        if let Some(m) = self.markers.iter_mut().rev().find(|m| m.name == name && m.ready.is_none()) {
            m.ready = Some(now);
            if let (Value::Object(o), Value::Object(e)) = (&mut m.extra, extra) {
                for (k, v) in e {
                    o.insert(k, v);
                }
            }
        }
    }

    /// Start and immediately mark ready for a synchronously completed operation.
    pub fn instant(&mut self, name: &str, t0: Instant, extra: Value) {
        self.markers.push(Marker { name: name.into(), t0, ready: Some(Instant::now()), extra });
    }

    pub fn start_recording(&mut self, name: &str, window: &gpui_kit::Window) {
        self.poll();
        self.recording = Some(Recording {
            name: name.into(),
            start: Instant::now(),
            draws: Vec::new(),
            presents: Vec::new(),
            input_before: Some(window.input_latency_snapshot()),
        });
    }

    pub fn stop_recording(&mut self, window: &gpui_kit::Window) -> Value {
        self.poll();
        let Some(r) = self.recording.take() else { return Value::Null };
        let mut v = summarize(&r, Instant::now());
        if let Some(before) = &r.input_before {
            let after = window.input_latency_snapshot();
            let mut h = after.latency_histogram.clone();
            if h.subtract(&before.latency_histogram).is_ok() && !h.is_empty() {
                let q = |p: f64| round3(h.value_at_quantile(p) as f64 / 1e6);
                v["input_to_present_ms"] = json!({
                    "samples": h.len(), "p50": q(0.5), "p95": q(0.95), "p99": q(0.99), "max": round3(h.max() as f64 / 1e6),
                });
            }
            let mut e = after.events_per_frame_histogram.clone();
            if e.subtract(&before.events_per_frame_histogram).is_ok() && !e.is_empty() {
                v["input_events_per_frame"] = json!({"p50": e.value_at_quantile(0.5), "max": e.max()});
            }
        }
        v
    }

    /// Status bar readings: presents in the last second and the longest Draw in the last two seconds.
    pub fn hud(&self) -> (u32, f64, f64) {
        let now = Instant::now();
        let fps = self.presents.iter().rev().take_while(|p| now.duration_since(p.end) < Duration::from_secs(1)).count();
        let recent: Vec<f64> = self
            .draws
            .iter()
            .rev()
            .take_while(|d| now.duration_since(d.end) < Duration::from_secs(2))
            .map(|d| d.ms())
            .collect();
        let max = recent.iter().copied().fold(0.0, f64::max);
        let last = self.draws.back().map(|d| d.ms()).unwrap_or(0.0);
        (fps as u32, last, max)
    }
}

fn pct(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

pub fn summarize(r: &Recording, end: Instant) -> Value {
    let dur = end.duration_since(r.start).as_secs_f64();
    let mut draw: Vec<f64> = r.draws.iter().map(|d| d.ms()).collect();
    draw.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut present: Vec<f64> = r.presents.iter().map(|p| p.ms()).collect();
    present.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut intervals: Vec<f64> =
        r.presents.windows(2).map(|w| ms(w[1].end.saturating_duration_since(w[0].end))).collect();
    let over = |t: f64| intervals.iter().filter(|&&v| v > t).count();
    let (o16, o33, o50, o100) = (over(16.7), over(33.4), over(50.0), over(100.0));
    intervals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    // Total frame cost = Draw + the immediately following Present.
    let mut cost: Vec<f64> = Vec::new();
    let mut pi = 0;
    for d in &r.draws {
        while pi < r.presents.len() && r.presents[pi].start < d.end {
            pi += 1;
        }
        if pi < r.presents.len() {
            cost.push(d.ms() + r.presents[pi].ms());
        }
    }
    cost.sort_by(|a, b| a.partial_cmp(b).unwrap());
    json!({
        "name": r.name,
        "seconds": round3(dur),
        "draws": r.draws.len(),
        "presents": r.presents.len(),
        "fps": round3(r.presents.len() as f64 / dur.max(1e-9)),
        "draw_ms": {"p50": round3(pct(&draw, 0.5)), "p95": round3(pct(&draw, 0.95)), "max": round3(draw.last().copied().unwrap_or(0.0)), "mean": round3(draw.iter().sum::<f64>() / draw.len().max(1) as f64)},
        "present_ms": {"p50": round3(pct(&present, 0.5)), "p95": round3(pct(&present, 0.95)), "max": round3(present.last().copied().unwrap_or(0.0))},
        "frame_cost_ms": {"p50": round3(pct(&cost, 0.5)), "p95": round3(pct(&cost, 0.95)), "max": round3(cost.last().copied().unwrap_or(0.0))},
        "interval_ms": {"p50": round3(pct(&intervals, 0.5)), "p95": round3(pct(&intervals, 0.95)), "max": round3(intervals.last().copied().unwrap_or(0.0))},
        "intervals_over": {"16.7": o16, "33.4": o33, "50": o50, "100": o100},
    })
}
