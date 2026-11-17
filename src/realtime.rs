use crate::config::Transcribe;
use crate::audio::{self, Threshold};
use crate::core::{Ev, note};
use crate::phrase::{Cut, PhraseCutter};
use crate::record::SttTiming;
use crate::state::{Segment, Shared, Speaker};
use anyhow::{Result, anyhow, bail};
use base64::Engine;
use futures::{Sink, SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::{interval, sleep, timeout};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};

pub const REALTIME_URL: &str = "wss://api.openai.com/v1/realtime?intent=transcription";

const PING_EVERY: Duration = Duration::from_secs(10);
/// No frame (including pongs) for this long means the connection is dead.
const DEAD_AFTER: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// A connection that stayed up this long resets the retry counter.
const STABLE_AFTER: Duration = Duration::from_secs(30);
/// Audio kept across a reconnect: 50 chunks of ~100 ms = ~5 s.
const BACKLOG_CHUNKS: usize = 50;

/// Wait before reconnect attempt `attempt` (1-based): 1, 2, 4, 8, then 10 s.
pub fn backoff(attempt: u32) -> Duration {
    Duration::from_secs((1u64 << attempt.saturating_sub(1).min(4)).min(10))
}

/// An error that retrying cannot fix (bad key, rejected session config).
#[derive(Debug)]
struct Fatal(String);

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Fatal {}

/// Errors that retrying cannot fix: bad key, no access, wrong URL, rejected session config.
fn is_fatal(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Fatal>().is_some()
        || matches!(
            e.downcast_ref::<tungstenite::Error>(),
            Some(tungstenite::Error::Http(resp)) if matches!(resp.status().as_u16(), 401 | 403 | 404)
        )
}

/// Adds the HTTP status and OpenAI's JSON body to a failed handshake; auth errors become fatal.
fn describe_connect_error(e: tungstenite::Error) -> anyhow::Error {
    let tungstenite::Error::Http(resp) = &e else { return e.into() };
    let status = resp.status().as_u16();
    let body = resp.body().as_deref().map(String::from_utf8_lossy).unwrap_or_default();
    let msg = format!("handshake failed: HTTP {status} {}", body.trim().chars().take(500).collect::<String>());
    if matches!(status, 401 | 403 | 404) { Fatal(msg).into() } else { anyhow!(msg) }
}

/// Keeps a transcription connection alive for as long as the task runs.
/// Returns only on a fatal error (or when the audio channel closes).
/// One call per audio source; `speaker` tags the segments it produces.
pub async fn run_with_reconnect(
    cfg: &Transcribe,
    url: &str,
    key: &str,
    mut audio_rx: UnboundedReceiver<Vec<u8>>,
    shared: Shared,
    ev_tx: UnboundedSender<Ev>,
    speaker: Speaker,
) -> Result<()> {
    let conn = Conn { cfg, url, key, shared: &shared, ev_tx: &ev_tx, speaker };
    let mut backlog: VecDeque<Vec<u8>> = VecDeque::new();
    let mut attempt = 0u32;
    // Since when the connection is down (the first failure of the current outage).
    let mut lost_at: Option<Instant> = None;
    loop {
        let mut up_since = None;
        let res = run(&conn, &mut audio_rx, &mut backlog, &mut up_since, lost_at).await;
        shared.update(|s| s.meters[speaker.idx()].connected = false);
        // Whatever was half-transcribed is final now: translate it and detect questions.
        finalize_partials(&shared, &ev_tx, speaker);
        let e = match res {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        if is_fatal(&e) {
            eprintln!("realtime[{speaker:?}]: fatal: {e:#}");
            return Err(e);
        }
        if up_since.is_some_and(|t: Instant| t.elapsed() > STABLE_AFTER) {
            attempt = 0;
        }
        // A connection that was up and dropped starts a new outage; failed retries continue the old one.
        if up_since.is_some() || lost_at.is_none() {
            lost_at = Some(Instant::now());
        }
        attempt += 1;
        let wait = backoff(attempt);
        eprintln!("realtime[{speaker:?}]: connection lost, attempt {attempt}, retry in {}s: {e:#}", wait.as_secs());
        let _ = ev_tx.send(note(
            speaker,
            "CONN",
            "conn_lost",
            format!("connection lost: {e:#} (attempt {attempt}, retry in {} s)", wait.as_secs()),
            json!({ "reason": format!("{e:#}"), "attempt": attempt, "retry_in_ms": wait.as_millis() as u64 }),
        ));
        shared.update(|s| {
            s.status = format!("Reconnecting in {}s (attempt {attempt})…", wait.as_secs());
            s.conn_error = Some(format!("Connection lost: {e}"));
        });
        sleep(wait).await;
        trim_backlog(&mut audio_rx, &mut backlog);
    }
}

/// Moves queued audio into `backlog` and keeps only the most recent ~5 s.
fn trim_backlog(rx: &mut UnboundedReceiver<Vec<u8>>, backlog: &mut VecDeque<Vec<u8>>) {
    while let Ok(c) = rx.try_recv() {
        backlog.push_back(c);
    }
    while backlog.len() > BACKLOG_CHUNKS {
        backlog.pop_front();
    }
}

/// Only touches this speaker's segments: the other session may still be mid-utterance.
fn finalize_partials(shared: &Shared, ev_tx: &UnboundedSender<Ev>, speaker: Speaker) {
    let mut ids = Vec::new();
    shared.update(|s| {
        for seg in s.segments.iter_mut().filter(|x| x.speaker == speaker) {
            if seg.partial {
                seg.partial = false;
                if !seg.es.trim().is_empty() {
                    ids.push(seg.item_id.clone());
                }
            }
        }
        s.segments.retain(|x| x.speaker != speaker || !x.es.trim().is_empty());
    });
    for id in ids {
        let _ = ev_tx.send(Ev::Completed { id, stt: None });
    }
}

async fn send<S>(w: &mut S, msg: Message) -> Result<()>
where
    S: Sink<Message, Error = tungstenite::Error> + Unpin,
{
    timeout(SEND_TIMEOUT, w.send(msg)).await.map_err(|_| anyhow!("send timed out"))??;
    Ok(())
}

/// Everything one connection needs; the same for every reconnect attempt.
struct Conn<'a> {
    cfg: &'a Transcribe,
    url: &'a str,
    key: &'a str,
    shared: &'a Shared,
    ev_tx: &'a UnboundedSender<Ev>,
    speaker: Speaker,
}

/// The session settings sent right after connecting (GA Realtime API shape; the beta one is
/// disabled by OpenAI: `beta_api_shape_disabled`).
fn session_update(cfg: &Transcribe, speaker: Speaker) -> Value {
    let silence_ms = if speaker == Speaker::Me { cfg.mic_silence_ms } else { cfg.silence_ms };
    let live = is_live(cfg, speaker);
    let mut transcription = json!({ "model": cfg.model_for(speaker == Speaker::Them), "language": cfg.language });
    if !cfg.prompt.trim().is_empty() {
        transcription["prompt"] = json!(cfg.prompt.trim());
    }
    json!({
        "type": "session.update",
        "session": {
            "type": "transcription",
            "audio": { "input": {
                "format": { "type": "audio/pcm", "rate": crate::audio::RATE },
                "transcription": transcription,
                // The live model has no pause detection: the app commits each phrase itself (`PhraseCutter`).
                "turn_detection": if live { Value::Null } else { json!({ "type": "server_vad", "threshold": 0.5, "prefix_padding_ms": 300, "silence_duration_ms": silence_ms }) }
            } }
        }
    })
}

/// The other person is transcribed by the live model, and the app cuts the phrases.
fn is_live(cfg: &Transcribe, speaker: Speaker) -> bool {
    speaker == Speaker::Them && cfg.live()
}

/// The text that tells the recognizer what to expect. A short utterance has little to go on, and
/// without this it sometimes writes Spanish in Cyrillic; the language and topic keep it on track.
pub fn stt_prompt(language: &str, topic: Option<&str>, extra: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    if language == "es" {
        parts.push("Conversación en español.".into());
    }
    if let Some(t) = topic.map(str::trim).filter(|t| !t.is_empty()) {
        parts.push(format!("Tema: {t}."));
    }
    if !extra.trim().is_empty() {
        parts.push(extra.trim().to_string());
    }
    parts.join(" ")
}

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// Is this "phrase" not speech at all? On a sound without words the recognizer still has to return
/// something: it either writes a symbol like `###` (no letters) or repeats the prompt it was given.
/// Such a phrase is dropped. A text counts as an echo of the prompt when it is two or more words
/// that stand together in the prompt (case and punctuation ignored), so a real one-word answer
/// such as "Algebra" is kept.
pub fn is_junk(text: &str, prompt: &str) -> bool {
    let w = words(text);
    if w.is_empty() {
        return true;
    }
    let p = words(prompt);
    w.len() >= 2 && p.windows(w.len()).any(|x| x == w.as_slice())
}

/// One connection. `up_since` is set once the session is established.
async fn run(
    conn: &Conn<'_>,
    audio_rx: &mut UnboundedReceiver<Vec<u8>>,
    backlog: &mut VecDeque<Vec<u8>>,
    up_since: &mut Option<Instant>,
    lost_at: Option<Instant>,
) -> Result<()> {
    let Conn { cfg, url, key, shared, ev_tx, speaker } = *conn;
    let mut req = url.into_client_request()?;
    req.headers_mut().insert("Authorization", format!("Bearer {key}").parse()?);
    let (ws, _) = timeout(CONNECT_TIMEOUT, connect_async(req))
        .await
        .map_err(|_| anyhow!("connect timed out"))?
        .map_err(describe_connect_error)?;
    let (mut w, mut r) = ws.split();

    send(&mut w, Message::Text(session_update(cfg, speaker).to_string())).await?;
    *up_since = Some(Instant::now());
    let _ = ev_tx.send(match lost_at {
        Some(t) => {
            let off = t.elapsed();
            note(speaker, "CONN", "conn_reconnected", format!("reconnected after {:.1} s offline", off.as_secs_f64()), json!({ "offline_ms": off.as_millis() as u64 }))
        }
        None => note(speaker, "CONN", "conn_up", "connected".to_string(), json!({})),
    });
    shared.update(|s| {
        s.status = "Listening".into();
        s.conn_error = None;
        s.meters[speaker.idx()].connected = true;
    });

    let b64 = base64::engine::general_purpose::STANDARD;
    let audio_msg = |c: &[u8]| Message::Text(json!({ "type": "input_audio_buffer.append", "audio": b64.encode(c) }).to_string());

    // Live mode: this task decides where a phrase ends (the server has no pause detection for the live model).
    let live = is_live(cfg, speaker);
    let mut cutter = PhraseCutter::new(Duration::from_millis(cfg.silence_ms), Duration::from_secs_f64(cfg.max_phrase_secs), Duration::from_secs_f64(cfg.max_phrase_hard_secs));
    let threshold = shared.read(|s| s.meters[speaker.idx()].threshold.clone());
    let mut clock = LiveClock::default();
    // After the silence gate's hangover no frames come, but a pause still has to be noticed.
    let mut tick = interval(Duration::from_millis(100));

    // Audio captured while we were disconnected goes first.
    while let Some(c) = backlog.pop_front() {
        send(&mut w, audio_msg(&c)).await?;
        note_sent(shared, speaker, c.len());
        if live {
            let cut = cutter.on_frame(Instant::now(), is_loud(&c, &threshold));
            apply_cut(&mut w, cut, speaker, ev_tx, &mut clock).await?;
        }
    }

    let mut ping = interval(PING_EVERY);
    let mut last_rx = Instant::now();
    // The server accepted the session config (or sent transcripts) on this connection.
    let mut accepted = false;
    let mut last_error: Option<String> = None;
    let mut speeches: HashMap<String, Speech> = HashMap::new();
    loop {
        tokio::select! {
            chunk = audio_rx.recv() => {
                let Some(c) = chunk else { return Ok(()) };
                send(&mut w, audio_msg(&c)).await?;
                note_sent(shared, speaker, c.len());
                if live {
                    let cut = cutter.on_frame(Instant::now(), is_loud(&c, &threshold));
                    apply_cut(&mut w, cut, speaker, ev_tx, &mut clock).await?;
                }
            }
            _ = tick.tick(), if live => {
                let cut = cutter.on_tick(Instant::now());
                apply_cut(&mut w, cut, speaker, ev_tx, &mut clock).await?;
            }
            _ = ping.tick() => {
                if last_rx.elapsed() > DEAD_AFTER {
                    bail!("connection timed out");
                }
                send(&mut w, Message::Ping(Vec::new())).await?;
            }
            msg = r.next() => {
                last_rx = Instant::now();
                shared.update(|s| s.meters[speaker.idx()].last_server_msg = Some(last_rx));
                match msg {
                    Some(Ok(Message::Text(t))) => match handle(&t, shared, ev_tx, speaker, &mut speeches, &mut clock)? {
                        Handled::Accepted => accepted = true,
                        Handled::Other => {}
                        Handled::ServerError { kind, message } => {
                            eprintln!("realtime[{speaker:?}]: server error: {t}");
                            let _ = ev_tx.send(note(speaker, "CONN", "server_error", format!("server error: {message}"), json!({ "kind": kind, "message": message })));
                            let msg = format!("OpenAI: {message}");
                            // A config rejected before anything worked cannot be fixed by reconnecting.
                            if kind == "invalid_request_error" && !accepted {
                                return Err(Fatal(msg).into());
                            }
                            last_error = Some(message);
                        }
                    },
                    Some(Ok(Message::Close(frame))) => {
                        let how = frame.map(|f| format!("{} {}", u16::from(f.code), f.reason)).unwrap_or_else(|| "no close frame".into());
                        match &last_error {
                            Some(m) => bail!("OpenAI closed the connection ({how}): {m}"),
                            None => bail!("OpenAI closed the connection ({how})"),
                        }
                    }
                    None => bail!("connection to OpenAI dropped"),
                    Some(Err(e)) => return Err(e.into()),
                    _ => {}
                }
            }
        }
    }
}

/// Records an audio chunk that went to OpenAI (for the Audio window).
fn note_sent(shared: &Shared, speaker: Speaker, bytes: usize) {
    shared.update(|s| {
        let m = &mut s.meters[speaker.idx()];
        m.bytes_sent += bytes as u64;
        m.last_sent = Some(Instant::now());
    });
}

enum Handled {
    /// The server confirmed the session or sent a transcript.
    Accepted,
    ServerError { kind: String, message: String },
    Other,
}

/// What the voice detector reported about one utterance, to time the transcription.
#[derive(Default)]
struct Speech {
    started_ms: Option<u64>,
    stopped_ms: Option<u64>,
    /// Live mode: when the phrase began (the app's own clock; the server sends no `speech_started`).
    started_at: Option<Instant>,
    /// When the end of the speech reached us.
    stopped_at: Option<Instant>,
    got_text: bool,
}

impl Speech {
    fn since_stop_ms(&self) -> Option<u64> {
        self.stopped_at.map(|t| t.elapsed().as_millis() as u64)
    }

    fn since_start_ms(&self) -> Option<u64> {
        self.started_at.map(|t| t.elapsed().as_millis() as u64)
    }

    /// How long the phrase lasted: from the server's voice detector, or from the app's own clock in live mode.
    fn speech_secs(&self) -> Option<f64> {
        self.started_ms
            .zip(self.stopped_ms)
            .map(|(a, b)| b.saturating_sub(a) as f64 / 1000.0)
            .or_else(|| self.started_at.zip(self.stopped_at).map(|(a, b)| b.saturating_duration_since(a).as_secs_f64()))
    }
}

/// Live mode: the phrases the app has begun and closed, in order. The server numbers its items in
/// the order of the commits, so the n-th `committed` event belongs to the n-th phrase here.
#[derive(Default)]
struct LiveClock {
    /// The phrase being spoken now.
    open: Option<Instant>,
    /// Starts of the phrases whose `committed` event has not arrived yet.
    starts: VecDeque<Instant>,
    /// Moments of our `commit` messages whose `committed` event has not arrived yet.
    commits: VecDeque<Instant>,
}

/// Does what the cutter decided: a local `speech_started`, or a `commit` and a local `speech_stopped`.
async fn apply_cut<S>(w: &mut S, cut: Cut, speaker: Speaker, ev_tx: &UnboundedSender<Ev>, clock: &mut LiveClock) -> Result<()>
where
    S: Sink<Message, Error = tungstenite::Error> + Unpin,
{
    let now = Instant::now();
    match cut {
        Cut::None => {}
        Cut::Started => {
            clock.open = Some(now);
            clock.starts.push_back(now);
            // The hint panel switches to "listening" on this event, as it did on the server's.
            let _ = ev_tx.send(note(speaker, "STT", "speech_started", String::new(), json!({ "live": true })));
        }
        Cut::Commit(why) => {
            send(w, Message::Text(json!({ "type": "input_audio_buffer.commit" }).to_string())).await?;
            clock.commits.push_back(now);
            let ms = clock.open.take().map(|t| now.saturating_duration_since(t).as_millis() as u64);
            let _ = ev_tx.send(note(speaker, "STT", "speech_stopped", String::new(), json!({ "live": true, "reason": why.label(), "speech_ms": ms })));
        }
    }
    Ok(())
}

/// Is this 100 ms frame above the silence threshold of the Audio window?
fn is_loud(frame: &[u8], threshold: &Threshold) -> bool {
    audio::rms(frame) >= audio::threshold_get(threshold)
}

fn handle(text: &str, shared: &Shared, ev_tx: &UnboundedSender<Ev>, speaker: Speaker, speeches: &mut HashMap<String, Speech>, clock: &mut LiveClock) -> Result<Handled> {
    let v: Value = serde_json::from_str(text)?;
    let item_id = format!("{}{}", speaker.id_prefix(), v["item_id"].as_str().unwrap_or_default());
    let ty = v["type"].as_str().unwrap_or_default();
    let mut handled = if ty.starts_with("conversation.item.input_audio_transcription") || ty == "session.updated" {
        Handled::Accepted
    } else {
        Handled::Other
    };
    let timing = |kind: &'static str, fields: Value| {
        let _ = ev_tx.send(note(speaker, "STT", kind, String::new(), fields));
    };
    match ty {
        "input_audio_buffer.speech_started" => {
            let ms = v["audio_start_ms"].as_u64();
            speeches.entry(item_id.clone()).or_default().started_ms = ms;
            timing("speech_started", json!({ "id": item_id, "audio_start_ms": ms }));
        }
        "input_audio_buffer.speech_stopped" => {
            let ms = v["audio_end_ms"].as_u64();
            let sp = speeches.entry(item_id.clone()).or_default();
            (sp.stopped_ms, sp.stopped_at) = (ms, Some(Instant::now()));
            timing("speech_stopped", json!({ "id": item_id, "audio_end_ms": ms, "speech_ms": sp.started_ms.zip(ms).map(|(a, b)| b.saturating_sub(a)) }));
        }
        "input_audio_buffer.committed" => {
            let sp = speeches.entry(item_id.clone()).or_default();
            // Live mode: this item is the oldest phrase we closed.
            if let Some(at) = clock.commits.pop_front() {
                sp.stopped_at = Some(at);
                sp.started_at = clock.starts.pop_front().or(sp.started_at);
            }
            timing("committed", json!({ "id": item_id, "since_stop_ms": sp.since_stop_ms() }));
        }
        "conversation.item.input_audio_transcription.delta" => {
            let sp = speeches.entry(item_id.clone()).or_default();
            if sp.started_at.is_none() {
                // Live mode: text of the phrase that is open now (deltas come before its `committed`).
                sp.started_at = clock.starts.front().copied();
            }
            if !std::mem::replace(&mut sp.got_text, true) {
                timing("first_delta", json!({ "id": item_id, "since_stop_ms": sp.since_stop_ms(), "since_start_ms": sp.since_start_ms() }));
            }
            let delta = v["delta"].as_str().unwrap_or_default().to_string();
            shared.update(|s| {
                if let Some(seg) = s.segments.iter_mut().find(|x| x.item_id == item_id) {
                    seg.es.push_str(&delta);
                } else {
                    s.segments.push(Segment { item_id, speaker, es: delta, ru: None, partial: true, at: Instant::now() });
                }
            });
        }
        "conversation.item.input_audio_transcription.completed" => {
            let full = v["transcript"].as_str().unwrap_or_default().trim().to_string();
            let sp = speeches.remove(&item_id).unwrap_or_default();
            let stt = SttTiming {
                speech_secs: sp.speech_secs(),
                stt_secs: sp.stopped_at.map(|t| t.elapsed().as_secs_f64()),
            };
            timing(
                "stt_completed",
                json!({
                    "id": item_id,
                    "speech_ms": stt.speech_secs.map(|s| (s * 1000.0).round() as u64),
                    "stt_ms": stt.stt_secs.map(|s| (s * 1000.0).round() as u64),
                    "chars": full.chars().count(),
                }),
            );
            shared.update(|s| {
                if full.is_empty() {
                    s.segments.retain(|x| x.item_id != item_id);
                } else if let Some(seg) = s.segments.iter_mut().find(|x| x.item_id == item_id) {
                    seg.es = full.clone();
                    seg.partial = false;
                } else {
                    s.segments.push(Segment { item_id: item_id.clone(), speaker, es: full.clone(), ru: None, partial: false, at: Instant::now() });
                }
            });
            if let Some((input_tokens, output_tokens, seconds)) = crate::usage::parse_transcribe_usage(&v) {
                let _ = ev_tx.send(Ev::TranscribeUsage { speaker, input_tokens, output_tokens, seconds });
            }
            if !full.is_empty() {
                let _ = ev_tx.send(Ev::Completed { id: item_id, stt: Some(stt) });
            }
        }
        // Not fatal: show it and keep going. A truly fatal condition is followed by a close,
        // which goes through the reconnect path.
        "error" => {
            let msg = v["error"]["message"].as_str().unwrap_or("unknown error").to_string();
            shared.update(|s| s.conn_error = Some(format!("OpenAI: {msg}")));
            handled = Handled::ServerError { kind: v["error"]["type"].as_str().unwrap_or_default().to_string(), message: msg };
        }
        _ => {}
    }
    Ok(handled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc::unbounded_channel;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::http::Response;

    #[test]
    fn backoff_sequence() {
        let secs: Vec<u64> = (1..=7).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 10, 10, 10]);
    }

    #[test]
    fn fatal_only_for_auth_errors() {
        let http = |code: u16| anyhow::Error::from(tungstenite::Error::Http(Response::builder().status(code).body(None).unwrap()));
        assert!(is_fatal(&http(401)));
        assert!(is_fatal(&http(404)));
        assert!(!is_fatal(&http(503)));
        assert!(!is_fatal(&http(429)));
        let io = anyhow::Error::from(tungstenite::Error::Io(std::io::Error::other("reset")));
        assert!(!is_fatal(&io));
    }

    #[test]
    fn microphone_waits_longer_before_a_phrase_ends() {
        let cfg = test_cfg();
        let at = |sp| session_update(&cfg, sp)["session"]["audio"]["input"]["turn_detection"]["silence_duration_ms"].clone();
        assert_eq!(at(Speaker::Them), json!(500));
        assert_eq!(at(Speaker::Me), json!(900));
    }

    #[test]
    fn prompt_is_sent_only_when_there_is_one() {
        let mut cfg = test_cfg();
        let t = |c: &Transcribe| session_update(c, Speaker::Them)["session"]["audio"]["input"]["transcription"].clone();
        assert!(t(&cfg).get("prompt").is_none());
        cfg.prompt = " Tema: álgebra. ".into();
        assert_eq!(t(&cfg)["prompt"], json!("Tema: álgebra."));
        assert_eq!(t(&cfg)["language"], json!("es"));
    }

    #[test]
    fn recognizer_prompt_is_built_from_language_topic_and_extra() {
        assert_eq!(stt_prompt("es", Some("algebra"), ""), "Conversación en español. Tema: algebra.");
        assert_eq!(stt_prompt("es", None, "Cramer, determinante"), "Conversación en español. Cramer, determinante");
        assert_eq!(stt_prompt("es", Some("  "), ""), "Conversación en español.");
        // No Spanish sentence for another language.
        assert_eq!(stt_prompt("en", Some("math"), ""), "Tema: math.");
    }

    #[test]
    fn junk_is_symbols_or_an_echo_of_the_recognizer_prompt() {
        let prompt = stt_prompt("es", Some("algebra"), "");
        for junk in ["Conversación en español.", "Conversación en español. Tema: algebra.", "conversación en español", "Tema: algebra.", "###", "...", "-", " ", ""] {
            assert!(is_junk(junk, &prompt), "{junk:?} must be dropped");
        }
        for speech in ["Sí.", "Ah,", "Hmm", "Y...", "Algebra", "Hola, buenas noches.", "¿Qué es una preimagen?", "Conversación en la clase."] {
            assert!(!is_junk(speech, &prompt), "{speech:?} must be kept");
        }
        // Without a topic only the language sentence is the prompt.
        assert!(!is_junk("Tema: algebra.", &stt_prompt("es", None, "")));
    }

    fn test_cfg() -> Transcribe {
        Transcribe {
            api_key: String::new(),
            api_key_env: String::new(),
            model: "m".into(),
            language: "es".into(),
            silence_threshold: 300.0,
            mic_silence_threshold: 500.0,
            capture_mic: true,
            silence_ms: 500,
            mic_silence_ms: 900,
            prompt: String::new(),
            // Off here: the tests below talk to a server that sends the old events.
            live_model: String::new(),
            max_phrase_secs: 12.0,
            max_phrase_hard_secs: 20.0,
        }
    }

    /// Server that answers the first client message with `reply` and then closes with `close`.
    async fn one_shot_server(reply: Value, close: Option<tungstenite::protocol::CloseFrame<'static>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut ws = accept_async(tcp).await.unwrap();
                let _ = ws.next().await;
                let _ = ws.send(Message::Text(reply.to_string())).await;
                let _ = ws.close(close.clone()).await;
            }
        });
        url
    }

    #[tokio::test]
    async fn rejected_session_config_is_fatal() {
        let err = json!({ "type": "error", "error": { "type": "invalid_request_error", "message": "bad shape" } });
        let url = one_shot_server(err, None).await;
        let (_tx, rx) = unbounded_channel::<Vec<u8>>();
        let (ev_tx, _ev_rx) = unbounded_channel::<Ev>();
        let res = timeout(Duration::from_secs(5), run_with_reconnect(&test_cfg(), &url, "k", rx, Shared::new(None), ev_tx, Speaker::Them))
            .await
            .expect("must not retry forever");
        assert!(res.unwrap_err().to_string().contains("bad shape"));
    }

    #[tokio::test]
    async fn close_reason_reaches_conn_error() {
        let frame = tungstenite::protocol::CloseFrame { code: 4000u16.into(), reason: "going away".into() };
        let url = one_shot_server(json!({ "type": "session.updated" }), Some(frame)).await;
        let (_tx, rx) = unbounded_channel::<Vec<u8>>();
        let (ev_tx, _ev_rx) = unbounded_channel::<Ev>();
        let shared = Shared::new(None);
        let task = tokio::spawn({
            let shared = shared.clone();
            async move { run_with_reconnect(&test_cfg(), &url, "k", rx, shared, ev_tx, Speaker::Them).await }
        });
        let mut got = None;
        for _ in 0..100 {
            got = shared.read(|s| s.conn_error.clone()).filter(|e| e.contains("4000"));
            if got.is_some() {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
        task.abort();
        assert!(got.is_some_and(|e| e.contains("going away")));
    }

    #[tokio::test]
    async fn reconnects_after_server_drops_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicUsize::new(0));
        let conns = connections.clone();
        tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let n = conns.fetch_add(1, Ordering::SeqCst) + 1;
                let mut ws = accept_async(tcp).await.unwrap();
                let _ = ws.next().await; // session update
                if n == 1 {
                    let ev = json!({ "type": "conversation.item.input_audio_transcription.delta", "item_id": "a", "delta": "Hola" });
                    ws.send(Message::Text(ev.to_string())).await.unwrap();
                    let _ = ws.close(None).await; // drop mid-utterance
                } else {
                    let ev = json!({ "type": "conversation.item.input_audio_transcription.completed", "item_id": "b", "transcript": "Buenos dias" });
                    ws.send(Message::Text(ev.to_string())).await.unwrap();
                    while ws.next().await.is_some() {}
                }
            }
        });

        let shared = Shared::new(None);
        let (audio_tx, audio_rx) = unbounded_channel::<Vec<u8>>();
        let (ev_tx, mut ev_rx) = unbounded_channel::<Ev>();
        let cfg = test_cfg();

        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let (seen2, shared2) = (seen.clone(), shared.clone());
        let poll = tokio::spawn(async move {
            loop {
                {
                    let st = shared2.read(|s| s.status.clone());
                    let mut v = seen2.lock().unwrap();
                    if v.last() != Some(&st) {
                        v.push(st);
                    }
                }
                sleep(Duration::from_millis(20)).await;
            }
        });
        let task = tokio::spawn({
            let shared = shared.clone();
            async move { run_with_reconnect(&cfg, &url, "k", audio_rx, shared, ev_tx, Speaker::Them).await }
        });

        let mut completed = Vec::new();
        let mut notes: Vec<(&str, String)> = Vec::new();
        while completed.len() < 2 {
            let ev = timeout(Duration::from_secs(10), ev_rx.recv()).await.expect("timed out").expect("channel closed");
            match ev {
                Ev::Completed { id, .. } => completed.push(id),
                Ev::Note { kind, text, .. } if kind.starts_with("conn_") => notes.push((kind, text)),
                _ => {}
            }
        }
        assert_eq!(completed, ["them:a", "them:b"]); // "a" finalized from a partial after the drop
        assert_eq!(connections.load(Ordering::SeqCst), 2);
        let kinds: Vec<&str> = notes.iter().map(|n| n.0).collect();
        assert_eq!(kinds, ["conn_up", "conn_lost", "conn_reconnected"], "{notes:?}");
        assert!(notes[1].1.contains("attempt 1") && notes[2].1.contains("s offline"), "{notes:?}");
        let segs = shared.read(|s| s.segments.iter().map(|x| (x.item_id.clone(), x.es.clone(), x.partial)).collect::<Vec<_>>());
        assert_eq!(segs, [("them:a".into(), "Hola".into(), false), ("them:b".into(), "Buenos dias".into(), false)]);
        assert!(seen.lock().unwrap().iter().any(|s| s.starts_with("Reconnecting")), "statuses: {:?}", seen.lock().unwrap());

        drop(audio_tx);
        poll.abort();
        task.abort();
    }
    fn live_cfg() -> Transcribe {
        Transcribe { live_model: "live-m".into(), ..test_cfg() }
    }

    #[test]
    fn live_model_has_no_server_pause_detection_and_only_serves_the_other_person() {
        let cfg = live_cfg();
        let input = |sp| session_update(&cfg, sp)["session"]["audio"]["input"].clone();
        let them = input(Speaker::Them);
        assert_eq!(them["transcription"]["model"], json!("live-m"));
        assert!(them["turn_detection"].is_null(), "the app commits the phrases itself");
        // The microphone keeps the old model and the server's pause detection.
        let me = input(Speaker::Me);
        assert_eq!(me["transcription"]["model"], json!("m"));
        assert_eq!(me["turn_detection"]["type"], json!("server_vad"));
        // Language and prompt are sent for the live model too.
        assert_eq!(them["transcription"]["language"], json!("es"));
        // Without a live model nothing changes for the other person.
        let old = session_update(&test_cfg(), Speaker::Them)["session"]["audio"]["input"].clone();
        assert_eq!((old["transcription"]["model"].clone(), old["turn_detection"]["type"].clone()), (json!("m"), json!("server_vad")));
    }

    /// Server for one live connection: a delta right after the first audio, and the final text once the client commits.
    async fn live_server(seen: UnboundedSender<Value>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(tcp).await.unwrap();
            let mut sent_delta = false;
            while let Some(Ok(m)) = ws.next().await {
                // The client pings right away: only text frames matter here.
                let Message::Text(t) = m else { continue };
                let v: Value = serde_json::from_str(&t).unwrap();
                let ty = v["type"].as_str().unwrap_or_default().to_string();
                let _ = seen.send(v);
                if ty == "input_audio_buffer.append" && !sent_delta {
                    sent_delta = true;
                    let delta = json!({ "type": "conversation.item.input_audio_transcription.delta", "item_id": "a", "delta": "Hola" });
                    ws.send(Message::Text(delta.to_string())).await.unwrap();
                }
                if ty == "input_audio_buffer.commit" {
                    let committed = json!({ "type": "input_audio_buffer.committed", "item_id": "a" });
                    let done = json!({ "type": "conversation.item.input_audio_transcription.completed", "item_id": "a", "transcript": "Hola mundo" });
                    ws.send(Message::Text(committed.to_string())).await.unwrap();
                    ws.send(Message::Text(done.to_string())).await.unwrap();
                }
            }
        });
        url
    }

    #[tokio::test]
    async fn live_phrase_grows_while_spoken_and_is_committed_by_the_app_after_a_pause() {
        let (seen_tx, mut seen) = unbounded_channel::<Value>();
        let url = live_server(seen_tx).await;
        let shared = Shared::new(None);
        audio::threshold_set(&shared.read(|s| s.meters[Speaker::Them.idx()].threshold.clone()), 300.0);
        let (audio_tx, audio_rx) = unbounded_channel::<Vec<u8>>();
        let (ev_tx, mut ev_rx) = unbounded_channel::<Ev>();
        let task = tokio::spawn({
            let shared = shared.clone();
            async move { run_with_reconnect(&live_cfg(), &url, "k", audio_rx, shared, ev_tx, Speaker::Them).await }
        });
        // Two loud 100 ms frames, then nothing at all (as after the silence gate's hangover).
        let loud: Vec<u8> = (0..2400).flat_map(|_| 2000i16.to_le_bytes()).collect();
        audio_tx.send(loud.clone()).unwrap();
        audio_tx.send(loud).unwrap();

        // While the phrase is still open the text is already on screen, as a partial.
        let mut partial = false;
        for _ in 0..100 {
            partial = shared.read(|s| s.segments.iter().any(|x| x.es == "Hola" && x.partial));
            if partial {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
        assert!(partial, "the delta must show before the phrase is closed");

        let mut notes: Vec<(&str, Value)> = Vec::new();
        let completed = loop {
            match timeout(Duration::from_secs(5), ev_rx.recv()).await.expect("no commit after the pause").expect("channel closed") {
                Ev::Completed { id, stt } => break (id, stt),
                Ev::Note { kind, fields, .. } => notes.push((kind, fields)),
                _ => {}
            }
        };
        assert_eq!(completed.0, "them:a");
        let stt = completed.1.expect("timing");
        // The phrase lasted ~0.5 s of silence after the second frame; the final text came right after the commit.
        assert!(stt.speech_secs.is_some_and(|s| (0.4..2.0).contains(&s)), "{stt:?}");
        assert!(stt.stt_secs.is_some_and(|s| s < 1.0), "{stt:?}");
        assert_eq!(shared.read(|s| s.segments.iter().map(|x| (x.es.clone(), x.partial)).collect::<Vec<_>>()), [("Hola mundo".to_string(), false)]);

        let kinds: Vec<&str> = notes.iter().map(|n| n.0).collect();
        let pos = |k: &str| kinds.iter().position(|x| *x == k).unwrap_or_else(|| panic!("no {k} in {kinds:?}"));
        assert!(pos("speech_started") < pos("speech_stopped"), "{kinds:?}");
        assert_eq!(notes[pos("speech_stopped")].1["reason"], json!("pause"));
        assert!(notes[pos("first_delta")].1["since_start_ms"].is_u64(), "{:?}", notes[pos("first_delta")]);

        // What the server saw: no pause detection in the session, one commit, after the audio.
        let mut types = Vec::new();
        let mut first = None;
        while let Ok(v) = seen.try_recv() {
            first.get_or_insert_with(|| v.clone());
            types.push(v["type"].as_str().unwrap_or_default().to_string());
        }
        let first = first.unwrap();
        assert_eq!(first["type"], json!("session.update"));
        assert!(first["session"]["audio"]["input"]["turn_detection"].is_null());
        assert_eq!(types.iter().filter(|t| *t == "input_audio_buffer.commit").count(), 1, "{types:?}");
        assert_eq!(types.iter().position(|t| t == "input_audio_buffer.commit"), Some(types.len() - 1), "{types:?}");
        task.abort();
    }

    #[test]
    fn live_timing_comes_from_the_apps_own_clock() {
        let (ev_tx, mut ev_rx) = unbounded_channel::<Ev>();
        let shared = Shared::new(None);
        let mut speeches = HashMap::new();
        let now = Instant::now();
        let mut clock = LiveClock::default();
        clock.starts.push_back(now - Duration::from_millis(3000));
        clock.commits.push_back(now - Duration::from_millis(500));
        let mut feed = |v: Value| handle(&v.to_string(), &shared, &ev_tx, Speaker::Them, &mut speeches, &mut clock).unwrap();
        feed(json!({ "type": "conversation.item.input_audio_transcription.delta", "item_id": "a", "delta": "Hola" }));
        feed(json!({ "type": "input_audio_buffer.committed", "item_id": "a" }));
        feed(json!({ "type": "conversation.item.input_audio_transcription.completed", "item_id": "a", "transcript": "Hola" }));
        let (mut first_delta, mut stt) = (None, None);
        while let Ok(ev) = ev_rx.try_recv() {
            match ev {
                Ev::Note { kind: "first_delta", fields, .. } => first_delta = Some(fields),
                Ev::Completed { stt: Some(t), .. } => stt = Some(t),
                _ => {}
            }
        }
        let ms = first_delta.expect("first_delta")["since_start_ms"].as_u64().expect("since_start_ms");
        assert!((3000..3500).contains(&ms), "{ms}");
        let stt = stt.expect("timing");
        // Speech: from the start to the commit (2.5 s); the text came 0.5 s after the commit.
        assert!(stt.speech_secs.is_some_and(|s| (s - 2.5).abs() < 0.05), "{stt:?}");
        assert!(stt.stt_secs.is_some_and(|s| (0.5..1.0).contains(&s)), "{stt:?}");
        assert!(clock.starts.is_empty() && clock.commits.is_empty());
    }
    /// Real OpenAI with the live model; costs money (a 52 s recording is about $0.015). Run by hand:
    /// `cargo test live_api -- --ignored --nocapture` (key from ~/assistant/config.toml; `LIVE_WAV` picks the recording,
    /// a 24 kHz 16-bit mono wav). Feeds the recording through `run_with_reconnect` at real speed.
    #[tokio::test]
    #[ignore]
    async fn live_api_smoke_with_a_recorded_monologue() {
        let wav = std::env::var("LIVE_WAV").unwrap_or_else(|_| "issues/02/experiments/audio/exp2_52s_0858-0950.wav".into());
        let bytes = std::fs::read(&wav).unwrap_or_else(|e| panic!("{wav}: {e}"));
        let data = bytes.windows(4).position(|w| w == b"data").expect("data chunk") + 8;
        let pcm = bytes[data..].to_vec();
        let home = std::env::var("HOME").unwrap();
        let cfg = crate::config::Config::load(&format!("{home}/assistant/config.toml")).unwrap();
        let mut tcfg = Transcribe { live_model: "gpt-live-transcribe".into(), ..cfg.transcribe.clone() };
        tcfg.prompt = stt_prompt(&tcfg.language, Some("inteligencia-artificial"), "");
        let key = tcfg.key().unwrap();

        let shared = Shared::new(None);
        audio::threshold_set(&shared.read(|s| s.meters[Speaker::Them.idx()].threshold.clone()), tcfg.silence_threshold);
        let (audio_tx, audio_rx) = unbounded_channel::<Vec<u8>>();
        let (ev_tx, mut ev_rx) = unbounded_channel::<Ev>();
        let task = tokio::spawn({
            let shared = shared.clone();
            async move { run_with_reconnect(&tcfg, REALTIME_URL, &key, audio_rx, shared, ev_tx, Speaker::Them).await }
        });
        let feeder = tokio::spawn(async move {
            sleep(Duration::from_secs(2)).await; // let the connection come up
            let mut tick = interval(Duration::from_millis(100));
            for c in pcm.chunks(4800) {
                tick.tick().await;
                audio_tx.send(c.to_vec()).unwrap();
            }
            sleep(Duration::from_secs(1)).await;
            drop(audio_tx);
        });

        let (mut phrases, mut first_ms, mut last_event) = (Vec::new(), Vec::new(), Instant::now());
        // Inside a long phrase nothing is reported until it is closed (up to 20 s): wait for the end of the audio first.
        while !(feeder.is_finished() && last_event.elapsed() > Duration::from_secs(6)) {
            let Ok(Some(ev)) = timeout(Duration::from_secs(1), ev_rx.recv()).await else { continue };
            last_event = Instant::now();
            match ev {
                Ev::Note { kind: "first_delta", fields, .. } => first_ms.push(fields["since_start_ms"].as_u64()),
                Ev::Note { kind: "speech_stopped", fields, .. } => println!("closed: {} after {} ms", fields["reason"], fields["speech_ms"]),
                Ev::Completed { id, stt } => {
                    let text = shared.read(|s| s.segments.iter().find(|x| x.item_id == id).map(|x| x.es.clone())).unwrap_or_default();
                    println!("phrase {id}: speech {:?} s, stt {:?} s: {text}", stt.as_ref().and_then(|t| t.speech_secs), stt.as_ref().and_then(|t| t.stt_secs));
                    phrases.push((stt.and_then(|t| t.speech_secs).unwrap_or(0.0), text));
                }
                _ => {}
            }
        }
        task.abort();
        println!("first words after (ms from the start of each phrase): {first_ms:?}");
        assert!(shared.read(|s| s.conn_error.is_none()), "{:?}", shared.read(|s| s.conn_error.clone()));
        assert!(phrases.len() >= 3, "a 52 s monologue must be cut into several phrases: {phrases:?}");
        assert!(phrases.iter().all(|(secs, text)| *secs <= 21.0 && !text.trim().is_empty()), "{phrases:?}");
        assert!(first_ms.first().copied().flatten().is_some_and(|ms| ms < 4000), "{first_ms:?}");
    }
}
