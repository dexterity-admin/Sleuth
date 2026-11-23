//! How many reader processes may run right now.
//!
//! No I/O happens here: the caller measures the PC ([`crate::meters`]), passes
//! the readings in with the time, and gets the number of workers per lane and
//! a sentence for the status bar.
//!
//! While someone is at the PC (keyboard or mouse used in the last five
//! minutes) automatic mode reads at the minimum: 1 OCR worker and 2 text
//! workers, and readers beyond that are stopped at once (`Targets::cut`),
//! not after the document they are on. Up to 1.3 it read on half the cores
//! with someone there and let extra readers finish their document, which
//! for a scanned set could be an hour: people's PCs were slow all day. Once
//! nobody is there it climbs, doubling every 30 s, to all cores but one.

use std::time::{Duration, Instant};

/// The OCR level doubles each time this long passes without the PC being busy.
const RAMP: Duration = Duration::from_secs(30);
/// Never fewer than this while there is work and reading is not paused.
const FLOOR_OCR: u32 = 1;
const FLOOR_FAST: u32 = 2;
/// Text extraction is mostly disk-bound; more than this only queues on the disk.
const MAX_FAST: u32 = 16;
/// Automatic mode stays under this many text workers: they load the disks
/// and the file server, which the rest of the office shares.
const AUTO_MAX_FAST: u32 = 8;
/// Memory left for the user's own programs whatever the mode.
const KEEP_FREE_MB: u64 = 1024;
/// Busy CPU counts only while someone has used the PC this recently: a
/// rendering job left running overnight is not a person waiting.
const BUSY_WITHIN_SECS: u64 = 120;
/// Longer than this without input and nobody is at the PC.
const AWAY_SECS: u64 = 300;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Speed {
    Auto,
    Gentle,
    Full,
}

#[derive(Clone, Debug)]
pub struct Inputs {
    pub cores: usize,
    /// Percent of all cores used by other programs (from `CpuMeter`).
    pub others_cpu: f32,
    pub idle_secs: u64,
    pub free_ram_mb: u64,
    pub on_battery: bool,
    /// An always-on server: never throttled, never battery-paused.
    pub server: bool,
    pub speed: Speed,
    /// Setting, default 60.
    pub busy_cpu_percent: u32,
    /// Paused by the user.
    pub paused: bool,
    /// Memory one OCR worker needs (the engine supplies it, e.g. 600).
    pub ocr_worker_mb: u64,
    /// Work is waiting in the lane.
    pub fast_backlog: bool,
    pub ocr_backlog: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Targets {
    pub fast: u32,
    pub ocr: u32,
    /// "reading", "paused", "paused: on battery" or "idle".
    pub state: String,
    /// One plain sentence for the status bar: what is happening and why.
    pub note: String,
    /// Readers beyond these numbers are to stop now, in the middle of a
    /// document (it is read again later): someone is at the PC, or reading
    /// was paused or set to the minimum.
    pub cut: bool,
}

/// Why the numbers are what they are; chooses the note.
enum Why {
    Gentle,
    Full,
    Busy,
    Server,
    Away,
    Ramping,
    Present,
}

pub struct Governor {
    /// The OCR level of automatic mode: 1, 2, 4, ... up to the ceiling.
    level: u32,
    /// When the level last changed, or the PC was last busy; the next
    /// doubling is `RAMP` after this. `None` while automatic mode is not
    /// in charge, so time spent paused does not count as time proven free.
    since: Option<Instant>,
}

impl Default for Governor {
    fn default() -> Governor {
        Governor::new()
    }
}

impl Governor {
    pub fn new() -> Governor {
        Governor { level: FLOOR_OCR, since: None }
    }

    pub fn step(&mut self, i: &Inputs, now: Instant) -> Targets {
        if i.paused {
            self.since = None;
            return stopped("paused", "Reading is paused on this PC until you resume it.");
        }
        if i.on_battery && !i.server {
            self.since = None;
            return stopped("paused: on battery", "Reading is paused while this PC runs on battery.");
        }
        let cores = u32::try_from(i.cores).unwrap_or(u32::MAX).max(1);
        let mut cut = false;
        let (wanted, fast, why) = match i.speed {
            Speed::Gentle => {
                cut = true;
                self.since = None;
                (FLOOR_OCR, FLOOR_FAST, Why::Gentle)
            }
            Speed::Full => {
                self.since = None;
                (cores, cores.saturating_mul(2).clamp(FLOOR_FAST, MAX_FAST), Why::Full)
            }
            Speed::Auto => {
                let a = self.auto(i, cores, now);
                cut = matches!(a.2, Why::Present | Why::Busy);
                a
            }
        };
        // Whatever the mode: an OCR worker that does not fit in memory makes
        // the whole PC swap, which is worse for the user than slow reading.
        let room = u32::try_from((i.free_ram_mb.saturating_sub(KEEP_FREE_MB) / i.ocr_worker_mb.max(1)).max(1)).unwrap_or(u32::MAX);
        let ocr = wanted.min(room);
        if !i.fast_backlog && !i.ocr_backlog {
            return stopped("idle", "Nothing is waiting to be read.");
        }
        let note = if !i.ocr_backlog {
            match why {
                Why::Gentle => "Reading at the minimum until you change it.".to_string(),
                Why::Full => "Full speed until you change it.".to_string(),
                Why::Busy => busy_note(i),
                Why::Present => PRESENT_NOTE.to_string(),
                _ => format!("Reading text on {fast} workers."),
            }
        } else {
            match why {
                Why::Gentle => "Reading at the minimum until you change it.".to_string(),
                Why::Full if ocr < wanted => format!("Full speed, but free memory allows only {}.", ocr_workers(ocr)),
                Why::Full => "Full speed until you change it.".to_string(),
                Why::Busy => busy_note(i),
                _ if ocr < wanted => format!("Reading on {}: free memory allows no more.", ocr_workers(ocr)),
                Why::Server => format!("Server: reading on {ocr} of {cores} cores."),
                Why::Away => format!("Nobody is at the PC: reading on {ocr} of {cores} cores."),
                Why::Ramping => format!("Nobody is at the PC: reading on {}, doubling every 30 s.", ocr_workers(ocr)),
                Why::Present => PRESENT_NOTE.to_string(),
            }
        };
        Targets { fast: if i.fast_backlog { fast } else { 0 }, ocr: if i.ocr_backlog { ocr } else { 0 }, state: "reading".to_string(), note, cut }
    }

    /// Automatic mode: the minimum while someone is at the PC, otherwise
    /// one doubling per 30 s up to all cores but one.
    fn auto(&mut self, i: &Inputs, cores: u32, now: Instant) -> (u32, u32, Why) {
        if !i.server && i.idle_secs <= AWAY_SECS {
            // The climb starts from the bottom, 30 s after they have left.
            self.level = FLOOR_OCR;
            self.since = Some(now);
            let busy = i.others_cpu > i.busy_cpu_percent as f32 && i.idle_secs < BUSY_WITHIN_SECS;
            return if busy { (FLOOR_OCR, 1, Why::Busy) } else { (FLOOR_OCR, FLOOR_FAST, Why::Present) };
        }
        let ceiling = if i.server { cores } else { (cores - 1).max(1) };
        match self.since {
            None => self.since = Some(now),
            Some(t) if now.saturating_duration_since(t) >= RAMP => {
                self.level = self.level.saturating_mul(2);
                self.since = Some(now);
            }
            Some(_) => {}
        }
        self.level = self.level.clamp(FLOOR_OCR, ceiling);
        let why = if i.server {
            Why::Server
        } else if self.level < ceiling {
            Why::Ramping
        } else {
            Why::Away
        };
        (self.level, self.level.saturating_mul(2).clamp(FLOOR_FAST, if i.server { MAX_FAST } else { AUTO_MAX_FAST }), why)
    }
}

const PRESENT_NOTE: &str = "Someone is at the PC: reading at the minimum. It speeds up by itself after 5 minutes without keyboard or mouse.";

fn stopped(state: &str, note: &str) -> Targets {
    // Paused means now, not after the documents in hand; with nothing
    // waiting there is nothing to stop.
    Targets { fast: 0, ocr: 0, state: state.to_string(), note: note.to_string(), cut: state != "idle" }
}

fn busy_note(i: &Inputs) -> String {
    format!("Reading at the minimum: other programs use {:.0}% of the CPU.", i.others_cpu.clamp(0.0, 100.0))
}

fn ocr_workers(n: u32) -> String {
    if n == 1 { "1 OCR worker".to_string() } else { format!("{n} OCR workers") }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A free 8-core PC with someone at it and work in both lanes.
    fn free_pc() -> Inputs {
        Inputs {
            cores: 8,
            others_cpu: 5.0,
            idle_secs: 10,
            free_ram_mb: 16_000,
            on_battery: false,
            server: false,
            speed: Speed::Auto,
            busy_cpu_percent: 60,
            paused: false,
            ocr_worker_mb: 600,
            fast_backlog: true,
            ocr_backlog: true,
        }
    }

    /// The same PC with nobody at it.
    fn empty_pc() -> Inputs {
        Inputs { idle_secs: 900, ..free_pc() }
    }

    /// Step at each of `secs` (seconds after `t0`) and return (ocr, fast) each time.
    fn run(g: &mut Governor, i: &Inputs, t0: Instant, secs: &[u64]) -> Vec<(u32, u32)> {
        secs.iter()
            .map(|s| {
                let t = g.step(i, t0 + Duration::from_secs(*s));
                (t.ocr, t.fast)
            })
            .collect()
    }

    #[test]
    fn paused_stops_everything_now() {
        let t0 = Instant::now();
        for speed in [Speed::Auto, Speed::Gentle, Speed::Full] {
            for (server, on_battery) in [(false, false), (true, false), (false, true), (true, true)] {
                let i = Inputs { paused: true, speed, server, on_battery, ..free_pc() };
                let t = Governor::new().step(&i, t0);
                assert_eq!((t.fast, t.ocr), (0, 0));
                assert_eq!(t.state, "paused");
                assert!(t.cut, "paused readers stop in the middle of a document");
                assert_eq!(t.note, "Reading is paused on this PC until you resume it.");
            }
        }
    }

    #[test]
    fn battery_pauses_a_pc_but_not_a_server() {
        let t0 = Instant::now();
        for speed in [Speed::Auto, Speed::Gentle, Speed::Full] {
            let i = Inputs { on_battery: true, speed, ..free_pc() };
            let t = Governor::new().step(&i, t0);
            assert_eq!((t.fast, t.ocr, t.state.as_str(), t.cut), (0, 0, "paused: on battery", true), "{speed:?}");
            assert!(t.note.contains("battery"));
            let t = Governor::new().step(&Inputs { server: true, ..i }, t0);
            assert_eq!(t.state, "reading", "{speed:?}");
            assert!(t.ocr >= 1 && t.fast >= 2);
        }
    }

    #[test]
    fn someone_at_the_pc_means_the_minimum_and_extras_stop_now() {
        let t0 = Instant::now();
        // However long the PC stays free, and however big it is.
        for (cores, free_ram_mb) in [(1, 2_000), (4, 8_000), (8, 16_000), (64, 128_000)] {
            let i = Inputs { cores, free_ram_mb, ..free_pc() };
            let mut g = Governor::new();
            for got in run(&mut g, &i, t0, &[0, 30, 60, 90, 600, 3_600]) {
                assert_eq!(got, (1, 2), "{cores} cores");
            }
            let t = g.step(&i, t0 + Duration::from_secs(3_601));
            assert!(t.cut);
            assert_eq!(t.note, PRESENT_NOTE);
        }
        // 300 s without input is still "at the PC"; 301 is not.
        let mut g = Governor::new();
        assert_eq!(run(&mut g, &Inputs { idle_secs: 300, ..free_pc() }, t0, &[0, 30, 60, 90]), [(1, 2), (1, 2), (1, 2), (1, 2)]);
        // Text only: the same sentence.
        assert_eq!(Governor::new().step(&Inputs { ocr_backlog: false, ..free_pc() }, t0).note, PRESENT_NOTE);
    }

    #[test]
    fn a_busy_pc_with_someone_at_it_gets_one_text_worker() {
        let t0 = Instant::now();
        // (others_cpu, idle_secs, busy_cpu_percent, busy?)
        let cases = [(82.0, 10, 60, true), (60.0, 10, 60, false), (60.5, 10, 60, true), (59.0, 10, 60, false), (99.0, 119, 60, true), (99.0, 120, 60, false), (30.0, 10, 20, true), (30.0, 10, 90, false)];
        for (others_cpu, idle_secs, busy_cpu_percent, busy) in cases {
            let t = Governor::new().step(&Inputs { others_cpu, idle_secs, busy_cpu_percent, ..free_pc() }, t0);
            assert_eq!((t.ocr, t.fast), (1, if busy { 1 } else { 2 }), "{others_cpu}% with {idle_secs} s idle against {busy_cpu_percent}%");
            assert_eq!(t.note.starts_with("Reading at the minimum: other programs use"), busy);
            assert!(t.cut);
        }
        // A loaded CPU with nobody there (a render left running) is not a person waiting.
        let mut g = Governor::new();
        let last = run(&mut g, &Inputs { others_cpu: 99.0, ..empty_pc() }, t0, &[0, 30, 60, 90]).pop();
        assert_eq!(last, Some((7, 8)));
    }

    #[test]
    fn nobody_there_doubles_every_30_s_up_to_all_cores_but_one() {
        let t0 = Instant::now();
        let mut g = Governor::new();
        let got = run(&mut g, &empty_pc(), t0, &[0, 10, 29, 30, 45, 59, 60, 89, 90, 120, 600]);
        assert_eq!(got, [(1, 2), (1, 2), (1, 2), (2, 4), (2, 4), (2, 4), (4, 8), (4, 8), (7, 8), (7, 8), (7, 8)]);
        let t = g.step(&empty_pc(), t0 + Duration::from_secs(601));
        assert_eq!(t.note, "Nobody is at the PC: reading on 7 of 8 cores.");
        assert!(!t.cut);
        // A big PC shows the whole series; text workers stop at 8.
        let mut g = Governor::new();
        let got = run(&mut g, &Inputs { cores: 64, free_ram_mb: 128_000, ..empty_pc() }, t0, &[0, 30, 60, 90, 120, 150, 180, 210]);
        assert_eq!(got, [(1, 2), (2, 4), (4, 8), (8, 8), (16, 8), (32, 8), (63, 8), (63, 8)]);
        // One doubling per step, however long since the last one: a gap in
        // the calls is not proof that the PC was free all that time.
        let mut g = Governor::new();
        assert_eq!(run(&mut g, &empty_pc(), t0, &[0, 300]), [(1, 2), (2, 4)]);
        assert_eq!(Governor::new().step(&empty_pc(), t0).note, "Nobody is at the PC: reading on 1 OCR worker, doubling every 30 s.");
        // Small PCs: never below one. (cores, ceiling)
        for (cores, ceiling) in [(1, 1), (2, 1), (3, 2), (4, 3), (16, 15)] {
            let mut g = Governor::new();
            let last = run(&mut g, &Inputs { cores, free_ram_mb: 64_000, ..empty_pc() }, t0, &[0, 30, 60, 90, 120, 150]).pop().map(|x| x.0);
            assert_eq!(last, Some(ceiling), "{cores} cores");
        }
    }

    #[test]
    fn coming_back_drops_to_the_minimum_at_once_and_leaving_climbs_from_the_bottom() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut g = Governor::new();
        assert_eq!(run(&mut g, &empty_pc(), t0, &[0, 30, 60, 90]), [(1, 2), (2, 4), (4, 8), (7, 8)]);
        // The user touches the mouse.
        let t = g.step(&free_pc(), at(91));
        assert_eq!((t.ocr, t.fast, t.cut), (1, 2, true));
        assert_eq!(g.step(&free_pc(), at(200)).ocr, 1);
        assert_eq!(g.step(&free_pc(), at(500)).ocr, 1);
        // Gone again: 30 s after the last moment they were there, then doubling.
        assert_eq!(run(&mut g, &empty_pc(), t0, &[501, 529, 530, 560, 590]), [(1, 2), (1, 2), (2, 4), (4, 8), (7, 8)]);
    }

    #[test]
    fn a_lane_without_backlog_gets_nothing_and_keeps_its_level() {
        let t0 = Instant::now();
        let mut g = Governor::new();
        assert_eq!(run(&mut g, &empty_pc(), t0, &[0, 30, 60]), [(1, 2), (2, 4), (4, 8)]);
        let t = g.step(&Inputs { ocr_backlog: false, ..empty_pc() }, t0 + Duration::from_secs(61));
        assert_eq!((t.ocr, t.fast, t.state.as_str()), (0, 8, "reading"));
        assert_eq!(t.note, "Reading text on 8 workers.");
        let t = g.step(&Inputs { fast_backlog: false, ..empty_pc() }, t0 + Duration::from_secs(62));
        assert_eq!((t.ocr, t.fast, t.state.as_str()), (4, 0, "reading"));
        let t = g.step(&Inputs { fast_backlog: false, ocr_backlog: false, ..empty_pc() }, t0 + Duration::from_secs(63));
        assert_eq!((t.ocr, t.fast, t.state.as_str(), t.cut), (0, 0, "idle", false));
        assert_eq!(t.note, "Nothing is waiting to be read.");
        // Work comes back: the level reached earlier is still there.
        let t = g.step(&empty_pc(), t0 + Duration::from_secs(64));
        assert_eq!((t.ocr, t.fast), (4, 8));
        // Idle in the other modes too.
        for speed in [Speed::Gentle, Speed::Full] {
            let t = Governor::new().step(&Inputs { speed, fast_backlog: false, ocr_backlog: false, ..free_pc() }, t0);
            assert_eq!((t.ocr, t.fast, t.state.as_str()), (0, 0, "idle"));
        }
    }

    #[test]
    fn gentle_is_exactly_the_floor() {
        let t0 = Instant::now();
        // (cores, others_cpu, idle_secs, free_ram_mb, server)
        let cases = [(8, 5.0, 10, 16_000, false), (8, 95.0, 0, 16_000, false), (64, 0.0, 9_000, 128_000, false), (1, 0.0, 0, 300, false), (16, 0.0, 9_000, 64_000, true)];
        for (cores, others_cpu, idle_secs, free_ram_mb, server) in cases {
            let i = Inputs { speed: Speed::Gentle, cores, others_cpu, idle_secs, free_ram_mb, server, ..free_pc() };
            let mut g = Governor::new();
            for (ocr, fast) in run(&mut g, &i, t0, &[0, 30, 60, 600]) {
                assert_eq!((ocr, fast), (1, 2), "{i:?}");
            }
            let t = g.step(&i, t0);
            assert_eq!(t.note, "Reading at the minimum until you change it.");
            assert!(t.cut);
        }
    }

    #[test]
    fn full_uses_every_core_whatever_the_pc_does() {
        let t0 = Instant::now();
        // (cores, ocr, fast)
        for (cores, ocr, fast) in [(1, 1, 2), (2, 2, 4), (4, 4, 8), (8, 8, 16), (32, 32, 16)] {
            for (others_cpu, idle_secs) in [(0.0, 9_000), (99.0, 0)] {
                let i = Inputs { speed: Speed::Full, cores, others_cpu, idle_secs, free_ram_mb: 64_000, ..free_pc() };
                let t = Governor::new().step(&i, t0);
                assert_eq!((t.ocr, t.fast, t.cut), (ocr, fast, false), "{cores} cores, {others_cpu}% busy");
                assert_eq!(t.note, "Full speed until you change it.");
            }
        }
        // Still limited by memory: 1 GB stays free, 600 MB per worker.
        let i = Inputs { speed: Speed::Full, free_ram_mb: 1024 + 1_300, ..free_pc() };
        let t = Governor::new().step(&i, t0);
        assert_eq!((t.ocr, t.fast), (2, 16));
        assert_eq!(t.note, "Full speed, but free memory allows only 2 OCR workers.");
    }

    #[test]
    fn memory_caps_ocr_in_every_mode() {
        let t0 = Instant::now();
        // (free_ram_mb, ocr_worker_mb, most OCR workers)
        let cases = [(16_000, 600, 24), (1024 + 1_800, 600, 3), (1024 + 1_799, 600, 2), (1024 + 600, 600, 1), (1_500, 600, 1), (500, 600, 1), (0, 600, 1), (8_000, 0, 6_976), (9_216, 2_048, 4)];
        for (free_ram_mb, ocr_worker_mb, room) in cases {
            let i = Inputs { cores: 64, free_ram_mb, ocr_worker_mb, ..empty_pc() };
            let t = Governor::new().step(&Inputs { speed: Speed::Full, ..i.clone() }, t0);
            assert_eq!(t.ocr, room.min(64), "full, {free_ram_mb} MB free");
            assert_eq!(t.fast, 16, "memory limits OCR only");
            let mut g = Governor::new();
            let last = run(&mut g, &i, t0, &[0, 30, 60, 90, 120, 150, 180, 210]).pop().map(|x| x.0);
            assert_eq!(last, Some(room.min(63)), "auto, {free_ram_mb} MB free");
            assert_eq!(Governor::new().step(&Inputs { speed: Speed::Gentle, ..i }, t0).ocr, 1);
        }
        let mut g = Governor::new();
        let tight = Inputs { free_ram_mb: 1024 + 1_300, ..empty_pc() };
        assert_eq!(run(&mut g, &tight, t0, &[0, 30, 60]), [(1, 2), (2, 4), (2, 8)]);
        assert_eq!(g.step(&tight, t0 + Duration::from_secs(61)).note, "Reading on 2 OCR workers: free memory allows no more.");
    }

    #[test]
    fn a_server_is_never_throttled() {
        let t0 = Instant::now();
        // Loaded CPU, a user "present", on battery: none of it matters.
        let i = Inputs { server: true, others_cpu: 99.0, idle_secs: 0, on_battery: true, ..free_pc() };
        let mut g = Governor::new();
        assert_eq!(run(&mut g, &i, t0, &[0, 30, 60, 90, 120]), [(1, 2), (2, 4), (4, 8), (8, 16), (8, 16)]);
        let t = g.step(&i, t0 + Duration::from_secs(121));
        assert_eq!((t.state.as_str(), t.note.as_str(), t.cut), ("reading", "Server: reading on 8 of 8 cores.", false));
        // The user's pause still holds on a server.
        assert_eq!(g.step(&Inputs { paused: true, ..i }, t0 + Duration::from_secs(122)).state, "paused");
    }

    #[test]
    fn time_spent_paused_is_not_time_proven_free() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut g = Governor::new();
        assert_eq!(run(&mut g, &empty_pc(), t0, &[0, 30]), [(1, 2), (2, 4)]);
        assert_eq!(g.step(&Inputs { paused: true, ..empty_pc() }, at(40)).ocr, 0);
        // Resumed an hour later: the level is kept, the next doubling is 30 s on.
        assert_eq!(run(&mut g, &empty_pc(), t0, &[3_600, 3_629, 3_630]), [(2, 4), (2, 4), (4, 8)]);
    }
}
