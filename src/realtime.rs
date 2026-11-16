use crate::config::Transcribe;
use crate::core::{Ev, note};
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
    let mut transcription = json!({ "model": cfg.model, "language": cfg.language });
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
                "turn_detection": { "type": "server_vad", "threshold": 0.5, "prefix_padding_ms": 300, "silence_duration_ms": silence_ms }
            } }
        }
    })
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

    // Audio captured while we were disconnected goes first.
    while let Some(c) = backlog.pop_front() {
        send(&mut w, audio_msg(&c)).await?;
        note_sent(shared, speaker, c.len());
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
                    Some(Ok(Message::Text(t))) => match handle(&t, shared, ev_tx, speaker, &mut speeches)? {
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
    /// When the end of the speech reached us.
    stopped_at: Option<Instant>,
    got_text: bool,
}

impl Speech {
    fn since_stop_ms(&self) -> Option<u64> {
        self.stopped_at.map(|t| t.elapsed().as_millis() as u64)
    }
}

fn handle(text: &str, shared: &Shared, ev_tx: &UnboundedSender<Ev>, speaker: Speaker, speeches: &mut HashMap<String, Speech>) -> Result<Handled> {
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
            let since = speeches.get(&item_id).and_then(Speech::since_stop_ms);
            timing("committed", json!({ "id": item_id, "since_stop_ms": since }));
        }
        "conversation.item.input_audio_transcription.delta" => {
            let sp = speeches.entry(item_id.clone()).or_default();
            if !std::mem::replace(&mut sp.got_text, true) {
                timing("first_delta", json!({ "id": item_id, "since_stop_ms": sp.since_stop_ms() }));
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
                speech_secs: sp.started_ms.zip(sp.stopped_ms).map(|(a, b)| b.saturating_sub(a) as f64 / 1000.0),
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
        let cfg = Transcribe {
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
        };

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
}
