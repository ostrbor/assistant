use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Who is talking: the other side (speaker output) or the user (microphone).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speaker {
    Them,
    Me,
}

impl Speaker {
    /// Index into `AppState::meters`.
    pub fn idx(self) -> usize {
        match self {
            Speaker::Them => 0,
            Speaker::Me => 1,
        }
    }

    /// Keeps item ids of the two realtime sessions from colliding.
    pub fn id_prefix(self) -> &'static str {
        match self {
            Speaker::Them => "them:",
            Speaker::Me => "me:",
        }
    }
}

#[derive(Clone)]
pub struct Segment {
    pub item_id: String,
    pub speaker: Speaker,
    pub es: String,
    pub ru: Option<String>,
    pub partial: bool,
    pub at: Instant,
}

#[derive(Clone, Default)]
pub struct Hint {
    pub question: String,
    pub es: String,
    pub ru: String,
    pub raw: String,
    /// The answer is not from the user's documents: the model produced it from its own knowledge.
    pub own: bool,
    pub done: bool,
    pub sources: Vec<String>,
    pub started: Option<Instant>,
}

impl Hint {
    /// Parses streamed text in the format `SRC: DOCS|OWN / Q: … / ES: … / RU: …`.
    pub fn apply_raw(&mut self, raw: &str) {
        self.raw = raw.to_string();
        const PREFIXES: [&str; 4] = ["Q:", "ES:", "RU:", "SRC:"];
        let mut fields: [String; 4] = Default::default();
        let mut cur = None;
        for line in raw.lines().map(str::trim) {
            let mut rest = line;
            if let Some((i, r)) = PREFIXES.iter().enumerate().find_map(|(i, p)| Some((i, line.strip_prefix(p)?))) {
                cur = Some(i);
                rest = r;
            }
            // Text before the first prefix is ignored (it stays in `raw`).
            let Some(i) = cur else { continue };
            if !fields[i].is_empty() {
                fields[i].push(' ');
            }
            fields[i].push_str(rest.trim());
        }
        let [question, es, ru, src] = fields;
        (self.question, self.es, self.ru) = (question, es, ru);
        self.own = src.trim().to_uppercase().starts_with("OWN");
    }

    /// The model answered `NONE`: there was no question to answer.
    pub fn is_none(&self) -> bool {
        self.es.is_empty() && self.raw.trim_start().to_uppercase().starts_with("NONE")
    }

    /// The parsed text has something worth showing.
    pub fn has_content(&self) -> bool {
        !self.es.is_empty() || !self.question.is_empty()
    }
}

/// What the hint machinery is doing; the hint panel tells the user so they know whether to wait.
#[derive(Clone, Default, PartialEq, Debug)]
pub enum HintStatus {
    #[default]
    Idle,
    /// The other person is speaking (an automatic hint will follow the turn).
    Listening,
    /// The turn ended; waiting a moment in case they go on, then asking the model.
    Checking,
    /// The model is writing the hint.
    Writing,
    /// The model found no question in the last phrases.
    NoQuestion,
    Error(String),
}

#[derive(Clone, Default)]
pub struct DocsReport {
    pub files: Vec<crate::docs::FileReport>,
    pub chunks: usize,
}

/// How many 100 ms frames the level history keeps (~15 s).
pub const METER_FRAMES: usize = 150;

/// Live audio statistics of one source, shown in the Audio window.
#[derive(Clone, Default)]
pub struct AudioMeter {
    /// (RMS, frame goes out) per 100 ms frame, oldest first.
    pub levels: VecDeque<(f32, bool)>,
    /// Gate threshold; the Audio window can change it while capturing.
    pub threshold: crate::audio::Threshold,
    pub frames_total: u64,
    pub frames_passed: u64,
    pub bytes_sent: u64,
    pub last_sent: Option<Instant>,
    pub last_server_msg: Option<Instant>,
    /// WebSocket to OpenAI is up.
    pub connected: bool,
}

impl AudioMeter {
    pub fn record(&mut self, rms: f64, sent: bool) {
        if self.levels.len() >= METER_FRAMES {
            self.levels.pop_front();
        }
        self.levels.push_back((rms as f32, sent));
        self.frames_total += 1;
        self.frames_passed += u64::from(sent);
    }

    /// Clears the counters for a new session; the threshold stays.
    pub fn reset(&mut self) {
        *self = Self { threshold: self.threshold.clone(), ..Default::default() };
    }
}

#[derive(Clone, Default)]
pub struct AppState {
    /// Audio statistics: [speakers, microphone].
    pub meters: [AudioMeter; 2],
    /// Cost of the session that was stopped last, and whether its window is open.
    pub cost_report: Option<crate::usage::Summary>,
    pub cost_open: bool,
    /// Capturing for the Audio window without talking to OpenAI.
    pub monitoring: bool,
    pub segments: Vec<Segment>,
    pub listening: bool,
    pub listening_since: Option<Instant>,
    pub status: String,
    /// General error (dismissible).
    pub error: Option<String>,
    /// Connection problem; cleared on reconnect and on Stop, never touches `error`.
    pub conn_error: Option<String>,
    /// Parse results of the selected folder.
    pub docs_report: Option<DocsReport>,
    /// Log file, when stderr is redirected to one.
    pub log_path: Option<std::path::PathBuf>,
    /// Hint currently shown to the user.
    pub hint: Option<Hint>,
    /// What the hint machinery is doing right now (shown above the hint).
    pub hint_status: HintStatus,
    /// Document folders (subfolders of docs_dir) and the selected one.
    pub topics: Vec<String>,
    pub topic: Option<String>,
    pub topic_loading: bool,
}

#[derive(Clone)]
pub struct Shared {
    inner: Arc<Mutex<AppState>>,
    ctx: Option<eframe::egui::Context>,
}

impl Shared {
    pub fn new(ctx: Option<eframe::egui::Context>) -> Self {
        Self { inner: Arc::new(Mutex::new(AppState::default())), ctx }
    }

    pub fn update(&self, f: impl FnOnce(&mut AppState)) {
        f(&mut self.inner.lock().unwrap());
        if let Some(c) = &self.ctx {
            c.request_repaint();
        }
    }

    pub fn read<R>(&self, f: impl FnOnce(&AppState) -> R) -> R {
        f(&self.inner.lock().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::Hint;

    #[test]
    fn unprefixed_text_is_kept_in_raw() {
        let mut h = Hint::default();
        h.apply_raw("Tengo cinco años.");
        assert!(h.es.is_empty() && h.raw == "Tengo cinco años.");
    }

    #[test]
    fn source_marker_is_parsed() {
        let mut h = Hint::default();
        h.apply_raw("SRC: OWN\nQ: Что такое тавтология?\nES: Es una fórmula siempre verdadera.\nRU: Это всегда истинно.");
        assert!(h.own);
        assert_eq!((h.question.as_str(), h.es.as_str()), ("Что такое тавтология?", "Es una fórmula siempre verdadera."));
        h.apply_raw("SRC: DOCS\nQ: x\nES: y\nRU: z");
        assert!(!h.own);
        // No marker (old format or still streaming): treated as from the documents.
        h.apply_raw("Q: x\nES: y");
        assert!(!h.own);
        h.apply_raw("SRC: O");
        assert!(!h.own);
    }

    #[test]
    fn none_answer_is_detected() {
        let mut h = Hint::default();
        h.apply_raw("NONE");
        assert!(h.is_none() && !h.has_content());
        h.apply_raw("SRC: OWN\nQ: x\nES: hola\nRU: привет");
        assert!(!h.is_none() && h.has_content());
    }

    #[test]
    fn parses_streaming_hint() {
        let mut h = Hint::default();
        h.apply_raw("Q: Сколько опыта?\nES: Tengo cinco años.\nRU: У меня пять лет.");
        assert_eq!((h.question.as_str(), h.es.as_str(), h.ru.as_str()), ("Сколько опыта?", "Tengo cinco años.", "У меня пять лет."));
        h.apply_raw("Q: How much\nES: Ten");
        assert_eq!((h.question.as_str(), h.es.as_str(), h.ru.as_str()), ("How much", "Ten", ""));
    }
}
