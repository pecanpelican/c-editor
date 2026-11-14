//! Frame timing for performance work: set `CEDIT_PERF=1` and cedit repaints
//! continuously and prints frame statistics to stderr once a second.

use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Instant;

struct Stats {
    last_frame: Option<Instant>,
    window_start: Instant,
    frames: u32,
    ui_total: f64,
    ui_max: f64,
    gap_total: f64,
    gap_max: f64,
}

fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("CEDIT_PERF").is_some_and(|v| !v.is_empty() && v != "0"))
}

static START: OnceLock<Instant> = OnceLock::new();
static STATS: Mutex<Option<Stats>> = Mutex::new(None);

/// Call at the end of each frame's UI code with the time the frame started.
pub fn frame_done(ctx: &egui::Context, start: Instant) {
    if !enabled() {
        return;
    }
    START.get_or_init(|| start);
    let now = Instant::now();
    let ui_ms = (now - start).as_secs_f64() * 1000.0;
    let mut guard = STATS.lock().unwrap_or_else(|e| e.into_inner());
    let s = guard.get_or_insert_with(|| Stats {
        last_frame: None,
        window_start: now,
        frames: 0,
        ui_total: 0.0,
        ui_max: 0.0,
        gap_total: 0.0,
        gap_max: 0.0,
    });
    if let Some(last) = s.last_frame {
        let gap = (start - last).as_secs_f64() * 1000.0;
        s.gap_total += gap;
        s.gap_max = s.gap_max.max(gap);
    }
    s.last_frame = Some(now);
    s.frames += 1;
    s.ui_total += ui_ms;
    s.ui_max = s.ui_max.max(ui_ms);
    if (now - s.window_start).as_secs_f64() >= 1.0 {
        let n = s.frames.max(1) as f64;
        eprintln!(
            "[perf] t={:.1}s {} frames  ui avg {:.2} ms max {:.2} ms  outside-ui avg {:.2} ms max {:.2} ms",
            (now - *START.get_or_init(Instant::now)).as_secs_f64(),
            s.frames,
            s.ui_total / n,
            s.ui_max,
            s.gap_total / n,
            s.gap_max
        );
        *s = Stats {
            last_frame: s.last_frame,
            window_start: now,
            frames: 0,
            ui_total: 0.0,
            ui_max: 0.0,
            gap_total: 0.0,
            gap_max: 0.0,
        };
    }
    ctx.request_repaint();
}

/// Times a block when `CEDIT_PERF` is set; logs it if it took over 4 ms.
pub struct Scope(&'static str, Option<Instant>);

pub fn scope(name: &'static str) -> Scope {
    Scope(name, enabled().then(Instant::now))
}

impl Drop for Scope {
    fn drop(&mut self) {
        if let Some(t) = self.1 {
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            if ms > 4.0 {
                eprintln!("[perf]   slow: {} {:.1} ms", self.0, ms);
            }
        }
    }
}
