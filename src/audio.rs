use anyhow::{Context, Result, anyhow, bail};
use pulseaudio::protocol as pa;
use std::collections::VecDeque;
use std::ffi::CString;
use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

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

fn spec() -> pa::SampleSpec {
    pa::SampleSpec { format: pa::SampleFormat::S16Le, channels: 1, sample_rate: RATE }
}

fn cstr(s: &str) -> Result<CString> {
    CString::new(s).map_err(|_| anyhow!("invalid name {s:?}"))
}

/// A blocking connection to the PulseAudio (or PipeWire-pulse) server, speaking the native
/// protocol directly, so no libpulse or `pactl` is needed.
struct Conn {
    sock: BufReader<UnixStream>,
    version: u16,
    seq: u32,
}

impl Conn {
    fn connect(client: &str) -> Result<Self> {
        let path = pulseaudio::socket_path_from_env().context("PulseAudio / PipeWire is not running")?;
        let sock = UnixStream::connect(&path).with_context(|| format!("cannot connect to {}", path.display()))?;
        let mut c = Self { sock: BufReader::new(sock), version: pa::MAX_VERSION, seq: 0 };
        let cookie = pulseaudio::cookie_path_from_env().and_then(|p| std::fs::read(p).ok()).unwrap_or_default();
        let auth: pa::AuthReply =
            c.request(pa::Command::Auth(pa::AuthParams { version: pa::MAX_VERSION, supports_shm: false, supports_memfd: false, cookie }))?;
        c.version = auth.version.min(pa::MAX_VERSION);
        let mut props = pa::Props::new();
        props.set(pa::Prop::ApplicationName, cstr(client)?);
        let _: pa::SetClientNameReply = c.request(pa::Command::SetClientName(props))?;
        Ok(c)
    }

    fn send(&mut self, cmd: &pa::Command) -> Result<u32> {
        self.seq += 1;
        pa::write_command_message(self.sock.get_mut(), self.seq, cmd, self.version).context("pulse: write")?;
        Ok(self.seq)
    }

    fn request<T: pa::CommandReply>(&mut self, cmd: pa::Command) -> Result<T> {
        let seq = self.send(&cmd)?;
        let (got, reply) = pa::read_reply_message::<T>(&mut self.sock, self.version).context("pulse: server refused the request")?;
        anyhow::ensure!(got == seq, "pulse: unexpected reply");
        Ok(reply)
    }

    /// For commands whose reply carries a payload the library has no type for (LoadModule):
    /// waits for the reply and skips its payload.
    fn request_ignoring_payload(&mut self, cmd: pa::Command) -> Result<()> {
        let seq = self.send(&cmd)?;
        let desc = pa::read_descriptor(&mut self.sock).context("pulse: read")?;
        let mut body = vec![0u8; desc.length as usize];
        self.sock.read_exact(&mut body).context("pulse: read")?;
        let (got, reply) = pa::Command::read_tag_prefixed(&mut body.as_slice(), self.version).context("pulse: bad reply")?;
        match reply {
            pa::Command::Reply if got == seq => Ok(()),
            pa::Command::Error(e) => bail!("pulse: {e:?}"),
            _ => bail!("pulse: unexpected reply"),
        }
    }

    fn server_info(&mut self) -> Result<pa::ServerInfo> {
        self.request(pa::Command::GetServerInfo)
    }
}

/// Resolved on every (re)connect so a switch of headphones is picked up.
fn source_name(conn: &mut Conn, source: Source) -> Result<String> {
    let info = conn.server_info()?;
    let name = match source {
        Source::Speakers => info.default_sink_name.ok_or_else(|| anyhow!("no default output device"))?.to_string_lossy().into_owned() + ".monitor",
        Source::Mic => info.default_source_name.ok_or_else(|| anyhow!("no default input device"))?.to_string_lossy().into_owned(),
    };
    Ok(name)
}

/// Blocking capture. `on_chunk` returns false to stop.
pub fn capture(source: Source, stop: &AtomicBool, mut on_chunk: impl FnMut(Vec<u8>) -> bool) -> Result<()> {
    let mut conn = Conn::connect("assistant")?;
    let dev = source_name(&mut conn, source)?;
    let stream_name = match source {
        Source::Speakers => "zoom-monitor",
        Source::Mic => "microphone",
    };
    let mut props = pa::Props::new();
    props.set(pa::Prop::MediaName, cstr(stream_name)?);
    let mut params = pa::RecordStreamParams {
        sample_spec: spec(),
        channel_map: pa::ChannelMap::mono(),
        source_name: Some(cstr(&dev)?),
        cvolume: Some(pa::ChannelVolume::norm(1)),
        props,
        ..Default::default()
    };
    // Ask for ~100 ms fragments (the other buffer attributes stay at the server's defaults).
    params.buffer_attr.fragment_size = CHUNK as u32;
    let _: pa::CreateRecordStreamReply = conn.request(pa::Command::CreateRecordStream(params))?;

    // The server delivers data in blocks of any size; re-cut it into ~100 ms frames.
    let mut pending: Vec<u8> = Vec::with_capacity(CHUNK * 2);
    let mut block = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let desc = pa::read_descriptor(&mut conn.sock).context("pulse read")?;
        if desc.channel == u32::MAX {
            // A command from the server: only "stream killed" matters.
            let (_, cmd) = pa::Command::read_tag_prefixed(&mut (&mut conn.sock).take(desc.length as u64), conn.version).context("pulse read")?;
            match cmd {
                pa::Command::RecordStreamKilled(_) => bail!("pulse: the audio device was removed"),
                pa::Command::Error(e) => bail!("pulse: {e:?}"),
                _ => {}
            }
            continue;
        }
        block.resize(desc.length as usize, 0);
        conn.sock.read_exact(&mut block).context("pulse read")?;
        pending.extend_from_slice(&block);
        while pending.len() >= CHUNK {
            let frame: Vec<u8> = pending.drain(..CHUNK).collect();
            if !on_chunk(frame) {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Plays 24 kHz s16le mono PCM to the named sink and waits until it has been played (blocking).
pub fn play(device: &str, pcm: &[u8]) -> Result<()> {
    let mut conn = Conn::connect("assistant")?;
    let mut props = pa::Props::new();
    props.set(pa::Prop::MediaName, cstr("interviewer")?);
    let stream: pa::CreatePlaybackStreamReply = conn.request(pa::Command::CreatePlaybackStream(pa::PlaybackStreamParams {
        sample_spec: spec(),
        channel_map: pa::ChannelMap::mono(),
        sink_name: Some(cstr(device)?),
        cvolume: Some(pa::ChannelVolume::norm(1)),
        props,
        ..Default::default()
    }))?;
    let mut sent = 0usize;
    let mut drain_seq = None;
    let send = |conn: &mut Conn, sent: &mut usize, want: usize| -> Result<()> {
        let end = (*sent + want).min(pcm.len());
        if end > *sent {
            pa::write_memblock(conn.sock.get_mut(), stream.channel, &pcm[*sent..end], 0).context("pulse write")?;
            conn.sock.get_mut().flush()?;
            *sent = end;
        }
        Ok(())
    };
    send(&mut conn, &mut sent, stream.requested_bytes as usize)?;
    loop {
        if sent == pcm.len() && drain_seq.is_none() {
            drain_seq = Some(conn.send(&pa::Command::DrainPlaybackStream(stream.channel))?);
        }
        let (seq, cmd) = pa::read_command_message(&mut conn.sock, conn.version).context("pulse read")?;
        match cmd {
            pa::Command::Request(r) if r.channel == stream.channel => send(&mut conn, &mut sent, r.length as usize)?,
            pa::Command::Reply if Some(seq) == drain_seq => return Ok(()),
            pa::Command::PlaybackStreamKilled(_) => bail!("pulse: the audio device was removed"),
            pa::Command::Error(e) => bail!("pulse: {e:?}"),
            _ => {}
        }
    }
}

/// Creates the virtual sink and microphone for the interviewer simulator if they do not exist.
pub fn ensure_virtual_devices(sink: &str, mic: &str) -> Result<()> {
    let mut conn = Conn::connect("assistant")?;
    let sinks: Vec<pa::SinkInfo> = conn.request(pa::Command::GetSinkInfoList)?;
    if !sinks.iter().any(|s| s.name.to_bytes() == sink.as_bytes()) {
        let args = format!("sink_name={sink} sink_properties=device.description=Interviewer_out");
        conn.request_ignoring_payload(pa::Command::LoadModule(pa::LoadModuleParams { name: cstr("module-null-sink")?, arguments: Some(cstr(&args)?) }))?;
    }
    let sources: Vec<pa::SourceInfo> = conn.request(pa::Command::GetSourceInfoList)?;
    if !sources.iter().any(|s| s.name.to_bytes() == mic.as_bytes()) {
        let args = format!("master={sink}.monitor source_name={mic} source_properties=device.description=Interviewer_mic");
        conn.request_ignoring_payload(pa::Command::LoadModule(pa::LoadModuleParams { name: cstr("module-remap-source")?, arguments: Some(cstr(&args)?) }))?;
    }
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

/// Gate threshold (RMS) that can be changed while capturing; stored as `f32` bits.
pub type Threshold = Arc<AtomicU32>;

pub fn threshold(value: f64) -> Threshold {
    Arc::new(AtomicU32::new((value as f32).to_bits()))
}

pub fn threshold_get(t: &Threshold) -> f64 {
    f64::from(f32::from_bits(t.load(Ordering::Relaxed)))
}

pub fn threshold_set(t: &Threshold, value: f64) {
    t.store((value as f32).to_bits(), Ordering::Relaxed);
}

/// Result of pushing one frame through the gate.
pub struct GateOut {
    /// Frames to send now (possibly none).
    pub frames: Vec<Vec<u8>>,
    /// Level of the pushed frame.
    pub rms: f64,
    /// The frame itself goes out (speech or its tail), not just held back as lead-in.
    pub sent: bool,
}

/// Decides which ~100 ms frames are worth sending: speech plus a short lead-in and tail.
/// Pure silence is dropped, so nothing is paid for it.
pub struct SilenceGate {
    threshold: Threshold,
    pre: VecDeque<Vec<u8>>,
    hangover: usize,
}

impl SilenceGate {
    pub fn new(threshold: Threshold) -> Self {
        Self { threshold, pre: VecDeque::new(), hangover: 0 }
    }

    pub fn push(&mut self, frame: Vec<u8>) -> GateOut {
        let rms = rms(&frame);
        if rms >= threshold_get(&self.threshold) {
            self.hangover = HANGOVER_FRAMES;
            let mut frames: Vec<Vec<u8>> = self.pre.drain(..).collect();
            frames.push(frame);
            GateOut { frames, rms, sent: true }
        } else if self.hangover > 0 {
            self.hangover -= 1;
            GateOut { frames: vec![frame], rms, sent: true }
        } else {
            self.pre.push_back(frame);
            while self.pre.len() > PRE_FRAMES {
                self.pre.pop_front();
            }
            GateOut { frames: Vec::new(), rms, sent: false }
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
        let mut g = SilenceGate::new(threshold(300.0));
        for _ in 0..50 {
            assert!(g.push(frame(10)).frames.is_empty());
        }
    }

    #[test]
    fn speech_brings_lead_in_then_tail() {
        let mut g = SilenceGate::new(threshold(300.0));
        for _ in 0..10 {
            g.push(frame(10));
        }
        // 3 frames of lead-in (the oldest are dropped) plus the speech frame itself.
        assert_eq!(g.push(frame(2000)).frames.len(), PRE_FRAMES + 1);
        // Right after speech, exactly HANGOVER_FRAMES silent frames still go out.
        let sent = (0..30).filter(|_| !g.push(frame(10)).frames.is_empty()).count();
        assert_eq!(sent, HANGOVER_FRAMES);
    }

    #[test]
    fn threshold_change_applies_to_next_frame() {
        let t = threshold(300.0);
        let mut g = SilenceGate::new(t.clone());
        assert!(!g.push(frame(1000)).frames.is_empty());
        for _ in 0..HANGOVER_FRAMES {
            g.push(frame(10));
        }
        threshold_set(&t, 2000.0);
        let out = g.push(frame(1000));
        assert!(!out.sent && out.frames.is_empty());
        assert_eq!(out.rms, 1000.0);
    }

    #[test]
    fn continuous_speech_is_passed_through() {
        let mut g = SilenceGate::new(threshold(300.0));
        for _ in 0..20 {
            assert_eq!(g.push(frame(2000)).frames.len(), 1);
        }
    }
}

#[cfg(test)]
mod pulse_tests {
    use super::*;

    /// Needs a running sound server (leaves two virtual devices behind): `cargo test -- --ignored pulse`.
    #[test]
    #[ignore]
    fn virtual_devices_and_playback() {
        ensure_virtual_devices("assistant_test_out", "assistant_test_mic").unwrap();
        ensure_virtual_devices("assistant_test_out", "assistant_test_mic").unwrap(); // idempotent
        let pcm: Vec<u8> = (0..RATE as usize / 2).flat_map(|i| (((i as f64 * 0.1).sin() * 8000.0) as i16).to_le_bytes()).collect();
        play("assistant_test_out", &pcm).unwrap();
    }
}
