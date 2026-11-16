//! Cost accounting of a Listen session: every paid request (transcription events, translations,
//! hint streams including the hedged ones that lost) is recorded with its tokens and run time.
//! Rows go to `usage.csv` as they happen; at Stop they are condensed into a [`Summary`].
//! Prices are approximate: built-in defaults, overridable in `[prices]` of `config.toml`.

use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Translate,
    Hint,
    TranscribeThem,
    TranscribeMe,
}

impl Purpose {
    fn label(self) -> &'static str {
        match self {
            Purpose::Translate => "translate",
            Purpose::Hint => "hint",
            Purpose::TranscribeThem => "transcribe-them",
            Purpose::TranscribeMe => "transcribe-me",
        }
    }

    fn group(self) -> &'static str {
        match self {
            Purpose::TranscribeThem | Purpose::TranscribeMe => "transcribe",
            other => other.label(),
        }
    }

    fn is_transcribe(self) -> bool {
        self.group() == "transcribe"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The answer was used.
    Won,
    /// A hedged request that another model beat: its input tokens are usually billed anyway.
    Lost,
    /// Aborted (hint cancelled, session stopped).
    Cancelled,
    Error,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Won => "won",
            Outcome::Lost => "lost",
            Outcome::Cancelled => "cancelled",
            Outcome::Error => "error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Call {
    pub purpose: Purpose,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub audio_secs: f64,
    /// How long the request ran.
    pub duration: Duration,
    pub outcome: Outcome,
    /// Token counts are our guess (the provider did not report usage).
    pub estimated: bool,
}

/// USD per million tokens, and per minute of audio (used when no token counts are known).
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
pub struct Price {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub per_minute: f64,
}

/// Prices checked on 2026-11-14: OpenAI's pricing page for transcription, and the per-provider
/// prices that `https://router.huggingface.co/v1/models` lists for the HF router (which passes
/// provider rates through without markup). Providers change them: override in
/// `[prices."<model>"]` of config.toml when they differ.
const DEFAULT_PRICES: &[(&str, Price)] = &[
    ("gpt-4o-transcribe", Price { input: 2.5, output: 10.0, per_minute: 0.006 }),
    ("gpt-4o-mini-transcribe", Price { input: 1.25, output: 5.0, per_minute: 0.003 }),
    ("openai/gpt-oss-20b:groq", Price { input: 0.10, output: 0.50, per_minute: 0.0 }),
    ("Qwen/Qwen3-235B-A22B-Instruct-2507:parasail", Price { input: 0.14, output: 0.80, per_minute: 0.0 }),
    ("openai/gpt-oss-120b:groq", Price { input: 0.15, output: 0.75, per_minute: 0.0 }),
    ("openai/gpt-oss-120b:cerebras", Price { input: 0.35, output: 0.75, per_minute: 0.0 }),
];

#[derive(Debug, Clone)]
pub struct Prices(HashMap<String, Price>);

impl Prices {
    pub fn new(overrides: &HashMap<String, Price>) -> Self {
        let mut m: HashMap<String, Price> = DEFAULT_PRICES.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        m.extend(overrides.iter().map(|(k, v)| (k.clone(), *v)));
        Self(m)
    }

    pub fn get(&self, model: &str) -> Option<Price> {
        self.0.get(model).copied()
    }

    fn cost(&self, c: &Call) -> Option<f64> {
        let p = self.get(&c.model)?;
        if c.purpose.is_transcribe() && c.input_tokens == 0 && c.output_tokens == 0 {
            return (p.per_minute > 0.0).then(|| c.audio_secs / 60.0 * p.per_minute);
        }
        Some((c.input_tokens as f64 * p.input + c.output_tokens as f64 * p.output) / 1e6)
    }
}

/// Token counts from a chat-completions `usage` object (also Groq's `x_groq.usage`).
pub fn parse_chat_usage(v: &Value) -> Option<(u64, u64)> {
    [&v["usage"], &v["x_groq"]["usage"]].into_iter().find_map(|u| Some((u["prompt_tokens"].as_u64()?, u["completion_tokens"].as_u64().unwrap_or(0))))
}

/// `usage` of a Realtime `input_audio_transcription.completed` event:
/// `(input tokens, output tokens, seconds)`; tokens or seconds, depending on the model.
pub fn parse_transcribe_usage(v: &Value) -> Option<(u64, u64, f64)> {
    let u = v.get("usage").filter(|u| u.is_object())?;
    Some((u["input_tokens"].as_u64().unwrap_or(0), u["output_tokens"].as_u64().unwrap_or(0), u["seconds"].as_f64().unwrap_or(0.0)))
}

/// Rough token count of a text (~4 characters per token) for providers that report no usage.
pub fn estimate_tokens(chars: usize) -> u64 {
    chars.div_ceil(4) as u64
}

struct Inner {
    start: Instant,
    calls: Vec<(Duration, Call)>,
    csv: Option<BufWriter<File>>,
    /// `events.jsonl`: one JSON object per timing/service event of the session.
    events: Option<BufWriter<File>>,
}

pub struct UsageMeter {
    prices: Prices,
    inner: Mutex<Inner>,
}

impl UsageMeter {
    pub fn new(prices: Prices) -> Self {
        Self { prices, inner: Mutex::new(Inner { start: Instant::now(), calls: Vec::new(), csv: None, events: None }) }
    }

    /// Starts a session: clears the records and opens `<dir>/usage.csv`. A failure to open the
    /// file is not fatal: the summary still works.
    pub fn begin(&self, dir: Option<&Path>) {
        let mut g = self.inner.lock().unwrap();
        g.start = Instant::now();
        g.calls.clear();
        g.csv = dir.and_then(|d| File::create(d.join("usage.csv")).ok()).map(BufWriter::new);
        g.events = dir.and_then(|d| File::create(d.join("events.jsonl")).ok()).map(BufWriter::new);
        if let Some(w) = &mut g.csv {
            let _ = writeln!(w, "t_sec,purpose,model,input_tokens,output_tokens,audio_sec,duration_sec,outcome,estimated,cost_usd");
            let _ = w.flush();
        }
    }

    /// Appends `{"t": seconds since the session start, "ev": name, ...fields}` to `events.jsonl`.
    /// Does nothing outside a recorded session.
    pub fn event(&self, ev: &str, fields: Value) {
        let mut g = self.inner.lock().unwrap();
        let t = (g.start.elapsed().as_secs_f64() * 1000.0).round() / 1000.0;
        let Some(w) = &mut g.events else { return };
        let mut obj = serde_json::Map::new();
        obj.insert("t".into(), t.into());
        obj.insert("ev".into(), ev.into());
        if let Value::Object(f) = fields {
            obj.extend(f);
        }
        let _ = writeln!(w, "{}", Value::Object(obj));
        let _ = w.flush();
    }

    pub fn record(&self, call: Call) {
        let cost = self.prices.cost(&call);
        let mut g = self.inner.lock().unwrap();
        let at = g.start.elapsed();
        if let Some(w) = &mut g.csv {
            let _ = writeln!(
                w,
                "{:.1},{},{},{},{},{:.1},{:.2},{},{},{}",
                at.as_secs_f64(),
                call.purpose.label(),
                call.model,
                call.input_tokens,
                call.output_tokens,
                call.audio_secs,
                call.duration.as_secs_f64(),
                call.outcome.label(),
                call.estimated,
                cost.map(|c| format!("{c:.6}")).unwrap_or_default()
            );
            let _ = w.flush();
        }
        g.calls.push((at, call));
    }

    /// Condenses the session. `audio_sent` is the seconds of audio sent to the transcriber for
    /// [speakers, microphone]; it stands in for token counts when OpenAI reported none.
    pub fn summary(&self, transcribe_model: &str, audio_sent: [f64; 2]) -> Summary {
        let g = self.inner.lock().unwrap();
        let mut rows: Vec<(Call, bool)> = g.calls.iter().map(|(_, c)| (c.clone(), false)).collect();
        for (i, purpose) in [Purpose::TranscribeThem, Purpose::TranscribeMe].into_iter().enumerate() {
            let reported = g.calls.iter().any(|(_, c)| c.purpose == purpose && (c.input_tokens > 0 || c.output_tokens > 0));
            if audio_sent[i] > 0.0 && !reported {
                let call = Call { purpose, model: transcribe_model.into(), input_tokens: 0, output_tokens: 0, audio_secs: audio_sent[i], duration: Duration::ZERO, outcome: Outcome::Won, estimated: true };
                rows.push((call, true));
            }
        }
        let mut lines: BTreeMap<(&'static str, String), Line> = BTreeMap::new();
        for (c, synthetic) in &rows {
            let l = lines.entry((c.purpose.group(), c.model.clone())).or_insert_with(|| Line {
                what: c.purpose.group(),
                model: c.model.clone(),
                price: self.prices.get(&c.model),
                requests: 0,
                input: 0,
                output: 0,
                audio_secs: 0.0,
                busy: Duration::ZERO,
                cost: self.prices.get(&c.model).map(|_| 0.0),
                estimated: false,
            });
            l.requests += usize::from(!synthetic);
            l.input += c.input_tokens;
            l.output += c.output_tokens;
            l.audio_secs += c.audio_secs;
            l.busy += c.duration;
            l.estimated |= c.estimated;
            l.cost = l.cost.zip(self.prices.cost(c)).map(|(a, b)| a + b);
        }
        let sent_total: f64 = audio_sent.iter().sum();
        let mut lines: Vec<Line> = lines.into_values().collect();
        for l in lines.iter_mut().filter(|l| l.what == "transcribe") {
            // The connection works for as long as audio is flowing.
            l.audio_secs = l.audio_secs.max(sent_total);
            l.busy = Duration::from_secs_f64(l.audio_secs);
        }
        Summary {
            elapsed: g.start.elapsed(),
            total: lines.iter().filter_map(|l| l.cost).sum(),
            unpriced: lines.iter().any(|l| l.cost.is_none()),
            estimated: lines.iter().any(|l| l.estimated),
            lines,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Line {
    pub what: &'static str,
    pub model: String,
    pub price: Option<Price>,
    pub requests: usize,
    pub input: u64,
    pub output: u64,
    pub audio_secs: f64,
    pub busy: Duration,
    /// None: no price known for the model.
    pub cost: Option<f64>,
    pub estimated: bool,
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub elapsed: Duration,
    pub lines: Vec<Line>,
    pub total: f64,
    /// Some model has no price, so the total is incomplete.
    pub unpriced: bool,
    /// Some token counts are guesses.
    pub estimated: bool,
}

pub fn money(v: f64) -> String {
    if v > 0.0 && v < 0.0001 { "<$0.0001".into() } else { format!("${v:.4}") }
}

pub fn clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

impl Summary {
    /// One-line total for the log and the window title.
    pub fn headline(&self) -> String {
        let plus = if self.unpriced { "+" } else { "" };
        format!("≈ {}{plus} in {}", money(self.total), clock(self.elapsed))
    }

    /// The report written to `cost.txt`.
    pub fn to_text(&self) -> String {
        let mut s = format!("Session cost {}\n\n", self.headline());
        let _ = writeln!(s, "{:<11} {:<30} {:>5} {:>9} {:>8} {:>7} {:>7} {:>10}", "what", "model", "calls", "tok in", "tok out", "audio", "busy", "cost");
        for l in &self.lines {
            let cost = l.cost.map(money).unwrap_or_else(|| "no price".into());
            let mark = if l.estimated { "~" } else { "" };
            let _ = writeln!(
                s,
                "{:<11} {:<30} {:>5} {:>9} {:>8} {:>7} {:>7} {:>10}",
                l.what,
                l.model,
                l.requests,
                format!("{mark}{}", l.input),
                format!("{mark}{}", l.output),
                if l.audio_secs > 0.0 { clock(Duration::from_secs_f64(l.audio_secs)) } else { "-".into() },
                clock(l.busy),
                cost
            );
        }
        let _ = writeln!(s, "\nTotal: {}", money(self.total));
        s.push_str("\nPrices used (USD per 1M tokens, in / out):\n");
        for l in &self.lines {
            match l.price {
                Some(p) => {
                    let _ = writeln!(s, "  {}: {} / {}", l.model, p.input, p.output);
                }
                None => {
                    let _ = writeln!(s, "  {}: unknown - add [prices.\"{}\"] to config.toml", l.model, l.model);
                }
            }
        }
        if self.estimated {
            s.push_str("\n~ = estimated: the provider reported no usage (hint requests that lost the hedge race and\ntranscription without token counts are guessed from text length and audio time).\n");
        }
        if self.unpriced {
            s.push_str("\nThe total excludes models without a price.\n");
        }
        s.push_str("\nPrices are approximate list prices; the provider's dashboard is authoritative.\nPer-request details: usage.csv\n");
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(purpose: Purpose, model: &str, i: u64, o: u64, outcome: Outcome) -> Call {
        Call { purpose, model: model.into(), input_tokens: i, output_tokens: o, audio_secs: 0.0, duration: Duration::from_secs(2), outcome, estimated: false }
    }

    #[test]
    fn summary_prices_and_groups_calls() {
        let m = UsageMeter::new(Prices::new(&HashMap::new()));
        m.begin(None);
        m.record(call(Purpose::Hint, "openai/gpt-oss-120b:groq", 1_000_000, 500_000, Outcome::Won));
        m.record(call(Purpose::Hint, "openai/gpt-oss-120b:groq", 1_000_000, 0, Outcome::Lost));
        m.record(call(Purpose::Translate, "unknown-model", 10, 10, Outcome::Won));
        let s = m.summary("gpt-4o-transcribe", [0.0, 0.0]);
        let hint = s.lines.iter().find(|l| l.what == "hint").unwrap();
        assert_eq!((hint.requests, hint.input), (2, 2_000_000));
        // 2M in * 0.15 + 0.5M out * 0.75
        assert!((hint.cost.unwrap() - 0.675).abs() < 1e-9);
        assert!(s.unpriced && (s.total - 0.675).abs() < 1e-9);
        assert!(s.to_text().contains("no price"));
    }

    #[test]
    fn config_overrides_default_price() {
        let o = HashMap::from([("openai/gpt-oss-120b:groq".to_string(), Price { input: 1.0, output: 2.0, per_minute: 0.0 })]);
        assert_eq!(Prices::new(&o).get("openai/gpt-oss-120b:groq").unwrap().input, 1.0);
    }

    #[test]
    fn transcription_falls_back_to_audio_minutes() {
        let m = UsageMeter::new(Prices::new(&HashMap::new()));
        m.begin(None);
        let s = m.summary("gpt-4o-transcribe", [600.0, 0.0]);
        let l = &s.lines[0];
        assert!(l.estimated && (l.cost.unwrap() - 0.06).abs() < 1e-9, "{l:?}");
        // With reported tokens the audio time is not billed twice.
        m.record(call(Purpose::TranscribeThem, "gpt-4o-transcribe", 1_000_000, 0, Outcome::Won));
        let s = m.summary("gpt-4o-transcribe", [600.0, 0.0]);
        assert!((s.total - 2.5).abs() < 1e-9 && !s.estimated);
    }

    #[test]
    fn parses_usage_shapes() {
        assert_eq!(parse_chat_usage(&json!({ "usage": { "prompt_tokens": 12, "completion_tokens": 5 } })), Some((12, 5)));
        assert_eq!(parse_chat_usage(&json!({ "x_groq": { "usage": { "prompt_tokens": 3, "completion_tokens": 4 } } })), Some((3, 4)));
        assert_eq!(parse_chat_usage(&json!({ "usage": null, "choices": [] })), None);
        let tokens = json!({ "usage": { "type": "tokens", "input_tokens": 40, "output_tokens": 9 } });
        assert_eq!(parse_transcribe_usage(&tokens), Some((40, 9, 0.0)));
        let secs = json!({ "usage": { "type": "duration", "seconds": 2.5 } });
        assert_eq!(parse_transcribe_usage(&secs), Some((0, 0, 2.5)));
        assert_eq!(parse_transcribe_usage(&json!({})), None);
    }

    #[test]
    fn csv_row_is_written_as_it_happens() {
        let d = std::env::temp_dir().join(format!("assistant-test-usage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let m = UsageMeter::new(Prices::new(&HashMap::new()));
        m.begin(Some(&d));
        m.record(call(Purpose::Translate, "openai/gpt-oss-20b:groq", 100, 20, Outcome::Won));
        let text = std::fs::read_to_string(d.join("usage.csv")).unwrap();
        assert!(text.starts_with("t_sec,purpose") && text.contains(",translate,openai/gpt-oss-20b:groq,100,20,"), "{text}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn events_are_json_lines_with_time_and_name() {
        let d = std::env::temp_dir().join(format!("assistant-test-events-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let m = UsageMeter::new(Prices::new(&HashMap::new()));
        m.event("ignored", serde_json::json!({}));
        m.begin(Some(&d));
        m.event("translate", serde_json::json!({ "id": "them:1", "ms": 420 }));
        m.event("conn_lost", serde_json::json!({}));
        let text = std::fs::read_to_string(d.join("events.jsonl")).unwrap();
        let rows: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(rows.len(), 2, "{text}");
        assert!(rows[0]["t"].is_number() && rows[0]["ev"] == "translate" && rows[0]["ms"] == 420 && rows[0]["id"] == "them:1");
        assert_eq!(rows[1]["ev"], "conn_lost");
        let _ = std::fs::remove_dir_all(d);
    }
}
