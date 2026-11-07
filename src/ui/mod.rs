//! The Slint app shell and the 10-second refresh loop.

mod demo;
mod view;

use std::cell::RefCell;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use slint::{ComponentHandle, ModelRc, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode, VecModel};

use crate::brightness::{self, Controller, Levels};
use crate::config::{Config, Renderer};
use crate::display::{self, Change, Schedule, Screen};
use crate::health::{self, Health};
use crate::hermes::HermesReader;
use crate::metrics::{self, Metrics, Source};
use crate::reconcile;
use crate::store::views::Snapshot;
use crate::store::{self, Store};

slint::include_modules!();

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Live,
    /// `None` cycles through every scenario.
    Demo(Option<String>),
}

/// Events that brighten the screen.
const WAKE_EVENTS: &[&str] = &[
    "task_started",
    "task_finished",
    "feedback_needed",
    "queue_changed",
];

const TICK: Duration = Duration::from_secs(10);

struct App {
    config: Config,
    store: Option<Store>,
    store_path: PathBuf,
    store_error: Option<String>,
    db_stamp: Option<(Option<SystemTime>, Option<SystemTime>)>,
    hermes: Option<HermesReader>,
    demo: Option<demo::Demo>,
    source: Box<dyn Source>,
    metrics: Metrics,
    gpu_shown: bool,
    snapshot: Snapshot,
    health: Option<Health>,
    last_event_id: Option<i64>,
    brightness: Option<Controller>,
    /// Monitor power (live mode only).
    screen: Option<Screen>,
    schedule: Schedule,
}

pub fn run(
    config: &Config,
    mode: Mode,
    store_path: &Path,
    screenshot: Option<&Path>,
    windowed: bool,
) -> Result<()> {
    select_backend(config.ui.renderer);

    let window = MainWindow::new().context("creating the main window")?;
    // The Wayland app_id, for the compositor's logs and window rules.
    if let Err(e) = slint::set_xdg_app_id("hermes-beaver") {
        eprintln!("hermes-beaver: setting the app id failed: {e}");
    }
    let mut app = App::new(config.clone(), &mode, store_path, window.as_weak())?;
    let host = hostname();
    window.set_hostname(match mode {
        Mode::Live => host.into(),
        Mode::Demo(_) => format!("{host} · demo").into(),
    });
    window.set_mascot_title(view::mascot_title(&config.ui).into());
    window.set_mascot_font_size(view::mascot_font_px(config.ui.animal));
    tick_clock(window.as_weak());
    window.window().set_fullscreen(!windowed);

    window.on_quit(|| {
        let _ = slint::quit_event_loop();
    });
    let weak = window.as_weak();
    window.on_toggle_fullscreen(move || {
        if let Some(w) = weak.upgrade() {
            let fs = w.window().is_fullscreen();
            w.window().set_fullscreen(!fs);
        }
    });

    app.refresh(store::now());
    app.render(&window, store::now());
    app.sync_screen(store::now());
    let app = Rc::new(RefCell::new(app));

    window.on_user_input({
        let app = app.clone();
        move || {
            let a = &mut *app.borrow_mut();
            let woke = a.screen.as_mut().is_some_and(|s| s.input(store::now()));
            if woke && let Some(b) = &a.brightness {
                b.wake();
            }
            woke
        }
    });

    // First metrics shortly after start, then every 10 s.
    let tick = {
        let app = app.clone();
        let weak = window.as_weak();
        move || {
            if let Some(w) = weak.upgrade() {
                let mut a = app.borrow_mut();
                a.sample_metrics();
                let wake = a.refresh(store::now());
                a.render(&w, store::now());
                let change = a.sync_screen(store::now());
                // Events don't brighten a monitor in standby; turning it on does.
                let off = a.screen.as_ref().is_some_and(Screen::is_off);
                if (change == Change::TurnedOn || wake && !off)
                    && let Some(b) = &a.brightness
                {
                    b.wake();
                }
            }
        }
    };
    let first_timer = Timer::default();
    first_timer.start(TimerMode::SingleShot, Duration::from_secs(2), tick.clone());
    let tick_timer = Timer::default();
    tick_timer.start(TimerMode::Repeated, TICK, tick);

    let shot_timer = Timer::default();
    if let Some(path) = screenshot.map(Path::to_path_buf) {
        let weak = window.as_weak();
        shot_timer.start(
            TimerMode::SingleShot,
            Duration::from_millis(1500),
            move || {
                if let Some(w) = weak.upgrade() {
                    match w.window().take_snapshot() {
                        Ok(buf) => match write_ppm(&path, &buf) {
                            Ok(()) => {
                                eprintln!("hermes-beaver: snapshot saved to {}", path.display())
                            }
                            Err(e) => eprintln!("hermes-beaver: writing snapshot failed: {e}"),
                        },
                        Err(e) => eprintln!("hermes-beaver: snapshot failed: {e}"),
                    }
                }
                let _ = slint::quit_event_loop();
            },
        );
    }

    watch_signals();
    window.run().context("running the event loop")?;
    // Dropping the app restores brightness, wakes the monitor if it's in standby
    // and removes the demo store.
    drop(app);
    Ok(())
}

impl App {
    fn new(
        config: Config,
        mode: &Mode,
        store_path: &Path,
        window: slint::Weak<MainWindow>,
    ) -> Result<Self> {
        let base = |store, store_error, source: Box<dyn Source>, metrics, gpu_shown| Self {
            config: config.clone(),
            store,
            store_path: store_path.to_path_buf(),
            store_error,
            db_stamp: None,
            hermes: None,
            demo: None,
            source,
            metrics,
            gpu_shown,
            snapshot: Snapshot::default(),
            health: None,
            last_event_id: None,
            brightness: None,
            screen: None,
            schedule: config.schedule(),
        };
        Ok(match mode {
            Mode::Live => {
                let (store, err) = match Store::open(store_path) {
                    Ok(s) => (Some(s), None),
                    Err(e) => (None, Some(format!("cannot open the event store: {e:#}"))),
                };
                let source = metrics::SystemSource::new(&config.metrics.disk);
                let mut app = base(
                    store,
                    err,
                    Box::new(source),
                    Metrics::default(),
                    metrics::gpu::available(),
                );
                app.hermes = Some(HermesReader::new(
                    config.hermes_home(),
                    config.hermes.agent_jobs.clone(),
                ));
                let method = config.brightness.method;
                let dim = brightness::Dim::new(move |opacity| {
                    let _ = window.upgrade_in_event_loop(move |w| w.set_dim(opacity));
                });
                app.brightness = Some(Controller::start(
                    Levels::from(&config.brightness),
                    move |pause| brightness::select(method, dim, pause),
                ));
                app.screen = Some(Screen::new(Box::new(display::OutputPower)));
                app
            }
            Mode::Demo(scenario) => {
                let fixed = scenario.as_deref().map(demo::Scenario::parse).transpose()?;
                let (demo, store) = demo::Demo::new(fixed)?;
                let mut source = demo::DemoSource::new();
                let mut m = Metrics::default();
                demo::prefill(&mut m, &mut source);
                let mut app = base(Some(store), None, Box::new(source), m, true);
                app.demo = Some(demo);
                app
            }
        })
    }

    fn sample_metrics(&mut self) {
        let s = self.source.sample();
        self.metrics.record(s);
    }

    /// Re-reads health and (if it changed) the store. Returns whether to wake the screen.
    fn refresh(&mut self, now: i64) -> bool {
        let mut force_reload = false;

        // Health (and reconciliation, which needs the same ledger).
        let mut health = if let Some(demo) = &mut self.demo {
            if let Some(store) = &mut self.store {
                match demo.advance(store) {
                    Ok(true) => force_reload = true,
                    Ok(false) => {}
                    Err(e) => eprintln!("hermes-beaver: demo: {e:#}"),
                }
            }
            demo.health()
        } else if let Some(hermes) = &mut self.hermes {
            let st = hermes.poll();
            if let Some(store) = &mut self.store {
                match reconcile::reconcile(store, &st.executions, now) {
                    Ok(0) => {}
                    Ok(n) => {
                        eprintln!("hermes-beaver: reconciled {n} task(s) from the cron ledger");
                        force_reload = true;
                    }
                    Err(e) => eprintln!("hermes-beaver: reconcile: {e:#}"),
                }
            }
            health::evaluate(
                &st,
                Duration::from_secs(self.config.hermes.heartbeat_max_age_secs),
                !self.config.hermes.agent_jobs.is_empty(),
            )
        } else {
            Health::default()
        };
        if let Some(e) = &self.store_error {
            health.problems.push(e.clone());
        }
        let mut wake = matches!(&self.health, Some(old) if *old != health);
        self.health = Some(health);

        // Store: re-query only when another process (or we) wrote to it.
        let Some(store) = &self.store else {
            return wake;
        };
        let stamp = db_stamp(&self.store_path);
        if force_reload || self.demo.is_some() || self.db_stamp != Some(stamp) {
            match store.snapshot(self.config.tasks.finished_rows) {
                Ok(snap) => {
                    self.snapshot = snap;
                    self.db_stamp = Some(stamp);
                }
                Err(e) => eprintln!("hermes-beaver: reading the store: {e:#}"),
            }
        }
        let latest = self.snapshot.last_event_id;
        if let Some(seen) = self.last_event_id
            && latest > seen
        {
            match store.event_kinds_since(seen) {
                Ok(kinds) => wake |= kinds.iter().any(|k| WAKE_EVENTS.contains(&k.as_str())),
                Err(e) => eprintln!("hermes-beaver: reading events: {e:#}"),
            }
        }
        self.last_event_id = Some(latest);
        wake
    }

    /// Puts the monitor into standby or wakes it, following the owner's last request
    /// and the daily schedule.
    fn sync_screen(&mut self, now: i64) -> Change {
        let want_off = display::wants_off(&self.snapshot.display, &self.schedule, now);
        match &mut self.screen {
            Some(s) => s.sync(want_off, now),
            None => Change::None,
        }
    }

    fn render(&self, w: &MainWindow, now: i64) {
        let snap = &self.snapshot;
        let health = self.health.clone().unwrap_or_default();
        let stale = self.config.tasks.stale_after_mins as i64 * 60;

        let (tasks, tasks_hint) = view::task_items(snap, now, stale);
        let (needs, needs_hint) = view::need_items(snap, now);
        let done_rows = view::done_rows(
            needs.len(),
            !needs_hint.is_empty(),
            self.config.tasks.finished_rows,
        );
        let (done, done_hint) = view::done_items(snap, now, done_rows);
        let mood = view::mood(&health, snap);
        let (caption, tone) = view::mascot_caption(mood, snap);

        w.set_banner(health.banner().into());
        w.set_tasks(model(tasks));
        w.set_tasks_hint(tasks_hint.into());
        w.set_needs(model(needs));
        w.set_needs_hint(needs_hint.into());
        w.set_done(model(done));
        w.set_done_hint(done_hint.into());
        w.set_metrics(model(view::metric_items(
            &self.metrics,
            self.gpu_shown,
            view::loadavg(),
        )));
        w.set_mascot_art(view::mascot_art(self.config.ui.animal, mood).into());
        w.set_mascot_caption(caption.into());
        w.set_mascot_tone(tone);
        w.set_counter(snap.done_count.to_string().into());
        w.set_counter_label(view::counter_label(snap.go_live_at).into());
    }
}

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::from(Rc::new(VecModel::from(v)))
}

fn db_stamp(db: &Path) -> (Option<SystemTime>, Option<SystemTime>) {
    let m = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    (m(db), m(Path::new(&wal)))
}

/// SIGTERM/SIGINT/SIGHUP end the event loop cleanly, so brightness and monitor power are restored.
fn watch_signals() {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let Ok(mut signals) = signal_hook::iterator::Signals::new([SIGTERM, SIGINT, SIGHUP]) else {
        return;
    };
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            if signals.forever().next().is_some() {
                let _ = slint::invoke_from_event_loop(|| {
                    let _ = slint::quit_event_loop();
                });
            }
        })
        .ok();
}

/// Renderer from the config; `SLINT_BACKEND` in the environment overrides it.
fn select_backend(renderer: Renderer) {
    if std::env::var_os("SLINT_BACKEND").is_some() {
        return;
    }
    let name = match renderer {
        Renderer::Software => "software",
        Renderer::Opengl => "femtovg",
    };
    let chosen = slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name(name.into())
        .select();
    if let Err(e) = chosen {
        eprintln!("hermes-beaver: renderer `{name}` unavailable ({e}); using the default");
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|_| "hermes".into())
}

/// Shows the time and schedules itself just after the next wall-clock second, so the
/// seconds tick evenly and never skip. The ticking seconds show at a glance that the
/// dashboard is alive: the 10 s refresh runs on the same thread.
fn tick_clock(weak: slint::Weak<MainWindow>) {
    let Some(w) = weak.upgrade() else { return };
    let now = chrono::Local::now();
    let (clock, seconds) = clock_parts(&now);
    if w.get_clock() != clock.as_str() {
        w.set_clock(clock.into());
    }
    w.set_seconds(seconds.into());
    let wait = 1_000 - u64::from(now.timestamp_subsec_millis().min(999)) + 20;
    Timer::single_shot(Duration::from_millis(wait), move || tick_clock(weak));
}

/// "HH:MM" and ":SS".
fn clock_parts<Tz: chrono::TimeZone>(t: &chrono::DateTime<Tz>) -> (String, String)
where
    Tz::Offset: std::fmt::Display,
{
    (t.format("%H:%M").to_string(), t.format(":%S").to_string())
}

/// Writes an RGBA buffer as binary PPM (alpha dropped). No image crate needed.
fn write_ppm(path: &Path, buf: &SharedPixelBuffer<Rgba8Pixel>) -> std::io::Result<()> {
    let mut out = Vec::with_capacity(32 + buf.as_slice().len() * 3);
    write!(out, "P6\n{} {}\n255\n", buf.width(), buf.height())?;
    for p in buf.as_slice() {
        out.extend_from_slice(&[p.r, p.g, p.b]);
    }
    std::fs::write(path, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::platform::{Platform, WindowAdapter};
    use slint::{PhysicalSize, Rgb8Pixel};

    const W: u32 = 1920;
    const H: u32 = 1080;

    struct Headless(Rc<MinimalSoftwareWindow>);

    impl Platform for Headless {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }

    fn frame(win: &MinimalSoftwareWindow, buf: &mut [Rgb8Pixel]) {
        win.request_redraw();
        win.draw_if_needed(|r| {
            r.render(buf, W as usize);
        });
    }

    #[test]
    fn clock_parts_split_minutes_and_seconds() {
        let t = chrono::DateTime::parse_from_rfc3339("2026-11-04T00:54:07+01:00").unwrap();
        assert_eq!(clock_parts(&t), ("00:54".into(), ":07".into()));
    }

    /// Like the dashboard on the host: the partial software renderer draws only what
    /// changed into a reused buffer, other text changes every few seconds and the clock
    /// once a minute. After a while the clock must still repaint (slint-ui/slint#14435:
    /// the property changed, the screen kept the old time).
    #[test]
    fn clock_keeps_repainting_after_many_other_text_updates() {
        let win = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        slint::platform::set_platform(Box::new(Headless(win.clone()))).unwrap();
        let ui = MainWindow::new().unwrap();
        ui.show().unwrap();
        win.set_size(PhysicalSize::new(W, H));
        let mut buf = vec![Rgb8Pixel::default(); (W * H) as usize];

        ui.set_clock("23:40".into());
        frame(&win, &mut buf);
        ui.set_clock("23:41".into());
        frame(&win, &mut buf);
        // The text cache sweeps after 1024 shapings; on the host that took about 4 minutes.
        for i in 0..1100 {
            ui.set_counter(i.to_string().into());
            frame(&win, &mut buf);
        }
        let before = buf.clone();
        ui.set_clock("00:54".into());
        frame(&win, &mut buf);
        let changed = buf.iter().zip(&before).filter(|(a, b)| a != b).count();
        assert!(changed > 0, "the clock changed but no pixel did");

        // The same state drawn from scratch.
        let mut fresh = vec![Rgb8Pixel::default(); (W * H) as usize];
        win.request_redraw();
        win.draw_if_needed(|r| {
            r.set_repaint_buffer_type(RepaintBufferType::NewBuffer);
            r.render(&mut fresh, W as usize);
        });
        let differing = buf.iter().zip(&fresh).filter(|(a, b)| a != b).count();
        assert_eq!(differing, 0, "incremental frame differs from a full redraw");
    }
}
