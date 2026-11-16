//! Debug video of the window: `sessions/<stamp>/screen.mp4` (and `screen_audio.mp4` with the
//! recorded sound mixed in). Frames come from egui's screenshot API and are piped to the system
//! `ffmpeg` at a constant rate, so the video runs on the wall clock like the WAV files do.

use anyhow::{Context, Result, anyhow};
use eframe::egui::ColorImage;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const FPS: u32 = 10;
/// How often the UI should ask egui for a screenshot.
pub const FRAME_EVERY: Duration = Duration::from_millis(1000 / FPS as u64);

/// Keeps the video at a constant frame rate on the wall clock and at a constant size.
struct Pacer<W: Write> {
    out: W,
    w: usize,
    h: usize,
    /// The last frame, RGBA, `w * h * 4` bytes (black until the first frame).
    canvas: Vec<u8>,
    written: u64,
}

impl<W: Write> Pacer<W> {
    /// Video sizes must be even for yuv420p: the first frame is cropped by a pixel if needed.
    fn size_for(first: &ColorImage) -> (usize, usize) {
        (first.size[0] & !1, first.size[1] & !1)
    }

    fn new(out: W, w: usize, h: usize) -> Self {
        Self { out, w, h, canvas: vec![0; w * h * 4], written: 0 }
    }

    /// Copies a frame into the canvas; a window of another size is cropped / padded with black.
    fn set_frame(&mut self, img: &ColorImage) {
        let [iw, ih] = img.size;
        self.canvas.fill(0);
        for y in 0..self.h.min(ih) {
            for x in 0..self.w.min(iw) {
                let p = img.pixels[y * iw + x].to_array();
                let o = (y * self.w + x) * 4;
                self.canvas[o..o + 4].copy_from_slice(&[p[0], p[1], p[2], 255]);
            }
        }
    }

    /// Writes the canvas until the video is as long as `elapsed` seconds.
    fn catch_up(&mut self, elapsed: f64) -> std::io::Result<()> {
        let due = (elapsed * FPS as f64).floor() as u64 + 1;
        while self.written < due {
            self.out.write_all(&self.canvas)?;
            self.written += 1;
        }
        Ok(())
    }
}

enum Msg {
    Frame(Arc<ColorImage>),
}

/// Records one session; `finish` closes the file and mixes the sound in the background.
pub struct VideoRecorder {
    tx: SyncSender<Msg>,
    worker: JoinHandle<()>,
}

impl VideoRecorder {
    /// `on_error` is called (from the worker thread) with a message for the user.
    pub fn start(dir: PathBuf, on_error: impl Fn(String) + Send + 'static) -> Self {
        // Small queue: when ffmpeg is slow, frames are dropped, the UI never waits.
        let (tx, rx) = sync_channel::<Msg>(4);
        let worker = std::thread::spawn(move || {
            if let Err(e) = run(&dir, rx) {
                let _ = std::fs::write(dir.join("screen.error.txt"), format!("{e:#}"));
                on_error(format!("Screen video: {e:#}"));
            }
        });
        Self { tx, worker }
    }

    pub fn push(&self, img: Arc<ColorImage>) {
        if let Err(TrySendError::Disconnected(_)) = self.tx.try_send(Msg::Frame(img)) {
            // The worker has failed and already reported it.
        }
    }

    /// Ends the video; the muxing runs on its own thread, so this does not block the UI.
    pub fn finish(self) {
        let Self { tx, worker } = self;
        drop(tx);
        std::thread::spawn(move || {
            let _ = worker.join();
        });
    }
}

fn spawn_ffmpeg(dir: &Path, w: usize, h: usize) -> Result<Child> {
    let log = std::fs::File::create(dir.join("screen.ffmpeg.log"))?;
    Command::new("ffmpeg")
        .args(["-y", "-loglevel", "warning", "-f", "rawvideo", "-pix_fmt", "rgba", "-s", &format!("{w}x{h}"), "-r", &FPS.to_string(), "-i", "-"])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-crf", "23", "-pix_fmt", "yuv420p"])
        .arg(dir.join("screen.mp4"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .context("cannot start ffmpeg (is it installed?)")
}

fn run(dir: &Path, rx: std::sync::mpsc::Receiver<Msg>) -> Result<()> {
    let start = Instant::now();
    let mut state: Option<(Child, Pacer<std::process::ChildStdin>)> = None;
    while let Ok(Msg::Frame(img)) = rx.recv() {
        if state.is_none() {
            let (w, h) = Pacer::<Vec<u8>>::size_for(&img);
            if w == 0 || h == 0 {
                continue;
            }
            let mut child = spawn_ffmpeg(dir, w, h)?;
            let stdin = child.stdin.take().ok_or_else(|| anyhow!("no ffmpeg stdin"))?;
            state = Some((child, Pacer::new(stdin, w, h)));
        }
        let (_, p) = state.as_mut().unwrap();
        p.set_frame(&img);
        p.catch_up(start.elapsed().as_secs_f64()).context("ffmpeg stopped accepting frames (see screen.ffmpeg.log)")?;
    }
    // Channel closed: the session is over. Pad to the exact end, close the pipe, wait for the file.
    let Some((mut child, mut p)) = state else { return Ok(()) };
    let _ = p.catch_up(start.elapsed().as_secs_f64());
    drop(p);
    let status = child.wait()?;
    if !status.success() {
        return Err(anyhow!("ffmpeg failed: {status} (see screen.ffmpeg.log)"));
    }
    mux_audio(dir)
}

/// True when the data size in the WAV header matches the file (the writer has finished).
fn wav_header_is_final(path: &Path) -> bool {
    let (Ok(bytes), Ok(meta)) = (std::fs::read(path).map(|b| b.into_iter().take(44).collect::<Vec<u8>>()), std::fs::metadata(path)) else {
        return false;
    };
    bytes.len() == 44 && u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]) as u64 + 44 == meta.len()
}

/// `screen_audio.mp4` = the video + the WAV files of the session mixed together.
fn mux_audio(dir: &Path) -> Result<()> {
    let wavs: Vec<PathBuf> = ["them.wav", "me.wav"].iter().map(|n| dir.join(n)).filter(|p| p.exists()).collect();
    // The audio tasks were aborted a moment ago: wait until they have patched the WAV headers.
    for w in &wavs {
        for _ in 0..50 {
            if wav_header_is_final(w) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    if wavs.is_empty() {
        return Ok(());
    }
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-y", "-loglevel", "warning", "-i"]).arg(dir.join("screen.mp4"));
    for w in &wavs {
        cmd.arg("-i").arg(w);
    }
    let inputs: String = (1..=wavs.len()).map(|i| format!("[{i}:a]")).collect();
    cmd.args(["-filter_complex", &format!("{inputs}amix=inputs={}:normalize=0:duration=longest[a]", wavs.len())]);
    cmd.args(["-map", "0:v", "-map", "[a]", "-c:v", "copy", "-c:a", "aac"]).arg(dir.join("screen_audio.mp4"));
    let log = std::fs::OpenOptions::new().append(true).open(dir.join("screen.ffmpeg.log")).or_else(|_| std::fs::File::create(dir.join("screen.ffmpeg.log")))?;
    let status = cmd.stdout(Stdio::null()).stderr(log).status().context("cannot start ffmpeg for the audio")?;
    if !status.success() {
        return Err(anyhow!("mixing the sound failed: {status} (see screen.ffmpeg.log)"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::Color32;

    fn img(w: usize, h: usize, c: Color32) -> ColorImage {
        ColorImage { size: [w, h], pixels: vec![c; w * h] }
    }

    #[test]
    fn odd_sizes_are_made_even() {
        assert_eq!(Pacer::<Vec<u8>>::size_for(&img(441, 821, Color32::BLACK)), (440, 820));
    }

    #[test]
    fn frames_follow_the_wall_clock() {
        let mut p = Pacer::new(Vec::new(), 4, 2);
        p.set_frame(&img(4, 2, Color32::RED));
        p.catch_up(0.0).unwrap();
        assert_eq!(p.written, 1);
        // A stall of 1 s repeats the frame instead of shortening the video.
        p.catch_up(1.0).unwrap();
        assert_eq!(p.written, FPS as u64 + 1);
        p.catch_up(1.0).unwrap();
        assert_eq!(p.written, FPS as u64 + 1);
        assert_eq!(p.out.len() as u64, p.written * 4 * 2 * 4);
        assert_eq!(&p.out[..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn other_window_size_is_cropped_and_padded() {
        let mut p = Pacer::new(Vec::new(), 4, 2);
        p.set_frame(&img(6, 6, Color32::WHITE));
        assert!(p.canvas.iter().all(|b| *b == 255));
        p.set_frame(&img(2, 1, Color32::WHITE));
        assert_eq!(p.canvas.len(), 4 * 2 * 4);
        assert_eq!(&p.canvas[..4], &[255, 255, 255, 255]);
        // Outside the smaller window: black.
        assert_eq!(&p.canvas[2 * 4..2 * 4 + 4], &[0, 0, 0, 0]);
    }
}
