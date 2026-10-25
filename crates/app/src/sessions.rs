//! Whole sessions of the games on the list: with "Record whole sessions"
//! on, a game on the games and apps list opening starts a recording, and the
//! last one closing stops it, saved. Clipping, if it ran, gives way while
//! the recording runs and starts again after (empty).
//!
//! A recording is the session's only when it was started for it: one you
//! started yourself is never stopped by a game closing. One you stop by hand
//! isn't started again until the games close and one opens again.

use std::time::{Duration, Instant};

/// How long the games stay closed before the session's over: a game that
/// restarts, or hands over from its launcher, stays one recording.
pub const GRACE: Duration = Duration::from_secs(10);

/// What the app should do now.
#[derive(Debug, PartialEq)]
pub enum Do {
    Nothing,
    /// Start recording (clipping gives way).
    Record,
    /// Stop the session's recording: the games closed.
    Stop,
}

#[derive(Debug, Default)]
pub struct Sessions {
    /// A game on the list is open, or closed less than `GRACE` ago.
    session: bool,
    /// When the last game closed, while it's in its grace.
    closed_at: Option<Instant>,
    /// The recording running is the session's: whether clipping ran before
    /// it took over.
    ours: Option<bool>,
    /// A recording was stopped by hand this session.
    declined: bool,
}

impl Sessions {
    /// What to do now. `watching`: Record whole sessions is on (and can
    /// work); `open`: a game on the list is open; `recording`, `buffering`:
    /// what's captured now.
    pub fn step(&mut self, watching: bool, open: bool, recording: bool, buffering: bool, now: Instant) -> Do {
        if !watching {
            // Turned off: a recording that runs goes on, as yours.
            *self = Self::default();
            return Do::Nothing;
        }
        if open {
            self.session = true;
            self.closed_at = None;
            // Once per session: a recording that couldn't start isn't tried
            // again and again.
            if !recording && self.ours.is_none() && !self.declined {
                self.ours = Some(buffering);
                return Do::Record;
            }
            return Do::Nothing;
        }
        if !self.session {
            return Do::Nothing;
        }
        let since = *self.closed_at.get_or_insert(now);
        if now.duration_since(since) < GRACE {
            return Do::Nothing;
        }
        self.session = false;
        self.closed_at = None;
        self.declined = false;
        if self.ours.is_some() && recording {
            // `ours` stays for `let_go`, which says whether clipping comes back.
            return Do::Stop;
        }
        self.ours = None;
        Do::Nothing
    }

    /// A recording stopped, by hand or as the session ended: whether
    /// clipping starts again (it ran before the session took over). Stopped
    /// by hand during a session, none starts again until it's over.
    pub fn let_go(&mut self) -> bool {
        if self.session {
            self.declined = true;
        }
        self.ours.take().unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Sessions` and a clock, with what the app would do in between.
    struct Run {
        s: Sessions,
        now: Instant,
        recording: bool,
        buffering: bool,
    }

    impl Run {
        fn new(buffering: bool) -> Self {
            Self { s: Sessions::default(), now: Instant::now(), recording: false, buffering }
        }

        /// `secs` later, a look with the game `open` (or not), done as the
        /// app does it.
        fn at(&mut self, secs: u64, open: bool) -> Do {
            self.now += Duration::from_secs(secs);
            let d = self.s.step(true, open, self.recording, self.buffering, self.now);
            match d {
                Do::Record => (self.recording, self.buffering) = (true, false),
                Do::Stop => self.stop(),
                Do::Nothing => {}
            }
            d
        }

        fn stop(&mut self) {
            self.recording = false;
            self.buffering = self.s.let_go();
        }
    }

    #[test]
    fn a_game_opening_records_until_it_closes() {
        let mut r = Run::new(true);
        assert_eq!(r.at(0, false), Do::Nothing);
        assert_eq!(r.at(2, true), Do::Record);
        assert_eq!(r.at(2, true), Do::Nothing);
        // Closed: kept for the grace, then stopped, and clipping comes back.
        assert_eq!(r.at(2, false), Do::Nothing);
        assert_eq!(r.at(GRACE.as_secs() - 1, false), Do::Nothing);
        assert_eq!(r.at(1, false), Do::Stop);
        assert!(!r.recording && r.buffering);
        assert_eq!(r.at(2, false), Do::Nothing);
    }

    #[test]
    fn clipping_off_before_stays_off() {
        let mut r = Run::new(false);
        assert_eq!(r.at(0, true), Do::Record);
        r.at(1, false);
        assert_eq!(r.at(GRACE.as_secs(), false), Do::Stop);
        assert!(!r.buffering);
    }

    #[test]
    fn a_restart_in_the_grace_is_one_recording() {
        let mut r = Run::new(true);
        r.at(0, true);
        r.at(2, false);
        assert_eq!(r.at(GRACE.as_secs() - 2, true), Do::Nothing);
        assert!(r.recording);
        // The grace starts over from the next close.
        r.at(2, false);
        assert_eq!(r.at(GRACE.as_secs() - 1, false), Do::Nothing);
        assert_eq!(r.at(1, false), Do::Stop);
    }

    #[test]
    fn stopped_by_hand_waits_for_the_next_session() {
        let mut r = Run::new(true);
        r.at(0, true);
        r.stop();
        // Clipping comes back, and no recording while the game stays open.
        assert!(r.buffering);
        assert_eq!(r.at(2, true), Do::Nothing);
        assert_eq!(r.at(60, true), Do::Nothing);
        r.at(2, false);
        assert_eq!(r.at(GRACE.as_secs(), false), Do::Nothing);
        // Opened again: a new session.
        assert_eq!(r.at(2, true), Do::Record);
    }

    #[test]
    fn your_own_recording_is_left_alone() {
        let mut r = Run::new(false);
        r.recording = true;
        assert_eq!(r.at(0, true), Do::Nothing);
        r.at(2, false);
        assert_eq!(r.at(GRACE.as_secs(), false), Do::Nothing);
        assert!(r.recording);
    }

    #[test]
    fn your_own_recording_stopped_isnt_followed_by_one() {
        let mut r = Run::new(false);
        r.recording = true;
        r.at(0, true);
        r.stop();
        assert_eq!(r.at(2, true), Do::Nothing);
        assert!(!r.recording);
    }

    #[test]
    fn a_recording_that_failed_isnt_tried_again() {
        let mut r = Run::new(true);
        assert_eq!(r.at(0, true), Do::Record);
        // The capture couldn't start: idle, as the app hears it.
        (r.recording, r.buffering) = (false, false);
        assert_eq!(r.at(2, true), Do::Nothing);
        r.at(2, false);
        assert_eq!(r.at(GRACE.as_secs(), false), Do::Nothing);
        assert_eq!(r.at(2, true), Do::Record);
    }

    #[test]
    fn turned_off_mid_session_the_recording_goes_on() {
        let mut r = Run::new(true);
        r.at(0, true);
        assert_eq!(r.s.step(false, true, true, false, r.now), Do::Nothing);
        // On again with the game open, the recording isn't the session's.
        r.at(2, false);
        assert_eq!(r.at(GRACE.as_secs(), false), Do::Nothing);
        assert!(r.recording);
    }
}
