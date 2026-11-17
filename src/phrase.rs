//! Where a phrase of the other person ends, decided by the app itself.
//!
//! The live recognizer streams text while the person speaks but has no pause detection of its own,
//! so the app has to say "this phrase is over" (`input_audio_buffer.commit`). Only then the phrase
//! is final and gets translated and judged for a hint. `PhraseCutter` is that decision, kept free of
//! I/O: it is told, frame by frame, whether the sound is loud, and answers when to commit.

use std::time::{Duration, Instant};

/// Why a phrase was closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// A quiet time of `silence` after the last loud frame: the phrase really ended.
    Pause,
    /// The phrase went on longer than `soft` and the sound dipped for a moment: cut here so translation need not wait.
    Long,
    /// The phrase went on longer than `hard` without a dip: cut anyway.
    Hard,
}

impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Reason::Pause => "pause",
            Reason::Long => "long",
            Reason::Hard => "hard",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cut {
    None,
    /// The first loud frame of a new phrase.
    Started,
    /// Close the phrase now (send `commit`).
    Commit(Reason),
}

/// A dip must last this long for a long phrase to be cut there: shorter gaps are mostly between two
/// words of the same sentence ("inteligencia | artificial"), longer ones are commas and breaths.
const LONG_DIP: Duration = Duration::from_millis(200);

pub struct PhraseCutter {
    silence: Duration,
    soft: Duration,
    hard: Duration,
    started: Option<Instant>,
    last_loud: Option<Instant>,
}

impl PhraseCutter {
    pub fn new(silence: Duration, soft: Duration, hard: Duration) -> Self {
        Self { silence, soft, hard, started: None, last_loud: None }
    }

    /// One 100 ms frame arrived at `now`; `loud` says it is above the silence threshold.
    pub fn on_frame(&mut self, now: Instant, loud: bool) -> Cut {
        if loud {
            if self.started.is_none() {
                (self.started, self.last_loud) = (Some(now), Some(now));
                return Cut::Started;
            }
            self.last_loud = Some(now);
            // Never dips: cut after `hard`; the next loud frame starts the next phrase.
            if self.age(now) >= self.hard {
                return self.close(Reason::Hard);
            }
            Cut::None
        } else {
            self.quiet(now)
        }
    }

    /// No frame came (the silence gate swallows them after the hangover), but time passes.
    pub fn on_tick(&mut self, now: Instant) -> Cut {
        self.quiet(now)
    }

    fn quiet(&mut self, now: Instant) -> Cut {
        let (Some(_), Some(last_loud)) = (self.started, self.last_loud) else { return Cut::None };
        if now.saturating_duration_since(last_loud) >= self.silence {
            self.close(Reason::Pause)
        } else if self.age(now) >= self.hard {
            self.close(Reason::Hard)
        } else if self.age(now) >= self.soft && now.saturating_duration_since(last_loud) >= LONG_DIP {
            self.close(Reason::Long)
        } else {
            Cut::None
        }
    }

    fn age(&self, now: Instant) -> Duration {
        self.started.map_or(Duration::ZERO, |s| now.saturating_duration_since(s))
    }

    fn close(&mut self, why: Reason) -> Cut {
        (self.started, self.last_loud) = (None, None);
        Cut::Commit(why)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn cutter() -> (PhraseCutter, Instant) {
        (PhraseCutter::new(500 * MS, 12_000 * MS, 20_000 * MS), Instant::now())
    }

    /// Feeds `n` frames of 100 ms, starting at `t0`; returns what happened and the time after them.
    fn feed(c: &mut PhraseCutter, t0: Instant, n: u32, loud: bool) -> (Vec<(u32, Cut)>, Instant) {
        let mut out = Vec::new();
        for i in 0..n {
            let cut = c.on_frame(t0 + 100 * MS * i, loud);
            if cut != Cut::None {
                out.push((i, cut));
            }
        }
        (out, t0 + 100 * MS * n)
    }

    #[test]
    fn first_loud_frame_starts_a_phrase_once() {
        let (mut c, t0) = cutter();
        let (out, _) = feed(&mut c, t0, 10, true);
        assert_eq!(out, [(0, Cut::Started)]);
    }

    #[test]
    fn nothing_happens_in_silence() {
        let (mut c, t0) = cutter();
        let (out, t) = feed(&mut c, t0, 50, false);
        assert!(out.is_empty());
        assert_eq!(c.on_tick(t), Cut::None);
    }

    #[test]
    fn a_pause_of_the_set_length_closes_the_phrase() {
        let (mut c, t0) = cutter();
        let (_, t) = feed(&mut c, t0, 5, true);
        // The last loud frame was at 400 ms; quiet frames at 500, 600, ... 900 ms: 500 ms of quiet is reached at 900 ms.
        let (out, _) = feed(&mut c, t, 10, false);
        assert_eq!(out, [(4, Cut::Commit(Reason::Pause))]);
    }

    #[test]
    fn a_short_dip_does_not_close_it() {
        let (mut c, t0) = cutter();
        let (_, t) = feed(&mut c, t0, 5, true);
        let (out, t) = feed(&mut c, t, 3, false); // 300 ms of quiet
        assert!(out.is_empty());
        let (out, _) = feed(&mut c, t, 5, true);
        assert!(out.is_empty(), "same phrase goes on: {out:?}");
    }

    #[test]
    fn a_tick_closes_the_phrase_when_frames_stopped_coming() {
        let (mut c, t0) = cutter();
        let (_, t) = feed(&mut c, t0, 5, true); // last loud at 400 ms, `t` = 500 ms
        assert_eq!(c.on_tick(t), Cut::None);
        assert_eq!(c.on_tick(t0 + 899 * MS), Cut::None);
        assert_eq!(c.on_tick(t0 + 900 * MS), Cut::Commit(Reason::Pause));
        // Closed: it does not fire again, and the next loud frame starts a new phrase.
        assert_eq!(c.on_tick(t0 + 1500 * MS), Cut::None);
        assert_eq!(c.on_frame(t0 + 2000 * MS, true), Cut::Started);
    }

    #[test]
    fn a_long_phrase_is_cut_at_the_first_real_dip_after_the_soft_limit() {
        let (mut c, t0) = cutter();
        let (out, t) = feed(&mut c, t0, 125, true); // 12.5 s of loud speech: no dip yet
        assert_eq!(out, [(0, Cut::Started)]);
        // A gap of one frame is a gap between words; the second quiet frame is a real dip.
        let (out, t) = feed(&mut c, t, 1, false);
        assert!(out.is_empty(), "{out:?}");
        let (out, t) = feed(&mut c, t, 1, false);
        assert_eq!(out, [(0, Cut::Commit(Reason::Long))]);
        // The speech goes on: a new phrase starts at the next loud frame.
        let (out, _) = feed(&mut c, t, 3, true);
        assert_eq!(out, [(0, Cut::Started)]);
    }

    #[test]
    fn a_dip_before_the_soft_limit_is_not_a_cut() {
        let (mut c, t0) = cutter();
        let (_, t) = feed(&mut c, t0, 100, true); // 10 s
        let (out, _) = feed(&mut c, t, 3, false);
        assert!(out.is_empty());
    }

    #[test]
    fn speech_without_any_dip_is_cut_at_the_hard_limit() {
        let (mut c, t0) = cutter();
        let (out, _) = feed(&mut c, t0, 210, true); // 21 s, loud all the time
        // The frame after the cut is loud again: it starts the next phrase.
        assert_eq!(out, [(0, Cut::Started), (200, Cut::Commit(Reason::Hard)), (201, Cut::Started)]);
    }

    #[test]
    fn the_hard_limit_also_applies_on_quiet_frames_and_ticks() {
        let (mut c, t0) = cutter();
        // Loud until just now (so no pause), but the phrase is 20 s old: a tick cuts it.
        feed(&mut c, t0, 1, true);
        c.last_loud = Some(t0 + 19_900 * MS);
        assert_eq!(c.on_tick(t0 + 20_000 * MS), Cut::Commit(Reason::Hard));
    }

    #[test]
    fn labels() {
        assert_eq!([Reason::Pause, Reason::Long, Reason::Hard].map(Reason::label), ["pause", "long", "hard"]);
    }
}
