use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Clone)]
pub struct Segment {
    pub item_id: String,
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
        let (mut q, mut es, mut en) = (String::new(), String::new(), String::new());
        let mut cur = 0;
        for line in raw.lines() {
            let line = line.trim();
            let (field, rest) = if let Some(r) = line.strip_prefix("Q:") {
                (1, r)
            } else if let Some(r) = line.strip_prefix("ES:") {
                (2, r)
            } else if let Some(r) = line.strip_prefix("EN:") {
                (3, r)
            } else {
                (cur, line)
            };
            cur = field;
            let target = match field {
                1 => &mut q,
                2 => &mut es,
                3 => &mut en,
                _ => continue,
            };
            if !target.is_empty() {
                target.push(' ');
            }
            target.push_str(rest.trim());
        }
        self.question = q;
        self.es = es;
        self.en = en;
    }
}

#[derive(Default)]
pub struct AppState {
    pub segments: Vec<Segment>,
    pub listening: bool,
    pub listening_since: Option<Instant>,
    pub status: String,
    pub error: Option<String>,
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
    fn parses_streaming_hint() {
        let mut h = Hint::default();
        h.apply_raw("Q: How much experience?\nES: Tengo cinco años.\nEN: I have five years.");
        assert_eq!((h.question.as_str(), h.es.as_str(), h.en.as_str()), ("How much experience?", "Tengo cinco años.", "I have five years."));
        h.apply_raw("Q: How much\nES: Ten");
        assert_eq!((h.question.as_str(), h.es.as_str(), h.en.as_str()), ("How much", "Ten", ""));
    }
}
