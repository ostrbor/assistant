use anyhow::{Context, Result};
use libpulse_binding::def::BufferAttr;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

/// Sample rate expected by OpenAI Realtime (pcm16, mono).
pub const RATE: u32 = 24000;
/// ~100 ms of audio.
const CHUNK: usize = (RATE as usize / 10) * 2;

/// Monitor source of the current output: whatever plays through the speakers (Zoom).
fn monitor_source() -> Result<String> {
    let out = Command::new("pactl").arg("get-default-sink").output().context("could not run pactl")?;
    anyhow::ensure!(out.status.success(), "pactl get-default-sink failed");
    Ok(format!("{}.monitor", String::from_utf8_lossy(&out.stdout).trim()))
}

/// Blocking capture. `on_chunk` returns false to stop.
pub fn capture(stop: &AtomicBool, mut on_chunk: impl FnMut(Vec<u8>) -> bool) -> Result<()> {
    let spec = Spec { format: Format::S16le, channels: 1, rate: RATE };
    let dev = monitor_source()?;
    let attr = BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: CHUNK as u32,
    };
    let s = Simple::new(None, "assistant", Direction::Record, Some(&dev), "zoom-monitor", &spec, None, Some(&attr))
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

/// Records `secs` seconds to a WAV file for checking (`--test-audio`).
pub fn record_wav(path: &str, secs: u32) -> Result<()> {
    let stop = AtomicBool::new(false);
    let mut pcm = Vec::new();
    let target = (RATE * 2 * secs) as usize;
    capture(&stop, |c| {
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
