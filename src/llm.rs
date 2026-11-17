use crate::config;
use crate::usage::{self, Call, Outcome, Purpose, UsageMeter};
use anyhow::{Result, anyhow, bail};
use futures::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Whole non-streaming request (translation).
const TRANSLATE_TIMEOUT: Duration = Duration::from_secs(30);
/// Pauses before the 1st and 2nd retry of a translation.
const RETRY_PAUSES: [Duration; 2] = [Duration::from_millis(500), Duration::from_millis(1500)];
/// No model has produced a first token within this time.
const FIRST_TOKEN_TIMEOUT: Duration = Duration::from_secs(15);
/// The winning stream went quiet for this long.
const IDLE_TIMEOUT: Duration = Duration::from_secs(20);
/// `winner` value while no stream has started answering.
const NO_WINNER: usize = usize::MAX;

/// A rate limit or a server-side failure may pass in a moment; a client error will not.
fn is_retryable(status: u16) -> bool {
    status == 429 || status >= 500
}

/// What the translator is told. The topic keeps terms right ("preimagen" is "прообраз" in algebra).
pub fn translate_prompt(topic: Option<&str>) -> String {
    let mut p = String::from("Translate the Spanish utterance from a video call into natural Russian. Output only the translation of the last utterance, nothing else.");
    if let Some(t) = topic.map(str::trim).filter(|t| !t.is_empty()) {
        p.push_str(&format!(" Topic of the call: {t}. Use the standard Russian terminology of this field."));
    }
    p
}

pub struct Llm {
    http: reqwest::Client,
    /// Full chat-completions endpoint.
    url: String,
    key: Option<String>,
    usage: Arc<UsageMeter>,
}

/// The finished answer of a hedged request and the model that produced it.
pub struct Streamed {
    pub text: String,
    pub model: String,
}

enum StreamEv {
    Delta(String),
    Done,
    Err(String),
}

impl Llm {
    pub fn new(cfg: &config::Llm, usage: Arc<UsageMeter>) -> Self {
        Self {
            http: reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT).build().expect("HTTP client"),
            url: format!("{}/chat/completions", cfg.base_url.trim_end_matches('/')),
            key: cfg.key(),
            usage,
        }
    }

    fn key(&self) -> Result<&str> {
        self.key.as_deref().ok_or_else(|| anyhow!("LLM token is not set (set [llm] api_key in config.toml)"))
    }

    fn body(model: &str, messages: &Value, stream: bool, max_tokens: u32) -> Value {
        let mut b = json!({ "model": model, "messages": messages, "stream": stream, "max_tokens": max_tokens });
        if stream {
            // The last chunk then carries the token counts (needed for the cost report).
            b["stream_options"] = json!({ "include_usage": true });
        }
        if model.contains("gpt-oss") {
            b["reasoning_effort"] = json!("low");
        }
        b
    }

    /// Translates one utterance. A rate limit (429), a server error (5xx) or a network failure is
    /// retried after the pauses in `RETRY_PAUSES`; anything else (bad key, bad request) is not.
    /// Returns the text and how many retries it took.
    pub async fn translate(&self, model: &str, topic: Option<&str>, context: &[String], es: &str) -> Result<(String, usize)> {
        let key = self.key()?;
        let mut user = String::new();
        if !context.is_empty() {
            user.push_str("Previous utterances (context only):\n");
            for c in context {
                user.push_str(&format!("- {c}\n"));
            }
        }
        user.push_str(&format!("Utterance to translate:\n{es}"));
        let messages = json!([
            { "role": "system", "content": translate_prompt(topic) },
            { "role": "user", "content": user }
        ]);
        let started = Instant::now();
        let mut retries = 0;
        let out = loop {
            match self.translate_request(key, model, &messages).await {
                Err((_, true)) if retries < RETRY_PAUSES.len() => {
                    tokio::time::sleep(RETRY_PAUSES[retries]).await;
                    retries += 1;
                }
                Err((e, _)) if retries > 0 => break Err(anyhow!("{e} (after {} attempts)", retries + 1)),
                Err((e, _)) => break Err(e),
                Ok(v) => break Ok(v),
            }
        };
        let (outcome, tokens, estimated) = match &out {
            Ok((_, Some(t))) => (Outcome::Won, *t, false),
            Ok((text, None)) => (Outcome::Won, (usage::estimate_tokens(messages.to_string().len()), usage::estimate_tokens(text.len())), true),
            // A failed request is not billed.
            Err(_) => (Outcome::Error, (0, 0), false),
        };
        self.usage.record(Call {
            purpose: Purpose::Translate,
            model: model.to_string(),
            input_tokens: tokens.0,
            output_tokens: tokens.1,
            audio_secs: 0.0,
            duration: started.elapsed(),
            outcome,
            estimated,
        });
        out.map(|(text, _)| (text, retries))
    }

    /// The error comes with a flag: is it worth trying again.
    async fn translate_request(&self, key: &str, model: &str, messages: &Value) -> std::result::Result<(String, Option<(u64, u64)>), (anyhow::Error, bool)> {
        let res = self
            .http
            .post(&self.url)
            .bearer_auth(key)
            .timeout(TRANSLATE_TIMEOUT)
            .json(&Self::body(model, messages, false, 600))
            .send()
            .await
            .map_err(|e| (anyhow::Error::from(e), true))?;
        let status = res.status();
        let v: Value = res.json().await.map_err(|e| (anyhow::Error::from(e), true))?;
        if !status.is_success() {
            return Err((anyhow!("LLM {status}: {}", v["error"]["message"].as_str().unwrap_or("error")), is_retryable(status.as_u16())));
        }
        let text = v["choices"][0]["message"]["content"].as_str().unwrap_or_default().trim().to_string();
        Ok((text, usage::parse_chat_usage(&v)))
    }

    /// Streams a hint from several models/providers at once and keeps whichever starts answering
    /// first. Every request, the losing ones too, is recorded in the usage meter.
    pub async fn hedged_stream(&self, models: &[String], messages: Value, mut on_update: impl FnMut(&str)) -> Result<Streamed> {
        let key = self.key()?;
        let (tx, mut rx) = mpsc::unbounded_channel::<(usize, StreamEv)>();
        let winner_idx = Arc::new(AtomicUsize::new(NO_WINNER));
        let input_chars = messages.to_string().len();
        let handles = AbortAll(models
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let (http, url, key, tx) = (self.http.clone(), self.url.clone(), key.to_string(), tx.clone());
                let body = Self::body(m, &messages, true, 800);
                let mut guard = StreamGuard {
                    usage: self.usage.clone(),
                    model: m.clone(),
                    idx: i,
                    winner: winner_idx.clone(),
                    started: Instant::now(),
                    first_token: None,
                    input_chars,
                    out_chars: 0,
                    tokens: None,
                    outcome: None,
                };
                tokio::spawn(async move {
                    if let Err(e) = stream_one(&http, &url, &key, body, i, &tx, &mut guard).await {
                        guard.end(Outcome::Error);
                        let _ = tx.send((i, StreamEv::Err(e.to_string())));
                    }
                })
            })
            .collect());
        drop(tx);

        let (mut winner, mut text, mut failed) = (None, String::new(), 0);
        let result = loop {
            let wait = if winner.is_none() { FIRST_TOKEN_TIMEOUT } else { IDLE_TIMEOUT };
            let Ok(next) = timeout(wait, rx.recv()).await else { break Err(anyhow!("LLM timed out")) };
            let Some((i, ev)) = next else { break Err(anyhow!("empty LLM response")) };
            match ev {
                StreamEv::Delta(d) => {
                    if winner.is_none() {
                        winner = Some(i);
                        winner_idx.store(i, Ordering::Relaxed);
                        for (j, h) in handles.0.iter().enumerate() {
                            if j != i {
                                h.abort();
                            }
                        }
                    }
                    if winner == Some(i) {
                        text.push_str(&d);
                        on_update(&text);
                    }
                }
                StreamEv::Done if winner == Some(i) => break Ok(Streamed { text: text.clone(), model: models[i].clone() }),
                StreamEv::Done => {}
                StreamEv::Err(e) => {
                    if winner == Some(i) {
                        break Err(anyhow!(e));
                    }
                    failed += 1;
                    if winner.is_none() && failed == models.len() {
                        break Err(anyhow!(e));
                    }
                }
            }
        };
        // `handles` aborts every request that is still running when it is dropped here, and also
        // when this future is dropped (cancelled) before it gets here.
        result
    }
}

/// Aborts its tasks when dropped. A dropped `JoinHandle` only detaches the task, so without this
/// the requests of a cancelled hint would keep streaming (and be paid for) to the end.
struct AbortAll(Vec<JoinHandle<()>>);

impl Drop for AbortAll {
    fn drop(&mut self) {
        for h in &self.0 {
            h.abort();
        }
    }
}

/// Records one hint request in the usage meter when its task ends, however it ends: finished,
/// failed, or aborted (the guard is dropped with the task's future).
struct StreamGuard {
    usage: Arc<UsageMeter>,
    model: String,
    idx: usize,
    winner: Arc<AtomicUsize>,
    started: Instant,
    /// Time from the request to the first token.
    first_token: Option<Duration>,
    input_chars: usize,
    out_chars: usize,
    /// Reported by the provider in the final chunk.
    tokens: Option<(u64, u64)>,
    /// Set when the stream ended by itself; otherwise the task was aborted.
    outcome: Option<Outcome>,
}

impl StreamGuard {
    /// The stream ended by itself (not aborted).
    fn end(&mut self, outcome: Outcome) {
        self.outcome = Some(outcome);
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        let outcome = self.outcome.unwrap_or_else(|| {
            let w = self.winner.load(Ordering::Relaxed);
            if w != NO_WINNER && w != self.idx { Outcome::Lost } else { Outcome::Cancelled }
        });
        let ((input_tokens, output_tokens), estimated) = match (self.tokens, outcome) {
            (Some(t), _) => (t, false),
            // A request that failed outright is not billed.
            (None, Outcome::Error) => ((0, 0), false),
            (None, _) => ((usage::estimate_tokens(self.input_chars), usage::estimate_tokens(self.out_chars)), true),
        };
        // Every request of the race, with how fast it started answering.
        self.usage.event(
            "hint_model",
            json!({
                "model": self.model,
                "outcome": outcome.label(),
                "first_token_ms": self.first_token.map(|d| d.as_millis() as u64),
                "total_ms": self.started.elapsed().as_millis() as u64,
                "out_chars": self.out_chars,
            }),
        );
        self.usage.record(Call {
            purpose: Purpose::Hint,
            model: std::mem::take(&mut self.model),
            input_tokens,
            output_tokens,
            audio_secs: 0.0,
            duration: self.started.elapsed(),
            outcome,
            estimated,
        });
    }
}

async fn stream_one(
    http: &reqwest::Client,
    url: &str,
    key: &str,
    body: Value,
    idx: usize,
    tx: &mpsc::UnboundedSender<(usize, StreamEv)>,
    guard: &mut StreamGuard,
) -> Result<()> {
    let res = http.post(url).bearer_auth(key).json(&body).send().await?;
    let status = res.status();
    if !status.is_success() {
        let t = res.text().await.unwrap_or_default();
        bail!("LLM {status}: {}", t.chars().take(200).collect::<String>());
    }
    let mut stream = res.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk?);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" {
                guard.end(Outcome::Won);
                let _ = tx.send((idx, StreamEv::Done));
                return Ok(());
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
            if let Some(t) = usage::parse_chat_usage(&v) {
                guard.tokens = Some(t);
            }
            if let Some(d) = v["choices"][0]["delta"]["content"].as_str()
                && !d.is_empty()
            {
                guard.first_token.get_or_insert_with(|| guard.started.elapsed());
                guard.out_chars += d.len();
                let _ = tx.send((idx, StreamEv::Delta(d.to_string())));
            }
        }
    }
    guard.end(Outcome::Won);
    let _ = tx.send((idx, StreamEv::Done));
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_rate_limits_and_server_errors_are_retried() {
        for s in [429, 500, 502, 503] {
            assert!(super::is_retryable(s), "{s}");
        }
        for s in [400, 401, 403, 404] {
            assert!(!super::is_retryable(s), "{s}");
        }
    }

    #[test]
    fn translator_is_told_the_topic() {
        assert!(!super::translate_prompt(None).contains("Topic"));
        assert!(!super::translate_prompt(Some("  ")).contains("Topic"));
        assert!(super::translate_prompt(Some("algebra")).contains("Topic of the call: algebra."));
    }

    use super::*;
    use crate::usage::Prices;

    fn guard(meter: &Arc<UsageMeter>, idx: usize, winner: usize) -> StreamGuard {
        StreamGuard {
            usage: meter.clone(),
            model: format!("m{idx}"),
            idx,
            winner: Arc::new(AtomicUsize::new(winner)),
            started: Instant::now(),
            first_token: None,
            input_chars: 400,
            out_chars: 40,
            tokens: None,
            outcome: None,
        }
    }

    #[tokio::test]
    async fn dropping_the_guard_stops_the_tasks() {
        struct Flag(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Flag(stopped.clone());
        let task = tokio::spawn(async move {
            let _flag = flag;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        drop(AbortAll(vec![task]));
        for _ in 0..50 {
            if stopped.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(stopped.load(Ordering::Relaxed), "the task was left running");
    }

    #[test]
    fn aborted_streams_are_recorded_as_estimates() {
        let meter = Arc::new(UsageMeter::new(Prices::new(&Default::default())));
        meter.begin(None);
        drop(guard(&meter, 1, 0)); // another model won
        drop(guard(&meter, 0, 0)); // the winner, cancelled
        let mut done = guard(&meter, 2, 2);
        done.tokens = Some((7, 3));
        done.end(Outcome::Won);
        drop(done);
        let s = meter.summary(["t"; 2], [0.0, 0.0]);
        let by = |m: &str| s.lines.iter().find(|l| l.model == m).unwrap().clone();
        assert!(by("m1").estimated && by("m1").input == 100 && by("m1").output == 10);
        assert!(by("m0").estimated);
        let m2 = by("m2");
        assert!(!m2.estimated && (m2.input, m2.output) == (7, 3));
    }
}
