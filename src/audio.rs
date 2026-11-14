use anyhow::{Context, Result};
use libpulse_binding::def::BufferAttr;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;
use std::collections::VecDeque;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

/// Sample rate expected by OpenAI Realtime (pcm16, mono).
pub const RATE: u32 = 24000;
/// ~100 ms of audio.
const CHUNK: usize = (RATE as usize / 10) * 2;

/// What to record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Monitor of the default output: whatever plays through the speakers (Zoom).
    Speakers,
    /// Default input device: your own voice.
    Mic,
}

const SPEC: Spec = Spec { format: Format::S16le, channels: 1, rate: RATE };

/// Runs `pactl <args>` and returns its trimmed stdout.
pub fn pactl(args: &[&str]) -> Result<String> {
    let out = Command::new("pactl").args(args).output().context("could not run pactl")?;
    anyhow::ensure!(out.status.success(), "pactl {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Resolved on every (re)connect so a switch of headphones is picked up.
fn source_name(source: Source) -> Result<String> {
    match source {
        Source::Speakers => Ok(format!("{}.monitor", pactl(&["get-default-sink"])?)),
        Source::Mic => pactl(&["get-default-source"]),
    }
}

/// Blocking capture. `on_chunk` returns false to stop.
pub fn capture(source: Source, stop: &AtomicBool, mut on_chunk: impl FnMut(Vec<u8>) -> bool) -> Result<()> {
    let dev = source_name(source)?;
    let stream_name = match source {
        Source::Speakers => "zoom-monitor",
        Source::Mic => "microphone",
    };
    let attr = BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: CHUNK as u32,
    };
    let s = Simple::new(None, "assistant", Direction::Record, Some(&dev), stream_name, &SPEC, None, Some(&attr))
        .map_err(|e| anyhow::anyhow!("pulse: {e}"))?;
    let mut buf = vec![0u8; CHUNK];
    while !stop.load(Ordering::Relaxed) {
        s.read(&mut buf).map_err(|e| anyhow::anyhow!("pulse read: {e}"))?;
        if !on_chunk(buf.clone()) {
            break;
        }
    }
    Ok(())
}

/// Plays 24 kHz s16le mono PCM to the named sink and waits until it has been played (blocking).
pub fn play(device: &str, pcm: &[u8]) -> Result<()> {
    let s = Simple::new(None, "assistant", Direction::Playback, Some(device), "interviewer", &SPEC, None, None)
        .map_err(|e| anyhow::anyhow!("pulse: {e}"))?;
    s.write(pcm).map_err(|e| anyhow::anyhow!("pulse write: {e}"))?;
    s.drain().map_err(|e| anyhow::anyhow!("pulse drain: {e}"))?;
    Ok(())
}

/// Records `secs` seconds to a WAV file for checking (`--test-audio`).
pub fn record_wav(path: &str, secs: u32) -> Result<()> {
    let stop = AtomicBool::new(false);
    let mut pcm = Vec::new();
    let target = (RATE * 2 * secs) as usize;
    capture(Source::Speakers, &stop, |c| {
        pcm.extend_from_slice(&c);
        pcm.len() < target
    })?;
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    wav.extend_from_slice(&pcm);
    std::fs::write(path, wav)?;
    Ok(())
}

/// Frames kept before speech starts, so the first syllable is not cut (~300 ms).
const PRE_FRAMES: usize = 3;
/// Frames still sent after speech stops (~1 s): longer than the server's 500 ms VAD silence,
/// so it can close the utterance.
const HANGOVER_FRAMES: usize = 10;

/// Decides which ~100 ms frames are worth sending: speech plus a short lead-in and tail.
/// Pure silence is dropped, so nothing is paid for it.
pub struct SilenceGate {
    threshold: f64,
    pre: VecDeque<Vec<u8>>,
    hangover: usize,
}

impl SilenceGate {
    pub fn new(threshold: f64) -> Self {
        Self { threshold, pre: VecDeque::new(), hangover: 0 }
    }

    /// Returns the frames to send now (possibly none).
    pub fn push(&mut self, frame: Vec<u8>) -> Vec<Vec<u8>> {
        if rms(&frame) >= self.threshold {
            self.hangover = HANGOVER_FRAMES;
            let mut out: Vec<Vec<u8>> = self.pre.drain(..).collect();
            out.push(frame);
            out
        } else if self.hangover > 0 {
            self.hangover -= 1;
            vec![frame]
        } else {
            self.pre.push_back(frame);
            while self.pre.len() > PRE_FRAMES {
                self.pre.pop_front();
            }
            Vec::new()
        }
    }
}

/// Root-mean-square level of little-endian i16 samples.
fn rms(frame: &[u8]) -> f64 {
    let (samples, _) = frame.as_chunks::<2>();
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|b| f64::from(i16::from_le_bytes(*b)).powi(2)).sum();
    (sum / samples.len() as f64).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(level: i16) -> Vec<u8> {
        std::iter::repeat_n(level.to_le_bytes(), 2400).flatten().collect()
    }

    #[test]
    fn rms_of_constant_signal() {
        assert_eq!(rms(&frame(1000)), 1000.0);
        assert_eq!(rms(&frame(0)), 0.0);
        assert_eq!(rms(&[]), 0.0);
    }

    #[test]
    fn silence_sends_nothing() {
        let mut g = SilenceGate::new(300.0);
        for _ in 0..50 {
            assert!(g.push(frame(10)).is_empty());
        }
    }

    #[test]
    fn speech_brings_lead_in_then_tail() {
        let mut g = SilenceGate::new(300.0);
        for _ in 0..10 {
            g.push(frame(10));
        }
        // 3 frames of lead-in (the oldest are dropped) plus the speech frame itself.
        assert_eq!(g.push(frame(2000)).len(), PRE_FRAMES + 1);
        // Right after speech, exactly HANGOVER_FRAMES silent frames still go out.
        let sent = (0..30).filter(|_| !g.push(frame(10)).is_empty()).count();
        assert_eq!(sent, HANGOVER_FRAMES);
    }

    #[test]
    fn continuous_speech_is_passed_through() {
        let mut g = SilenceGate::new(300.0);
        for _ in 0..20 {
            assert_eq!(g.push(frame(2000)).len(), 1);
        }
    }
}
