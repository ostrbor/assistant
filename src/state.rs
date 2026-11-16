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
    /// The hint replaced an earlier one for the same question because Them added something.
    pub updated: bool,
}

/// What the model decided about an automatic request, from the first line of its answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// Nothing new: the hint on screen still applies (or there is nothing to answer).
    Keep,
    /// The same question as the hint on screen, but the answer changes with what Them added.
    Update,
    /// A different question: the hint on screen moves to the earlier ones.
    New,
}

impl Verdict {
    /// Reads the marker on the first line of a (possibly still streaming) answer; `None` while it
    /// is not known yet. An answer that starts with the old `SRC:`/`Q:`/`ES:` lines counts as `New`.
    pub fn parse(raw: &str) -> Option<Verdict> {
        let first = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
        let word: String = first.chars().take_while(|c| c.is_alphabetic()).collect::<String>().to_uppercase();
        match word.as_str() {
            "KEEP" | "NONE" => Some(Verdict::Keep),
            "UPDATE" => Some(Verdict::Update),
            "NEW" | "SRC" | "Q" | "ES" => Some(Verdict::New),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Verdict::Keep => "KEEP",
            Verdict::Update => "UPDATE",
            Verdict::New => "NEW",
        }
    }
}

/// How many replaced hints are kept under the current one.
pub const PREV_HINTS: usize = 3;

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
    /// The other person is speaking (an automatic hint is asked after each of their phrases).
    Listening,
    /// A phrase ended; the model is deciding whether the hint needs to change.
    Checking,
    /// The model is writing the hint.
    Writing,
    /// The model found nothing new in the last phrases: the hint on screen still applies.
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
    /// Hints that were replaced by a different question, newest first.
    pub hint_prev: Vec<Hint>,
    /// The hint request that owns the screen (requests are numbered in the order they start).
    pub hint_owner: u64,
    /// The newest request that answered KEEP: older requests that come late must not replace the screen.
    pub hint_kept: u64,
    /// What the hint machinery is doing right now (shown above the hint).
    pub hint_status: HintStatus,
    /// Document folders (subfolders of docs_dir) and the selected one.
    pub topics: Vec<String>,
    pub topic: Option<String>,
    pub topic_loading: bool,
}

impl AppState {
    /// Empties the hint panel (the dialog was cleared or the documents changed).
    pub fn clear_hints(&mut self) {
        self.hint = None;
        self.hint_prev.clear();
        self.hint_status = HintStatus::Idle;
    }
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
    use super::{Hint, Verdict};

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
    fn verdict_is_read_from_the_first_line() {
        assert_eq!(Verdict::parse("KEEP"), Some(Verdict::Keep));
        assert_eq!(Verdict::parse("\n keep."), Some(Verdict::Keep));
        assert_eq!(Verdict::parse("NONE"), Some(Verdict::Keep));
        assert_eq!(Verdict::parse("UPDATE\nSRC: DOCS\nQ: x"), Some(Verdict::Update));
        assert_eq!(Verdict::parse("NEW\nSRC: OWN"), Some(Verdict::New));
        // Old format without a marker.
        assert_eq!(Verdict::parse("SRC: OWN\nQ: x"), Some(Verdict::New));
        // Not known yet while the first word is still arriving, or when the model ignores the format.
        assert_eq!(Verdict::parse(""), None);
        assert_eq!(Verdict::parse("UPD"), None);
        assert_eq!(Verdict::parse("Tengo cinco años"), None);
    }

    #[test]
    fn marker_line_does_not_leak_into_the_hint() {
        let mut h = Hint::default();
        h.apply_raw("UPDATE\nSRC: OWN\nQ: Что такое тавтология?\nES: Es siempre verdadera.\nRU: Всегда истинна.");
        assert!(h.own && h.has_content());
        assert_eq!((h.question.as_str(), h.es.as_str()), ("Что такое тавтология?", "Es siempre verdadera."));
        h.apply_raw("KEEP");
        assert!(!h.has_content());
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
