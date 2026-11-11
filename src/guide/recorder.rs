//! Recording a guide: every click inside the chosen region becomes a step,
//! with a screenshot taken the moment the button goes down (before the
//! click has changed anything) and a marker where it landed. Typing becomes
//! one step per field: its screenshot is retaken once the typing pauses, so
//! it shows what was typed. Shortcuts (⌘C, Ctrl+S) become steps of their own.
//!
//! Three threads: the input listener, a capture thread that turns events
//! into steps and grabs the screen at once, and an encoder that shrinks and
//! compresses each screenshot, so a slow encode never delays a capture.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use image::RgbaImage;

use super::input::{self, Input, Key, Mods};
use super::{Action, Button, Shot, Step};

pub struct GuideConfig {
    /// The monitor's top-left in global logical points.
    pub monitor_origin: (i32, i32),
    /// The recorded region, monitor-local logical points (x, y, w, h).
    pub region: (u32, u32, u32, u32),
    /// Rega's own global shortcuts (global-hotkey syntax): pressing one to
    /// stop isn't a step.
    pub ignore_shortcuts: Vec<String>,
    /// Screenshots are shrunk to this on their long side (`None`: kept whole).
    pub max_side: Option<u32>,
    pub record_typing: bool,
    /// Keep what was typed; off, typing steps only say to type.
    pub typed_text: bool,
    /// Shortcuts and lone key presses (Enter, Esc) become steps.
    pub record_keys: bool,
}

/// Which inputs become steps.
#[derive(Clone, Copy)]
struct Recording {
    typing: bool,
    keys: bool,
}

/// After typing stops for this long, the screenshot is retaken to show it.
const TYPING_SETTLED: Duration = Duration::from_millis(600);
/// A second press this soon, this close, makes a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(500);
const DOUBLE_CLICK_SLOP: f64 = 6.0;

struct Draft {
    id: u64,
    action: Action,
    marker: Option<[f32; 2]>,
    shot: Option<Arc<Shot>>,
}

/// A screenshot for the encoder, and the steps that show it.
struct Job {
    ids: Vec<u64>,
    image: RgbaImage,
}

pub struct GuideRecorder {
    drafts: Arc<Mutex<Vec<Draft>>>,
    stop: Arc<AtomicBool>,
    /// A global rectangle (logical points) whose clicks aren't steps: Rega's bar.
    exclude: Arc<Mutex<Option<[f32; 4]>>>,
    started: Instant,
    listener: Option<input::Listener>,
    capture: Option<JoinHandle<()>>,
    encoder: Option<JoinHandle<()>>,
    typed_text: bool,
}

impl GuideRecorder {
    pub fn start(cfg: GuideConfig) -> Result<Self> {
        crate::capture::monitor_with_origin(cfg.monitor_origin).context("the screen to record wasn't found")?;
        let (input_tx, input_rx) = mpsc::channel();
        let listener = input::listen(input_tx)?;
        let drafts: Arc<Mutex<Vec<Draft>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let exclude = Arc::new(Mutex::new(None));
        let (job_tx, job_rx) = mpsc::channel::<Job>();

        let encoder = {
            let drafts = drafts.clone();
            let max_side = cfg.max_side;
            thread::spawn(move || {
                for job in job_rx {
                    match Shot::encode(&super::fit_capture(job.image, max_side)) {
                        Ok(shot) => {
                            let mut drafts = drafts.lock().unwrap();
                            for d in drafts.iter_mut().filter(|d| job.ids.contains(&d.id)) {
                                d.shot = Some(shot.clone());
                            }
                        }
                        Err(e) => log::warn!("guide screenshot: {e:#}"),
                    }
                }
            })
        };
        let capture = {
            let mut state = Capturer {
                origin: cfg.monitor_origin,
                region: cfg.region,
                ignore: cfg.ignore_shortcuts,
                record: Recording { typing: cfg.record_typing, keys: cfg.record_keys },
                drafts: drafts.clone(),
                exclude: exclude.clone(),
                jobs: job_tx,
                next_id: 0,
                typing: None,
                last_click: None,
                focus: None,
            };
            let stop = stop.clone();
            thread::spawn(move || state.run(input_rx, &stop))
        };
        Ok(Self {
            drafts,
            stop,
            exclude,
            started: Instant::now(),
            listener: Some(listener),
            capture: Some(capture),
            encoder: Some(encoder),
            typed_text: cfg.typed_text,
        })
    }

    pub fn step_count(&self) -> usize {
        self.drafts.lock().map(|d| d.len()).unwrap_or(0)
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Clicks inside `rect` (global logical points) aren't steps.
    pub fn set_exclude(&self, rect: [f32; 4]) {
        *self.exclude.lock().unwrap() = Some(rect);
    }

    /// Stop and hand over the steps, once their screenshots are encoded.
    pub fn finish(mut self) -> Vec<Step> {
        self.shut_down();
        let drafts = std::mem::take(&mut *self.drafts.lock().unwrap());
        let typed_text = self.typed_text;
        drafts
            .into_iter()
            .filter_map(|mut d| {
                let shot = d.shot?;
                if !typed_text && let Action::Type { text, .. } = &mut d.action {
                    // Never kept: the caption only says to type.
                    text.clear();
                }
                Some(Step { title: d.action.default_title(), shot, action: d.action, marker: d.marker, marker_hidden: false, note: String::new() })
            })
            .collect()
    }

    fn shut_down(&mut self) {
        if let Some(listener) = self.listener.take() {
            listener.stop();
        }
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.capture.take() {
            let _ = t.join();
        }
        if let Some(t) = self.encoder.take() {
            let _ = t.join();
        }
    }
}

impl Drop for GuideRecorder {
    fn drop(&mut self) {
        self.shut_down();
    }
}

struct Typing {
    id: u64,
    last_key: Instant,
    /// The screenshot shows the text as it is now.
    settled: bool,
}

struct Capturer {
    origin: (i32, i32),
    region: (u32, u32, u32, u32),
    ignore: Vec<String>,
    record: Recording,
    drafts: Arc<Mutex<Vec<Draft>>>,
    exclude: Arc<Mutex<Option<[f32; 4]>>>,
    jobs: Sender<Job>,
    next_id: u64,
    typing: Option<Typing>,
    /// The last click step: when, where (global points), which button.
    last_click: Option<(Instant, (f64, f64), Button, u64)>,
    /// Where the field being typed into probably is: the last click, until
    /// Tab or Enter moves on.
    focus: Option<[f32; 2]>,
}

impl Capturer {
    fn run(&mut self, rx: Receiver<Input>, stop: &AtomicBool) {
        let Some(monitor) = crate::capture::monitor_with_origin(self.origin) else { return };
        let d = crate::capture::dpi(&monitor);
        let phys = |v: u32| (v as f32 * d).round() as u32;
        let (x, y, w, h) = self.region;
        let (px, py, pw, ph) = (phys(x), phys(y), phys(w).max(2), phys(h).max(2));
        let grab = || match monitor.capture_region(px, py, pw, ph) {
            Ok(img) => Some(img),
            Err(e) => {
                log::warn!("guide capture: {e}");
                None
            }
        };
        while !stop.load(Ordering::Relaxed) {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(input) => self.handle(input, &grab),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if self.typing.as_ref().is_some_and(|t| !t.settled && t.last_key.elapsed() >= TYPING_SETTLED) {
                self.retake_typing(&grab);
            }
        }
        // Events that came in just before Stop.
        while let Ok(input) = rx.try_recv() {
            self.handle(input, &grab);
        }
        if self.typing.as_ref().is_some_and(|t| !t.settled) {
            self.retake_typing(&grab);
        }
    }

    fn handle(&mut self, input: Input, grab: &dyn Fn() -> Option<RgbaImage>) {
        match input {
            Input::Down { x, y, button } => self.click(x, y, button, grab),
            Input::Key { key, mods, name, repeat } => self.key(key, mods, &name, repeat, grab),
        }
    }

    /// A position (global points) as fractions of the region, if inside it.
    fn to_region(&self, gx: f64, gy: f64) -> Option<[f32; 2]> {
        let (x, y, w, h) = self.region;
        let fx = (gx - self.origin.0 as f64 - x as f64) / w.max(1) as f64;
        let fy = (gy - self.origin.1 as f64 - y as f64) / h.max(1) as f64;
        ((0.0..=1.0).contains(&fx) && (0.0..=1.0).contains(&fy)).then_some([fx as f32, fy as f32])
    }

    fn click(&mut self, x: f64, y: f64, button: Button, grab: &dyn Fn() -> Option<RgbaImage>) {
        // Windows reports physical pixels; macOS already points.
        let (gx, gy) = if cfg!(windows) {
            let (lx, ly) = crate::capture::to_logical((x.round() as i32, y.round() as i32));
            (lx as f64, ly as f64)
        } else {
            (x, y)
        };
        let excluded = self.exclude.lock().ok().and_then(|e| *e).is_some_and(|[ex, ey, ew, eh]| {
            gx >= ex as f64 && gx <= (ex + ew) as f64 && gy >= ey as f64 && gy <= (ey + eh) as f64
        });
        let Some(marker) = self.to_region(gx, gy).filter(|_| !excluded) else {
            // Outside what's recorded (or on Rega's bar): not a step, but
            // whatever was being typed is finished.
            self.end_typing(grab, None);
            return;
        };
        if let Some((at, (lx, ly), b, id)) = self.last_click
            && b == button
            && at.elapsed() < DOUBLE_CLICK
            && (lx - gx).hypot(ly - gy) < DOUBLE_CLICK_SLOP
            && self.typing.is_none()
        {
            self.update(id, |d| {
                if let Action::Click { count, .. } = &mut d.action {
                    *count += 1;
                }
            });
            self.last_click = Some((Instant::now(), (gx, gy), button, id));
            return;
        }
        let Some(image) = grab() else { return };
        let id = self.push(Action::Click { button, count: 1 }, Some(marker));
        // Unless the typing's screenshot already shows the final text, this
        // one (taken before the click changes anything) does.
        let mut ids = vec![id];
        if let Some(t) = self.typing.take()
            && !t.settled
        {
            ids.push(t.id);
        }
        let _ = self.jobs.send(Job { ids, image });
        self.last_click = Some((Instant::now(), (gx, gy), button, id));
        self.focus = Some(marker);
    }

    fn key(&mut self, key: Key, mods: Mods, name: &str, repeat: bool, grab: &dyn Fn() -> Option<RgbaImage>) {
        if mods.is_shortcut() || (mods.alt && !matches!(key, Key::Text(_))) {
            if !self.record.keys || repeat || self.ignore.iter().any(|s| matches_shortcut(s, mods, name)) {
                return;
            }
            self.end_typing(grab, None);
            self.focus = None;
            self.key_step(input::combo(mods, name), grab);
            return;
        }
        match key {
            Key::Text(_) | Key::Backspace if !self.record.typing => {}
            Key::Text(text) => {
                if let Some(t) = &mut self.typing {
                    t.last_key = Instant::now();
                    t.settled = false;
                    let id = t.id;
                    self.update(id, |d| {
                        if let Action::Type { text: typed, .. } = &mut d.action {
                            typed.push_str(&text);
                        }
                    });
                } else {
                    // The field before anything is typed; retaken once typing pauses.
                    let Some(image) = grab() else { return };
                    let id = self.push(Action::Type { text, then: None }, self.focus);
                    let _ = self.jobs.send(Job { ids: vec![id], image });
                    self.typing = Some(Typing { id, last_key: Instant::now(), settled: false });
                }
            }
            Key::Backspace => {
                if let Some(t) = &mut self.typing {
                    t.last_key = Instant::now();
                    t.settled = false;
                    let id = t.id;
                    self.update(id, |d| {
                        if let Action::Type { text, .. } = &mut d.action {
                            text.pop();
                        }
                    });
                }
            }
            Key::Enter | Key::Tab if !repeat => {
                let then = if key == Key::Enter { "Enter" } else { "Tab" };
                if self.typing.is_some() {
                    self.end_typing(grab, Some(then));
                } else if key == Key::Enter && self.record.keys {
                    self.key_step(input::combo(mods, name), grab);
                }
                self.focus = None;
            }
            Key::Escape if !repeat => {
                self.end_typing(grab, None);
                self.focus = None;
                if self.record.keys {
                    self.key_step(input::combo(mods, name), grab);
                }
            }
            // Arrows and the like move around inside what's typed; holding
            // a key repeats it.
            _ => {}
        }
    }

    fn key_step(&mut self, combo: String, grab: &dyn Fn() -> Option<RgbaImage>) {
        let Some(image) = grab() else { return };
        let id = self.push(Action::Key { combo }, None);
        let _ = self.jobs.send(Job { ids: vec![id], image });
    }

    /// Finish the typing step, if any: retake its screenshot now (it shows
    /// the text) unless that's already done, and note the key that ended it.
    fn end_typing(&mut self, grab: &dyn Fn() -> Option<RgbaImage>, then: Option<&str>) {
        let Some(t) = self.typing.take() else { return };
        if let Some(key) = then {
            self.update(t.id, |d| {
                if let Action::Type { then, .. } = &mut d.action {
                    *then = Some(key.to_owned());
                }
            });
        }
        let empty = self.with_draft(t.id, |d| matches!(&d.action, Action::Type { text, .. } if text.is_empty())).unwrap_or(false);
        if empty {
            // Typed, then deleted it all: nothing to show.
            self.drafts.lock().unwrap().retain(|d| d.id != t.id);
            return;
        }
        if !t.settled
            && let Some(image) = grab()
        {
            let _ = self.jobs.send(Job { ids: vec![t.id], image });
        }
    }

    fn retake_typing(&mut self, grab: &dyn Fn() -> Option<RgbaImage>) {
        let Some(t) = &mut self.typing else { return };
        t.settled = true;
        let id = t.id;
        if let Some(image) = grab() {
            let _ = self.jobs.send(Job { ids: vec![id], image });
        }
    }

    fn push(&mut self, action: Action, marker: Option<[f32; 2]>) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.drafts.lock().unwrap().push(Draft { id, action, marker, shot: None });
        id
    }

    fn update(&self, id: u64, f: impl FnOnce(&mut Draft)) {
        if let Some(d) = self.drafts.lock().unwrap().iter_mut().find(|d| d.id == id) {
            f(d);
        }
    }

    fn with_draft<R>(&self, id: u64, f: impl FnOnce(&Draft) -> R) -> Option<R> {
        self.drafts.lock().unwrap().iter().find(|d| d.id == id).map(f)
    }
}

/// Does a key press match a shortcut in global-hotkey syntax, such as
/// `control+alt+KeyG`?
fn matches_shortcut(shortcut: &str, mods: Mods, name: &str) -> bool {
    if shortcut.is_empty() {
        return false;
    }
    let mut want = Mods::default();
    let mut key = None;
    for part in shortcut.split('+') {
        match part.to_ascii_lowercase().as_str() {
            "control" | "ctrl" => want.ctrl = true,
            "alt" | "option" => want.alt = true,
            "shift" => want.shift = true,
            "super" | "cmd" | "command" | "meta" => want.cmd = true,
            _ => key = Some(part),
        }
    }
    let Some(key) = key else { return false };
    let key = key.strip_prefix("Key").or_else(|| key.strip_prefix("Digit")).unwrap_or(key);
    want == mods && key.eq_ignore_ascii_case(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_shortcuts_are_recognised() {
        let mods = Mods { ctrl: true, alt: true, ..Default::default() };
        assert!(matches_shortcut("control+alt+KeyG", mods, "G"));
        assert!(!matches_shortcut("control+alt+KeyG", mods, "H"));
        assert!(!matches_shortcut("control+alt+KeyG", Mods { shift: true, ..mods }, "G"));
        assert!(matches_shortcut("control+alt+Digit5", mods, "5"));
        assert!(!matches_shortcut("", mods, "G"));
    }

    /// Clicks, typing and shortcuts turn into the steps a person would
    /// write down, with screenshots: the screen is a fake that counts grabs.
    fn capturer(record: Recording, jobs: Sender<Job>) -> Capturer {
        Capturer {
            origin: (0, 0),
            region: (100, 100, 400, 300),
            ignore: vec!["control+alt+KeyG".into()],
            record,
            drafts: Arc::new(Mutex::new(Vec::new())),
            exclude: Arc::new(Mutex::new(Some([100.0, 380.0, 400.0, 20.0]))),
            jobs,
            next_id: 0,
            typing: None,
            last_click: None,
            focus: None,
        }
    }

    /// With typing or keys switched off in Settings, they aren't steps.
    #[test]
    fn switched_off_inputs_are_skipped() {
        let (tx, _rx) = mpsc::channel();
        let mut c = capturer(Recording { typing: false, keys: false }, tx);
        let grab = || Some(RgbaImage::new(4, 4));
        let none = Mods::default();
        for input in [
            Input::Down { x: 300.0, y: 160.0, button: Button::Left },
            Input::Key { key: Key::Text("a".into()), mods: none, name: "A".into(), repeat: false },
            Input::Key { key: Key::Enter, mods: none, name: "Return".into(), repeat: false },
            Input::Key { key: Key::Text("c".into()), mods: Mods { cmd: true, ..none }, name: "C".into(), repeat: false },
            Input::Key { key: Key::Escape, mods: none, name: "Esc".into(), repeat: false },
        ] {
            c.handle(input, &grab);
        }
        let drafts = c.drafts.lock().unwrap();
        assert_eq!(drafts.iter().map(|d| &d.action).collect::<Vec<_>>(), [&Action::Click { button: Button::Left, count: 1 }]);
    }

    #[test]
    fn inputs_become_steps() {
        let (job_tx, job_rx) = mpsc::channel();
        let mut c = capturer(Recording { typing: true, keys: true }, job_tx);
        let grabs = std::cell::Cell::new(0);
        let grab = || {
            grabs.set(grabs.get() + 1);
            Some(RgbaImage::new(4, 4))
        };
        let none = Mods::default();
        let key = |k: Key, name: &str| Input::Key { key: k, mods: none, name: name.into(), repeat: false };
        let text = |s: &str| key(Key::Text(s.into()), s);
        let click = |x: f64, y: f64| Input::Down { x, y, button: Button::Left };
        for input in [
            click(300.0, 160.0),
            text("h"),
            text("i"),
            text("x"),
            key(Key::Backspace, "Delete"),
            key(Key::Enter, "Return"),
            click(50.0, 50.0),                // outside: not a step
            click(300.0, 390.0),              // on Rega's bar: not a step
            Input::Key { key: Key::Text("c".into()), mods: Mods { cmd: true, ..none }, name: "C".into(), repeat: false },
            Input::Key { key: Key::Other, mods: Mods { ctrl: true, alt: true, ..none }, name: "G".into(), repeat: false },
            click(200.0, 200.0),
            click(201.0, 200.0),              // the second half of a double-click
        ] {
            c.handle(input, &grab);
        }
        let drafts = c.drafts.lock().unwrap();
        let actions: Vec<&Action> = drafts.iter().map(|d| &d.action).collect();
        assert_eq!(
            actions,
            [
                &Action::Click { button: Button::Left, count: 1 },
                &Action::Type { text: "hi".into(), then: Some("Enter".into()) },
                &Action::Key { combo: input::combo(Mods { cmd: true, ..none }, "C") },
                &Action::Click { button: Button::Left, count: 2 },
            ]
        );
        assert_eq!(drafts[0].marker, Some([0.5, 0.2]));
        assert_eq!(drafts[1].marker, Some([0.5, 0.2]), "typing is marked where the field was clicked");
        assert_eq!(drafts[2].marker, None);
        // Click, typing (first key, then again on Enter), shortcut, click.
        assert_eq!(grabs.get(), 5);
        let jobs: Vec<Vec<u64>> = job_rx.try_iter().map(|j| j.ids).collect();
        assert_eq!(jobs, vec![vec![1], vec![2], vec![2], vec![3], vec![4]]);
    }
}
