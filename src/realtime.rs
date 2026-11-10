use crate::config::Transcribe;
use crate::core::Ev;
use crate::state::{Segment, Shared};
use anyhow::{Result, anyhow, bail};
use base64::Engine;
use futures::{Sink, SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::VecDeque;
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

/// Errors that retrying cannot fix: bad key, no access, wrong URL.
fn is_fatal(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<tungstenite::Error>(),
        Some(tungstenite::Error::Http(resp)) if matches!(resp.status().as_u16(), 401 | 403 | 404)
    )
}

/// Keeps a transcription connection alive for as long as the task runs.
/// Returns only on a fatal error (or when the audio channel closes).
pub async fn run_with_reconnect(
    cfg: &Transcribe,
    url: &str,
    key: &str,
    mut audio_rx: UnboundedReceiver<Vec<u8>>,
    shared: Shared,
    ev_tx: UnboundedSender<Ev>,
) -> Result<()> {
    let mut backlog: VecDeque<Vec<u8>> = VecDeque::new();
    let mut attempt = 0u32;
    loop {
        let mut up_since = None;
        let res = run(cfg, url, key, &mut audio_rx, &mut backlog, &shared, &ev_tx, &mut up_since).await;
        // Whatever was half-transcribed is final now: translate it and detect questions.
        finalize_partials(&shared, &ev_tx);
        let e = match res {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        if is_fatal(&e) {
            return Err(e);
        }
        if up_since.is_some_and(|t: Instant| t.elapsed() > STABLE_AFTER) {
            attempt = 0;
        }
        attempt += 1;
        let wait = backoff(attempt);
        shared.update(|s| {
            s.status = format!("Reconnecting in {}s (attempt {attempt})…", wait.as_secs());
            s.error = Some(format!("Connection lost: {e}"));
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

fn finalize_partials(shared: &Shared, ev_tx: &UnboundedSender<Ev>) {
    let mut ids = Vec::new();
    shared.update(|s| {
        for seg in &mut s.segments {
            if seg.partial {
                seg.partial = false;
                if !seg.es.trim().is_empty() {
                    ids.push(seg.item_id.clone());
                }
            }
        }
        s.segments.retain(|x| !x.es.trim().is_empty());
    });
    for id in ids {
        let _ = ev_tx.send(Ev::Completed(id));
    }
}

async fn send<S>(w: &mut S, text: String) -> Result<()>
where
    S: Sink<Message, Error = tungstenite::Error> + Unpin,
{
    timeout(SEND_TIMEOUT, w.send(Message::Text(text))).await.map_err(|_| anyhow!("send timed out"))??;
    Ok(())
}

/// One connection. `up_since` is set once the session is established.
#[allow(clippy::too_many_arguments)]
async fn run(
    cfg: &Transcribe,
    url: &str,
    key: &str,
    audio_rx: &mut UnboundedReceiver<Vec<u8>>,
    backlog: &mut VecDeque<Vec<u8>>,
    shared: &Shared,
    ev_tx: &UnboundedSender<Ev>,
    up_since: &mut Option<Instant>,
) -> Result<()> {
    let mut req = url.into_client_request()?;
    req.headers_mut().insert("Authorization", format!("Bearer {key}").parse()?);
    req.headers_mut().insert("OpenAI-Beta", "realtime=v1".parse()?);
    let (ws, _) = timeout(CONNECT_TIMEOUT, connect_async(req)).await.map_err(|_| anyhow!("connect timed out"))??;
    let (mut w, mut r) = ws.split();

    let session = json!({
        "type": "transcription_session.update",
        "session": {
            "input_audio_format": "pcm16",
            "input_audio_transcription": { "model": cfg.model, "language": cfg.language },
            "turn_detection": { "type": "server_vad", "threshold": 0.5, "prefix_padding_ms": 300, "silence_duration_ms": 500 }
        }
    });
    send(&mut w, session.to_string()).await?;
    *up_since = Some(Instant::now());
    shared.update(|s| {
        s.status = "Listening".into();
        s.error = None;
    });

    let b64 = base64::engine::general_purpose::STANDARD;
    let audio_msg = |c: &[u8]| json!({ "type": "input_audio_buffer.append", "audio": b64.encode(c) }).to_string();

    // Audio captured while we were disconnected goes first.
    while let Some(c) = backlog.pop_front() {
        send(&mut w, audio_msg(&c)).await?;
    }

    let mut ping = interval(PING_EVERY);
    let mut last_rx = Instant::now();
    loop {
        tokio::select! {
            chunk = audio_rx.recv() => {
                let Some(c) = chunk else { return Ok(()) };
                send(&mut w, audio_msg(&c)).await?;
            }
            _ = ping.tick() => {
                if last_rx.elapsed() > DEAD_AFTER {
                    bail!("connection timed out");
                }
                send_ping(&mut w).await?;
            }
            msg = r.next() => {
                last_rx = Instant::now();
                match msg {
                    Some(Ok(Message::Text(t))) => handle(&t, shared, ev_tx)?,
                    Some(Ok(Message::Close(_))) | None => bail!("connection to OpenAI closed"),
                    Some(Err(e)) => return Err(e.into()),
                    _ => {}
                }
            }
        }
    }
}

async fn send_ping<S>(w: &mut S) -> Result<()>
where
    S: Sink<Message, Error = tungstenite::Error> + Unpin,
{
    timeout(SEND_TIMEOUT, w.send(Message::Ping(Vec::new()))).await.map_err(|_| anyhow!("ping timed out"))??;
    Ok(())
}

fn handle(text: &str, shared: &Shared, ev_tx: &UnboundedSender<Ev>) -> Result<()> {
    let v: Value = serde_json::from_str(text)?;
    let item_id = v["item_id"].as_str().unwrap_or_default().to_string();
    match v["type"].as_str().unwrap_or_default() {
        "conversation.item.input_audio_transcription.delta" => {
            let delta = v["delta"].as_str().unwrap_or_default().to_string();
            shared.update(|s| {
                if let Some(seg) = s.segments.iter_mut().find(|x| x.item_id == item_id) {
                    seg.es.push_str(&delta);
                } else {
                    s.segments.push(Segment { item_id, es: delta, en: None, partial: true, at: Instant::now() });
                }
            });
        }
        "conversation.item.input_audio_transcription.completed" => {
            let full = v["transcript"].as_str().unwrap_or_default().trim().to_string();
            let id = item_id.clone();
            shared.update(|s| {
                if full.is_empty() {
                    s.segments.retain(|x| x.item_id != id);
                } else if let Some(seg) = s.segments.iter_mut().find(|x| x.item_id == id) {
                    seg.es = full.clone();
                    seg.partial = false;
                } else {
                    s.segments.push(Segment { item_id: id, es: full.clone(), en: None, partial: false, at: Instant::now() });
                }
            });
            if !full.is_empty() {
                let _ = ev_tx.send(Ev::Completed(item_id));
            }
        }
        // Not fatal: show it and keep going. A truly fatal condition is followed by a close,
        // which goes through the reconnect path.
        "error" => {
            let msg = v["error"]["message"].as_str().unwrap_or("unknown error").to_string();
            shared.update(|s| s.error = Some(format!("OpenAI: {msg}")));
        }
        _ => {}
    }
    Ok(())
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
        let cfg = Transcribe { api_key_env: String::new(), model: "m".into(), language: "es".into() };

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
            async move { run_with_reconnect(&cfg, &url, "k", audio_rx, shared, ev_tx).await }
        });

        let mut completed = Vec::new();
        while completed.len() < 2 {
            let ev = timeout(Duration::from_secs(10), ev_rx.recv()).await.expect("timed out").expect("channel closed");
            if let Ev::Completed(id) = ev {
                completed.push(id);
            }
        }
        assert_eq!(completed, ["a", "b"]); // "a" finalized from a partial after the drop
        assert_eq!(connections.load(Ordering::SeqCst), 2);
        let segs = shared.read(|s| s.segments.iter().map(|x| (x.item_id.clone(), x.es.clone(), x.partial)).collect::<Vec<_>>());
        assert_eq!(segs, [("a".into(), "Hola".into(), false), ("b".into(), "Buenos dias".into(), false)]);
        assert!(seen.lock().unwrap().iter().any(|s| s.starts_with("Reconnecting")), "statuses: {:?}", seen.lock().unwrap());

        drop(audio_tx);
        poll.abort();
        task.abort();
    }
}
