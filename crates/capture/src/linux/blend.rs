//! Live frame blending on Linux, on the graphics card (`super::gpu_frame`):
//! each frame is the average light of a fixed number of looks at the screen,
//! evenly spaced over the frame's time, or blended softly into the frames
//! either side (`crate::shutter`). As on macOS (`crate::mac::blend`), whose
//! timing this follows.
//!
//! The compositor only hands over what the display shows, so the blend rate
//! is at most the display's refresh rate. Each picture is queued with when
//! it arrived (`Latest::queue_arrivals`); a look takes the picture that was
//! up at its time. Looks right next to the pictures' times would get one
//! picture twice and the next not at all by chance, so they're kept halfway
//! between the pictures, locked to their pace as on Windows and macOS
//! (`crate::shutter::follow_picture`).
//!
//! Each Display and Window source that shows is blended too, on its own, as
//! on Windows and macOS: its pictures queued with their times, its looks kept
//! halfway between its own pictures (each source's a fraction of a look from
//! another's), its looks' light added up in its own pixels and resolved into
//! a picture the layers' canvas draws in its stead (`GpuPicture::layer_blend`).
//! A frame is made once every source has taken its looks for it.
//!
//! It's one way the pacer (`super::spawn_pacer`) times its frames: blending
//! starts, changes and stops at a frame while the capture runs
//! (`Recorder::set_blend`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::gpu_frame::LayerBlend;
use super::layers::Pixels;
use super::screen::{Frame, Latest};
use super::{Pace, host_now, submit_preview};
use crate::shutter::Shutter;

/// How long after a frame's time it's made: a picture reaches us a moment
/// after the screen showed it, and must still get into its frame. A guess,
/// with room for a busy system; as on macOS.
const BLEND_LATE: f64 = 0.006;

/// Frame blending's timing: `looks` looks at the screen per frame, their
/// light averaged on the graphics card (which the pace's picture must be
/// made on).
pub(super) struct Blending {
    t0: f64,
    /// Seconds between looks, and looks per frame.
    step: f64,
    looks: i64,
    /// Look `m` is at `t0 + phase + (m + 0.5) * step`.
    phase: f64,
    /// A picture the capture timed has come up (it set `phase`).
    timed: bool,
    shutter: Shutter,
    /// The next look to take, and the last frame done.
    next: i64,
    closed: i64,
    /// The picture up (its number), and the one the last look took.
    held: Option<u64>,
    last_taken: Option<u64>,
    /// Different pictures in the frame's own time so far.
    different: u32,
    /// The newest picture number taken from the feed.
    seen: u64,
    stats: BlendStats,
    /// Each Display and Window source that shows, blended on its own.
    layers: Vec<LayerSource>,
    /// A frame was made since the layers were last followed.
    follow_due: bool,
    /// The frames show the recording's own picture (see `follow_layers`).
    /// Its looks are taken either way: the frames are made from its sums.
    main_shown: bool,
}

/// A Display or Window source blended on its own (see the module's doc):
/// its capture's pictures, its blend, and its looks' own timing.
struct LayerSource {
    /// Its layer's id, and what it is, for the log.
    id: u64,
    name: String,
    /// Its capture's pictures, and the number their numbers start from (as
    /// the layer has them: `layers::CaptureFeed::picture`).
    latest: Arc<Latest>,
    base: u64,
    /// Its blend, made for its pictures' size once it has one (again when
    /// that changes: a window resized).
    blend: Option<LayerBlend>,
    /// Its look `m` is at `t0 + phase + (m + 0.5) * step`, its next look,
    /// and whether a picture has set `phase`.
    phase: f64,
    next: i64,
    timed: bool,
    /// The picture that's up (as the layer has it, and its number), and the
    /// one the last look took.
    up: Option<(Pixels, u64)>,
    last_taken: Option<u64>,
    /// The newest picture number (its capture's) taken in.
    seen: u64,
    different: u32,
    stats: LayerStats,
    /// Its pictures a second, for whose are told while the recording's own
    /// picture doesn't show (`Blending::say_sources`).
    rate: Rate,
}

/// A source's pictures a second, as of the last whole second (as Windows'
/// and macOS' pacers count them).
#[derive(Default)]
struct Rate {
    since: f64,
    count: u32,
    last: u32,
}

impl Rate {
    /// A picture came at `now`.
    fn note(&mut self, now: f64) {
        if now - self.since >= 1.0 {
            self.last = if now - self.since < 2.0 { self.count } else { 0 };
            (self.since, self.count) = (now, 0);
        }
        self.count += 1;
    }

    /// Pictures in the last whole second, as of `now` (or so far, before
    /// one's gone by).
    fn per_second(&self, now: f64) -> u32 {
        match now - self.since {
            gone if gone >= 2.0 => 0,
            gone if gone >= 1.0 => self.count,
            _ => self.last.max(self.count),
        }
    }
}

impl Blending {
    /// Blend from frame `k` on: `looks` a frame, where `shutter` says. The
    /// looks from now on see the picture that's up.
    pub(super) fn start(pace: &mut Pace, looks: u32, shutter: Shutter, k: i64) -> Self {
        let looks = looks.max(1);
        if let Some(g) = &mut pace.picture.gpu {
            g.start_blending(shutter.frames_open());
        }
        pace.picture.feed.queue_arrivals();
        crate::blending(pace.fps, looks);
        let mut b = Self {
            t0: pace.t0,
            step: 1.0 / (pace.fps.max(1) * looks) as f64,
            looks: looks as i64,
            phase: 0.0,
            timed: false,
            shutter,
            next: shutter.first_look(k),
            closed: k - 1,
            held: None,
            last_taken: None,
            different: 0,
            seen: 0,
            stats: BlendStats::new(),
            layers: Vec::new(),
            follow_due: true,
            main_shown: true,
        };
        b.hold_newest(pace);
        b
    }

    /// Blend `looks` a frame where `shutter` says from the next frame on:
    /// the frame under way is made from the looks it gets from now (at the
    /// new rate), and the looks lock onto the pictures again. As Windows'
    /// pacer takes a change (`crate::win`).
    pub(super) fn change(&mut self, pace: &mut Pace, looks: u32, shutter: Shutter) {
        let looks = looks.max(1);
        if looks as i64 == self.looks && shutter == self.shutter {
            return;
        }
        // Its sums made anew, as many as are open at once.
        if let Some(g) = &mut pace.picture.gpu {
            g.start_blending(shutter.frames_open());
        }
        let k = self.closed + 1;
        self.looks = looks as i64;
        self.step = 1.0 / (pace.fps.max(1) * looks) as f64;
        self.shutter = shutter;
        // Frame `k`'s own time is where it was: looks `k * looks` on, at the
        // new step. The first picture the capture times puts them halfway
        // between pictures again.
        self.phase = 0.0;
        self.timed = false;
        self.next = shutter.first_look(k);
        self.different = 0;
        self.stats = BlendStats::new();
        self.hold_newest(pace);
        // Each Display and Window source's too, its blend made anew.
        for l in &mut self.layers {
            (l.phase, l.timed, l.next, l.different) = (0.0, false, self.next, 0);
            l.blend = None;
            if let Some((pixels, seq)) = l.up.clone() {
                l.hold(pace, &pixels, seq);
            }
        }
        crate::blending(pace.fps, looks);
        self.say_sources();
        crate::eprintln_later!("frame blending: {looks} looks a frame from now on");
    }

    /// The last frame done: the next is the first another timing makes.
    pub(super) fn closed(&self) -> i64 {
        self.closed
    }

    /// Blending stops: the Display and Window sources' pictures are queued
    /// no more, and frames draw each from its newest picture.
    pub(super) fn let_go_of_layers(&mut self, pace: &mut Pace) {
        for l in self.layers.drain(..) {
            l.latest.stop_queueing();
        }
        pace.picture.blends.clear();
    }

    /// The looks from now on see the newest picture (blending started, or
    /// its sums made anew).
    fn hold_newest(&mut self, pace: &mut Pace) {
        if let Some((f, s)) = pace.picture.feed.get() {
            if let Some(g) = &mut pace.picture.gpu {
                g.hold(&f, s);
            }
            self.held = Some(s);
            self.seen = self.seen.max(s);
        }
    }

    fn look_time(&self, m: i64) -> f64 {
        self.t0 + self.phase + (m as f64 + 0.5) * self.step
    }

    /// When frame `k` is done: every source's last look for it.
    fn frame_done_at(&self, k: i64) -> f64 {
        let last = self.shutter.last_look(k);
        self.layers.iter().map(|l| l.look_time(self.t0, self.step, last)).fold(self.look_time(last), f64::max)
    }

    /// When the next frame to make is done.
    fn frame_end(&self) -> f64 {
        self.frame_done_at(self.closed + 1)
    }

    /// One turn of the pacer: wait for a picture or the next frame's end,
    /// then take the looks whose time has come, each seeing the picture that
    /// was up at its time; frames are made as their looks are all in.
    pub(super) fn step(&mut self, pace: &mut Pace) {
        if std::mem::take(&mut self.follow_due) {
            self.follow_layers(pace);
        }
        pace.picture.feed.want();
        let wait = self.frame_end() + BLEND_LATE - host_now();
        if wait > 0.0 {
            pace.picture.feed.wait(self.seen, Duration::from_secs_f64(wait.min(0.05)));
        }
        for a in pace.picture.feed.drain() {
            if a.seq > self.seen {
                self.seen = a.seq;
                self.arrive(pace, a.at, &a.frame, a.seq, true);
            }
        }
        // Put up some other way (the away screen, a new window): taken from the feed.
        if let Some((f, now)) = pace.picture.feed.get()
            && now > self.seen
        {
            self.seen = now;
            self.arrive(pace, host_now(), &f, now, false);
        }
        // Each Display and Window source's pictures (only its own looks
        // are taken for each: a frame is made once every source's are in,
        // so the order sources are taken in doesn't matter).
        for i in 0..self.layers.len() {
            for a in self.layers[i].latest.drain() {
                if a.seq > self.layers[i].seen {
                    self.layers[i].seen = a.seq;
                    self.arrive_layer(pace, i, a.at, &a.frame, a.seq);
                }
            }
        }
        // Looks whose time is past (and late pictures' too) see what's up.
        self.look_until(pace, host_now() - BLEND_LATE);
        pace.check_ended();
        submit_preview(&mut pace.picture, &mut pace.sent);
    }

    /// Take every look before `t`, each source's: each sees the picture
    /// it has up. Then the frames every source has taken its looks for are
    /// made.
    fn look_until(&mut self, pace: &mut Pace, t: f64) {
        let mut burst = 0;
        while self.look_time(self.next) < t && burst < 10_000 {
            self.look(pace);
            burst += 1;
        }
        for i in 0..self.layers.len() {
            self.layer_looks(pace, i, t);
        }
        self.close_done(pace);
    }

    fn look(&mut self, pace: &mut Pace) {
        let (first, weights) = self.shutter.split(self.next);
        if let Some(id) = self.held {
            for (k, weight) in [(first, weights[0]), (first + 1, weights[1])] {
                if k > self.closed && weight > 0.0 {
                    if let Some(g) = &mut pace.picture.gpu {
                        g.add(k, weight);
                    }
                }
            }
            if self.last_taken != Some(id) {
                self.different += 1;
                self.last_taken = Some(id);
            }
        }
        self.next += 1;
        if self.next % self.looks == 0 {
            let different = std::mem::take(&mut self.different);
            if self.held.is_some() {
                self.stats.frame(different, self.looks as u32);
            }
        }
    }

    /// Take layer `i`'s looks before `t`.
    fn layer_looks(&mut self, pace: &mut Pace, i: usize, t: f64) {
        let (closed, looks) = (self.closed, self.looks);
        let (t0, step, shutter) = (self.t0, self.step, self.shutter);
        let l = &mut self.layers[i];
        let mut burst = 0;
        while l.look_time(t0, step, l.next) < t && burst < 10_000 {
            let (first, weights) = shutter.split(l.next);
            if let (Some((_, id)), Some(blend), Some(g)) = (&l.up, l.blend.as_mut(), &pace.picture.gpu) {
                for (k, weight) in [(first, weights[0]), (first + 1, weights[1])] {
                    if k > closed && weight > 0.0 {
                        g.layer_add(blend, k, weight);
                    }
                }
                if l.last_taken != Some(*id) {
                    l.different += 1;
                    l.last_taken = Some(*id);
                }
            }
            l.next += 1;
            if l.next % looks == 0 {
                let different = std::mem::take(&mut l.different);
                if l.up.is_some() {
                    l.stats.frame(different, looks as u32, &l.name);
                }
            }
            burst += 1;
        }
    }

    /// The frames no source's later looks count toward are done: made.
    fn close_done(&mut self, pace: &mut Pace) {
        let next = self.layers.iter().map(|l| l.next).fold(self.next, i64::min);
        let (first, _) = self.shutter.split(next);
        while self.closed < first - 1 {
            self.close(pace, self.closed + 1);
        }
    }

    /// Frame `k` is done: made (with the webcam over it) and encoded, and
    /// the stream's pictures', where they want it (blended: theirs are made
    /// of all the frames in their time). Each Display and Window source's
    /// blend goes into the layers first.
    fn close(&mut self, pace: &mut Pace, k: i64) {
        self.closed = k;
        self.follow_due = true;
        pace.picture.blends.clear();
        if let Some(g) = &pace.picture.gpu {
            for l in &mut self.layers {
                if let Some(blend) = l.blend.as_mut()
                    && let Some((pixels, seq)) = g.layer_finish(blend, k)
                {
                    pace.picture.blends.push((l.id, pixels, seq));
                }
            }
        }
        pace.picture.layers();
        let Some(g) = &mut pace.picture.gpu else { return };
        if !g.finish(k) {
            return;
        }
        pace.picture.version += 1;
        let Some(g) = &pace.picture.gpu else { return };
        if pace.encoder.encode_made(g, pace.picture.version, k) {
            super::encoded();
        } else {
            super::skipped(&mut pace.skipped, 1);
        }
        pace.stream_frame(k, true);
    }

    /// A new picture came up at `at`: the looks before it saw the one
    /// before. Then the looks move toward halfway between pictures.
    /// `timed`: it came from the feed's queue with its time (else it was put
    /// up some other way, and `at` is when it was noticed).
    fn arrive(&mut self, pace: &mut Pace, at: f64, frame: &Frame, id: u64, timed: bool) {
        // How the capture keeps up counts the screen's pictures while the
        // frames show it (as on Windows and macOS, whose pacers then don't
        // take them at all).
        if self.main_shown {
            crate::count_picture();
        }
        if at < self.look_time(self.next - 1) {
            self.stats.late += 1;
            if self.main_shown {
                crate::count_missed(1);
            }
        }
        let mut burst = 0;
        while self.look_time(self.next) < at && burst < 10_000 {
            self.look(pace);
            burst += 1;
        }
        self.close_done(pace);
        // Pictures belong on whole steps of the looks (halfway between two);
        // the first the capture timed puts them there.
        let first = timed && !self.timed;
        self.timed |= timed;
        self.phase = crate::shutter::follow_picture(self.phase, self.step, at - self.t0, first);
        if let Some(g) = &mut pace.picture.gpu {
            g.hold(frame, id);
        }
        self.held = Some(id);
        self.stats.pictures += 1;
    }

    /// A new picture of layer `i` came up at `at` (number `seq`, its
    /// capture's): its looks before it saw the one before. Then its looks
    /// move toward halfway between its pictures.
    fn arrive_layer(&mut self, pace: &mut Pace, i: usize, at: f64, frame: &Arc<Frame>, seq: u64) {
        let (t0, step) = (self.t0, self.step);
        let l = &self.layers[i];
        crate::count_picture_of(l.id);
        if at < l.look_time(t0, step, l.next - 1) {
            crate::count_missed_of(l.id, 1);
            self.layers[i].stats.late += 1;
        }
        self.layer_looks(pace, i, at);
        self.close_done(pace);
        let l = &mut self.layers[i];
        let first = !l.timed;
        l.timed = true;
        l.phase = crate::shutter::follow_picture(l.phase, step, at - t0, first);
        l.stats.pictures += 1;
        l.rate.note(at);
        let number = l.base + seq;
        l.hold(pace, &Pixels::Frame(frame.clone()), number);
    }

    /// The Display and Window sources that show now, each blended on its
    /// own: one that's come into the frames (shown, put in the scene, or
    /// another display or window now) gets its pictures queued, starting
    /// from its newest; one that's gone is let go of. Not one whose capture
    /// is the recording's own (a Display source of the display it records):
    /// its pictures are taken by the recording's blend.
    fn follow_layers(&mut self, pace: &mut Pace) {
        self.main_shown = pace.picture.main_shown();
        let want: Vec<_> = pace.picture.feeds.frame_blended().into_iter().filter(|(_, latest, ..)| !pace.picture.feed.is(latest)).collect();
        self.layers.retain(|l| {
            let keep = want.iter().any(|(id, latest, base, _)| *id == l.id && *base == l.base && Arc::ptr_eq(latest, &l.latest));
            if !keep {
                l.latest.stop_queueing();
                crate::eprintln_later!("frame blending: {} isn't shown, no longer blended", l.name);
            }
            keep
        });
        for (id, latest, base, name) in want {
            // One blend a capture: a display shown twice shares one.
            if self.layers.iter().any(|l| l.id == id || Arc::ptr_eq(&l.latest, &latest)) {
                continue;
            }
            latest.queue_arrivals_for(id);
            let mut l = LayerSource {
                id,
                name,
                latest,
                base,
                blend: None,
                phase: self.phase,
                next: self.next,
                timed: false,
                up: None,
                last_taken: None,
                seen: 0,
                different: 0,
                stats: LayerStats::new(),
                rate: Rate::default(),
            };
            if let Some((frame, seq)) = l.latest.get() {
                l.seen = seq;
                l.hold(pace, &Pixels::Frame(frame), base + seq);
            }
            crate::eprintln_later!("frame blending: {} shown, blended on its own", l.name);
            self.layers.push(l);
        }
        self.say_sources();
    }

    /// Which sources the looks are at, for how the capture keeps up
    /// (`crate::health`), as on Windows and macOS: the recording's own
    /// picture while the frames show it, and each Display and Window source
    /// blended. Its pictures are told while it shows, else those of the
    /// source showing the most (the first of those showing as many).
    fn say_sources(&self) {
        let now = host_now();
        let busiest = self.layers.iter().fold(None::<&LayerSource>, |b, l| if b.is_some_and(|b| b.rate.per_second(now) >= l.rate.per_second(now)) { b } else { Some(l) });
        let told = if self.main_shown { 0 } else { busiest.map_or(0, |l| l.id) };
        crate::blend_sources(usize::from(self.main_shown) + self.layers.len(), told);
    }
}

impl LayerSource {
    /// When its look `m` is.
    fn look_time(&self, t0: f64, step: f64, m: i64) -> f64 {
        t0 + self.phase + (m as f64 + 0.5) * step
    }

    /// Its picture that's up is `pixels` (number `seq`, as the layer has
    /// it): its blend made for its size, if it isn't.
    fn hold(&mut self, pace: &mut Pace, pixels: &Pixels, seq: u64) {
        self.up = Some((pixels.clone(), seq));
        let Some(g) = &pace.picture.gpu else { return };
        let size = pixels.size();
        if self.blend.as_ref().is_none_or(|b| b.size() != size) {
            self.blend = g.layer_blend(size);
        }
        if let Some(b) = self.blend.as_mut() {
            g.layer_hold(b, pixels, seq);
        }
    }
}

/// What a Display or Window source's blending saw, printed every few
/// seconds (to the log), as [`BlendStats`] says the screen's.
struct LayerStats {
    since: Instant,
    pictures: u32,
    frames: u32,
    full: u32,
    late: u32,
}

impl LayerStats {
    fn new() -> Self {
        Self { since: Instant::now(), pictures: 0, frames: 0, full: 0, late: 0 }
    }

    fn frame(&mut self, different: u32, looks: u32, name: &str) {
        self.frames += 1;
        self.full += (different == looks) as u32;
        if self.since.elapsed() >= Duration::from_secs(5) {
            crate::eprintln_later!(
                "frame blending of {name}: it showed {:.0} pictures/s; {looks} looks per frame, {:.0}% of frames with {looks} different pictures; {} pictures too late, over {} frames",
                self.pictures as f64 / self.since.elapsed().as_secs_f64(),
                self.full as f64 * 100.0 / self.frames as f64,
                self.late,
                self.frames
            );
            *self = Self::new();
        }
    }
}

/// What the blending saw, printed every few seconds (to the log): how many
/// pictures the screen showed, how many different ones each frame got (all
/// its looks, when the screen changes at least at the blend rate), and how
/// many came too late for their frame. As on macOS.
struct BlendStats {
    since: Instant,
    pictures: u32,
    frames: u32,
    full: u32,
    fewest: u32,
    most: u32,
    late: u32,
}

impl BlendStats {
    fn new() -> Self {
        Self { since: Instant::now(), pictures: 0, frames: 0, full: 0, fewest: u32::MAX, most: 0, late: 0 }
    }

    fn frame(&mut self, different: u32, looks: u32) {
        self.frames += 1;
        self.full += (different == looks) as u32;
        self.fewest = self.fewest.min(different);
        self.most = self.most.max(different);
        if self.since.elapsed() >= Duration::from_secs(5) {
            eprintln!(
                "frame blending: the screen showed {:.0} pictures/s; {looks} looks per frame, {}-{} different pictures in each, {:.0}% of frames with {looks}; {} pictures too late, over {} frames",
                self.pictures as f64 / self.since.elapsed().as_secs_f64(),
                self.fewest,
                self.most,
                self.full as f64 * 100.0 / self.frames as f64,
                self.late,
                self.frames
            );
            *self = Self::new();
        }
    }
}
