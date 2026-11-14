use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Who is talking: the other side (speaker output) or the user (microphone).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speaker {
    Them,
    Me,
}

impl Speaker {
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
    pub en: Option<String>,
    pub partial: bool,
    pub at: Instant,
}

#[derive(Clone, Default)]
pub struct Hint {
    pub question: String,
    pub es: String,
    pub en: String,
    pub raw: String,
    pub done: bool,
    pub sources: Vec<String>,
    pub started: Option<Instant>,
}

impl Hint {
    /// Parses streamed text in the format `Q: … / ES: … / EN: …`.
    pub fn apply_raw(&mut self, raw: &str) {
        self.raw = raw.to_string();
        const PREFIXES: [&str; 3] = ["Q:", "ES:", "EN:"];
        let mut fields: [String; 3] = Default::default();
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
        let [question, es, en] = fields;
        (self.question, self.es, self.en) = (question, es, en);
    }
}

#[derive(Clone, Default)]
pub struct DocsReport {
    pub files: Vec<crate::docs::FileReport>,
    pub chunks: usize,
}

#[derive(Clone, Default)]
pub struct AppState {
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
    /// Hint prepared in the background (shown on button press).
    pub pending: Option<Hint>,
    pub pending_shown: bool,
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
    fn parses_streaming_hint() {
        let mut h = Hint::default();
        h.apply_raw("Q: How much experience?\nES: Tengo cinco años.\nEN: I have five years.");
        assert_eq!((h.question.as_str(), h.es.as_str(), h.en.as_str()), ("How much experience?", "Tengo cinco años.", "I have five years."));
        h.apply_raw("Q: How much\nES: Ten");
        assert_eq!((h.question.as_str(), h.es.as_str(), h.en.as_str()), ("How much", "Ten", ""));
    }
}
