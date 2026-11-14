//! Per-session debug record: `sessions/<local time>/session.log` (transcript, translations,
//! hints with sources, errors) and raw audio (`them.wav`, `me.wav`).

use crate::config::Config;
use crate::state::{Hint, Speaker};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

/// Re-patch the WAV header after this many bytes (~10 s), so a crash leaves a playable file.
const PATCH_EVERY: u64 = 24000 * 2 * 10;

pub struct SessionLog {
    dir: PathBuf,
    start: Instant,
    inner: Mutex<Inner>,
}

struct Inner {
    out: BufWriter<File>,
    next_no: usize,
    numbers: HashMap<String, usize>,
}

fn local_stamp() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!("{:04}-{:02}-{:02}_{:02}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

impl SessionLog {
    /// Creates `<workdir>/sessions/<timestamp>/` and writes the header line.
    pub fn create(base: &Path, cfg: &Config, topic: Option<&str>) -> Result<Self> {
        let mut dir = base.join("sessions").join(local_stamp());
        // Two sessions in the same second: add a suffix instead of mixing them.
        let mut n = 1;
        while dir.exists() {
            n += 1;
            dir = base.join("sessions").join(format!("{}-{n}", local_stamp()));
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let file = File::create(dir.join("session.log")).context("cannot create session.log")?;
        let log = Self { dir, start: Instant::now(), inner: Mutex::new(Inner { out: BufWriter::new(file), next_no: 0, numbers: HashMap::new() }) };
        let header = format!(
            "assistant v{} · topic: {} · transcribe: {} · translate: {} · hint models: {}",
            env!("CARGO_PKG_VERSION"),
            topic.unwrap_or("-"),
            cfg.transcribe.model,
            cfg.llm.translate_model,
            cfg.llm.hint_models.join(", ")
        );
        log.write_raw(&header);
        Ok(log)
    }

    fn stamp(&self) -> String {
        let t = self.start.elapsed().as_secs_f64();
        format!("[{:02}:{:04.1}]", (t / 60.0) as u64, t % 60.0)
    }

    fn write_raw(&self, text: &str) {
        let mut g = self.inner.lock().unwrap();
        let _ = writeln!(g.out, "{text}");
        let _ = g.out.flush();
    }

    fn write(&self, text: &str) {
        self.write_raw(&format!("{} {text}", self.stamp()));
    }

    /// The number of an utterance, assigned on first use.
    fn number(&self, item_id: &str) -> usize {
        let mut g = self.inner.lock().unwrap();
        if let Some(n) = g.numbers.get(item_id) {
            return *n;
        }
        g.next_no += 1;
        let n = g.next_no;
        g.numbers.insert(item_id.to_string(), n);
        n
    }

    pub fn utterance(&self, item_id: &str, speaker: Speaker, es: &str) {
        let who = if speaker == Speaker::Me { "ME  " } else { "THEM" };
        self.write(&format!("#{} {who}: {}", self.number(item_id), es.trim()));
    }

    pub fn translation(&self, item_id: &str, en: &str) {
        self.write(&format!("#{} EN: {}", self.number(item_id), en.trim()));
    }

    pub fn hint(&self, h: &Hint, requested: bool) {
        let kind = if requested { "requested" } else { "prepared" };
        let sources = if h.sources.is_empty() { "-".to_string() } else { h.sources.join(", ") };
        self.write(&format!(
            "HINT {kind}\n          Q: {}\n          ES: {}\n          EN: {}\n          Sources: {sources}",
            h.question, h.es, h.en
        ));
    }

    pub fn error(&self, e: &str) {
        self.write(&format!("ERROR {e}"));
    }

    pub fn stopped(&self) {
        self.write("stopped");
    }

    pub fn wav(&self, name: &str) -> Result<WavWriter> {
        WavWriter::create(&self.dir.join(name))
    }
}

/// Appends 24 kHz s16le mono PCM to a WAV file; sizes in the header are patched
/// periodically and on drop.
pub struct WavWriter {
    file: BufWriter<File>,
    data_len: u64,
    patched_at: u64,
}

impl WavWriter {
    pub fn create(path: &Path) -> Result<Self> {
        let mut file = BufWriter::new(File::create(path).with_context(|| format!("cannot create {}", path.display()))?);
        file.write_all(&crate::audio::wav_header(0))?;
        Ok(Self { file, data_len: 0, patched_at: 0 })
    }

    pub fn write(&mut self, pcm: &[u8]) {
        if self.file.write_all(pcm).is_err() {
            return;
        }
        self.data_len += pcm.len() as u64;
        if self.data_len - self.patched_at >= PATCH_EVERY {
            self.patch();
        }
    }

    fn patch(&mut self) {
        let header = crate::audio::wav_header(self.data_len.min(u32::MAX as u64) as u32);
        // Flush buffered PCM first so that the header never claims more than is on disk.
        if self.file.flush().is_ok() {
            let f = self.file.get_mut();
            let _ = f.seek(SeekFrom::Start(0)).and_then(|_| f.write_all(&header)).and_then(|_| f.seek(SeekFrom::End(0)));
        }
        self.patched_at = self.data_len;
    }
}

impl Drop for WavWriter {
    fn drop(&mut self) {
        self.patch();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("assistant-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn wav_has_correct_sizes() {
        let d = tmp("wav");
        let path = d.join("a.wav");
        {
            let mut w = WavWriter::create(&path).unwrap();
            w.write(&[1u8; 1000]);
            w.write(&[2u8; 500]);
        }
        let b = std::fs::read(&path).unwrap();
        assert_eq!(b.len(), 44 + 1500);
        assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 36 + 1500);
        assert_eq!(u32::from_le_bytes(b[40..44].try_into().unwrap()), 1500);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn log_lines_are_numbered_and_paired() {
        let d = tmp("log");
        let cfg: Config = toml::from_str(&std::fs::read_to_string("config.toml").unwrap()).unwrap();
        let log = SessionLog::create(&d, &cfg, Some("topic")).unwrap();
        log.utterance("them:1", Speaker::Them, "¿Cuántos años?");
        log.translation("them:1", "How many years?");
        log.utterance("me:1", Speaker::Me, "Cinco.");
        log.stopped();
        let dir = std::fs::read_dir(d.join("sessions")).unwrap().next().unwrap().unwrap().path();
        let text = std::fs::read_to_string(dir.join("session.log")).unwrap();
        assert!(text.contains("topic: topic"));
        assert!(text.contains("#1 THEM: ¿Cuántos años?") && text.contains("#1 EN: How many years?"));
        assert!(text.contains("#2 ME  : Cinco.") && text.contains("stopped"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn wav_is_playable_after_periodic_patch_without_drop() {
        let d = tmp("patch");
        let path = d.join("a.wav");
        let mut w = WavWriter::create(&path).unwrap();
        w.write(&vec![0u8; PATCH_EVERY as usize]);
        let b = std::fs::read(&path).unwrap();
        assert_eq!(u32::from_le_bytes(b[40..44].try_into().unwrap()) as u64, PATCH_EVERY);
        std::mem::forget(w);
        let _ = std::fs::remove_dir_all(d);
    }
}
