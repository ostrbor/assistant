use crate::config::{Config, env_key};
use crate::llm::Llm;
use crate::search::Index;
use crate::state::{Shared, Speaker};
use crate::{audio, hint, realtime};
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::{AbortHandle, JoinHandle};

pub enum Cmd {
    Start,
    Stop,
    AskHint,
    Clear,
    /// Switch to a document subfolder of docs_dir.
    SelectTopic(String),
    /// Re-scan docs_dir for subfolders.
    RefreshTopics,
}

pub enum Ev {
    Completed(String),
    Error(String),
}

pub struct Core {
    pub cfg: Arc<Config>,
    pub shared: Shared,
    /// Index of the selected document folder; None until one is selected.
    pub index: RwLock<Option<Arc<Index>>>,
    pub llm: Llm,
    pub prep: Mutex<Option<AbortHandle>>,
}

impl Core {
    pub fn new(cfg: Arc<Config>, shared: Shared) -> Self {
        let llm = Llm::new(&cfg.llm);
        Self { cfg, shared, index: RwLock::new(None), llm, prep: Mutex::new(None) }
    }

    /// Stops a hint that is still being prepared or streamed.
    pub fn cancel_hint(&self) {
        if let Some(h) = self.prep.lock().unwrap().take() {
            h.abort();
        }
    }
}

struct Session {
    stop: Arc<AtomicBool>,
    /// One transcription task per audio source (speakers, and the microphone if enabled).
    tasks: Vec<JoinHandle<()>>,
}

pub async fn run(core: Arc<Core>, mut cmd_rx: UnboundedReceiver<Cmd>) {
    let (ev_tx, mut ev_rx) = unbounded_channel::<Ev>();
    let mut session: Option<Session> = None;
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                None => break,
                Some(Cmd::Start) => if session.is_none() {
                    match start(&core, &ev_tx) {
                        Ok(s) => session = Some(s),
                        Err(e) => core.shared.update(|s| s.error = Some(e.to_string())),
                    }
                },
                Some(Cmd::Stop) => stop(&mut session, &core),
                Some(Cmd::AskHint) => hint::ask(&core),
                Some(Cmd::SelectTopic(t)) => select_topic(&core, t),
                Some(Cmd::RefreshTopics) => refresh_topics(&core),
                Some(Cmd::Clear) => {
                    // Stop a hint that is still streaming, or it would reappear.
                    core.cancel_hint();
                    core.shared.update(|s| {
                        s.segments.clear();
                        s.hint = None;
                        s.pending = None;
                        s.pending_shown = false;
                        s.error = None;
                    });
                }
            },
            ev = ev_rx.recv() => match ev {
                Some(Ev::Completed(id)) => on_completed(&core, &id),
                Some(Ev::Error(e)) => {
                    if session.is_some() {
                        stop(&mut session, &core);
                        core.shared.update(|s| s.error = Some(e));
                    }
                }
                None => break,
            }
        }
    }
}

fn start(core: &Arc<Core>, ev_tx: &UnboundedSender<Ev>) -> Result<Session> {
    let key = env_key(&core.cfg.transcribe.api_key_env)?;
    core.shared.update(|s| {
        s.listening = true;
        s.listening_since = Some(Instant::now());
        s.status = "Connecting…".into();
        s.error = None;
        s.conn_error = None;
    });

    let stop = Arc::new(AtomicBool::new(false));
    let t = &core.cfg.transcribe;
    let mut tasks = vec![spawn_stream(core, &key, Speaker::Them, audio::Source::Speakers, t.silence_threshold, &stop, ev_tx)];
    if t.capture_mic {
        tasks.push(spawn_stream(core, &key, Speaker::Me, audio::Source::Mic, t.mic_silence_threshold, &stop, ev_tx));
    }
    Ok(Session { stop, tasks })
}

/// Audio capture runs on its own thread (blocking pulse API). If the device goes away
/// (e.g. headphones switched), keep retrying; the default device is re-read on each attempt.
/// Silence is dropped before the frames are sent to `audio_tx`.
pub fn spawn_capture(shared: &Shared, source: audio::Source, threshold: f64, stop: &Arc<AtomicBool>, audio_tx: UnboundedSender<Vec<u8>>) {
    let lost = match source {
        audio::Source::Speakers => "Audio device lost",
        audio::Source::Mic => "Microphone lost",
    };
    let (stop2, shared2) = (stop.clone(), shared.clone());
    std::thread::spawn(move || {
        let mut gate = audio::SilenceGate::new(threshold);
        let mut recovering = false;
        while !stop2.load(Ordering::Relaxed) {
            let res = audio::capture(source, &stop2, |c| {
                if recovering {
                    recovering = false;
                    shared2.update(|s| {
                        if s.status.starts_with(lost) {
                            s.status = "Listening".into();
                        }
                    });
                }
                // An empty result (silence) counts as success; a closed receiver stops the capture.
                gate.push(c).into_iter().all(|f| audio_tx.send(f).is_ok())
            });
            match res {
                Ok(()) => break, // stopped, or the receiver is gone
                Err(e) => {
                    if stop2.load(Ordering::Relaxed) {
                        break;
                    }
                    recovering = true;
                    shared2.update(|s| s.status = format!("{lost}, retrying… ({e})"));
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        }
    });
}

/// Capture thread + transcription task for one audio source. Silence is dropped before it is sent.
fn spawn_stream(
    core: &Arc<Core>,
    key: &str,
    speaker: Speaker,
    source: audio::Source,
    threshold: f64,
    stop: &Arc<AtomicBool>,
    ev_tx: &UnboundedSender<Ev>,
) -> JoinHandle<()> {
    let (audio_tx, audio_rx) = unbounded_channel::<Vec<u8>>();
    spawn_capture(&core.shared, source, threshold, stop, audio_tx);

    let (core2, ev2, key) = (core.clone(), ev_tx.clone(), key.to_string());
    tokio::spawn(async move {
        let shared = core2.shared.clone();
        if let Err(e) = realtime::run_with_reconnect(&core2.cfg.transcribe, realtime::REALTIME_URL, &key, audio_rx, shared, ev2.clone(), speaker).await {
            // Only fatal errors (bad key, no access) get here; everything else is retried.
            let _ = ev2.send(Ev::Error(format!("Transcription: {e}")));
        }
    })
}

/// Stops capture and closes the WebSocket: nothing is spent while stopped.
fn stop(session: &mut Option<Session>, core: &Arc<Core>) {
    if let Some(s) = session.take() {
        s.stop.store(true, Ordering::Relaxed);
        for t in &s.tasks {
            t.abort();
        }
    }
    core.shared.update(|s| {
        s.listening = false;
        s.listening_since = None;
        s.status = "Stopped".into();
        s.conn_error = None;
        for seg in &mut s.segments {
            seg.partial = false;
        }
    });
}

fn on_completed(core: &Arc<Core>, item_id: &str) {
    let found = core.shared.read(|s| {
        let idx = s.segments.iter().position(|x| x.item_id == item_id)?;
        let ctx: Vec<String> = s.segments[..idx].iter().rev().filter(|x| !x.partial).take(2).map(|x| x.es.clone()).collect();
        Some((s.segments[idx].speaker, s.segments[idx].es.clone(), ctx.into_iter().rev().collect::<Vec<_>>()))
    });
    let Some((speaker, es, ctx)) = found else { return };

    // The user spoke: no translation needed, and a hint prepared before their answer is stale.
    // A hint that is on screen (possibly still streaming) is left alone.
    if speaker == Speaker::Me {
        let mut shown = false;
        core.shared.update(|s| {
            shown = s.pending_shown;
            s.pending = None;
        });
        if !shown {
            core.cancel_hint();
        }
        return;
    }

    // Translation does not block receiving the next utterances.
    let (core2, id) = (core.clone(), item_id.to_string());
    let es2 = es.clone();
    tokio::spawn(async move {
        let en = match core2.llm.translate(&core2.cfg.llm.translate_model, &ctx, &es2).await {
            Ok(t) => t,
            Err(e) => format!("[translation error: {e}]"),
        };
        core2.shared.update(|s| {
            if let Some(seg) = s.segments.iter_mut().find(|x| x.item_id == id) {
                seg.en = Some(en);
            }
        });
    });

    // Looks like a question: prepare the hint in advance.
    if hint::is_question(&es) {
        hint::prepare(core);
    }
}

pub fn refresh_topics(core: &Arc<Core>) {
    match crate::docs::list_topics(&core.cfg.docs_dir) {
        Ok(topics) => core.shared.update(|s| {
            if s.topic.as_ref().is_some_and(|t| !topics.contains(t)) {
                s.topic = None;
                s.topic_info = None;
            }
            s.topics = topics;
        }),
        Err(e) => core.shared.update(|s| s.error = Some(e.to_string())),
    }
}

/// Loads and indexes a document folder off the async threads, then swaps the index in.
fn select_topic(core: &Arc<Core>, topic: String) {
    let dir = match crate::docs::topic_dir(&core.cfg.docs_dir, &topic) {
        Ok(d) => d,
        Err(e) => return core.shared.update(|s| s.error = Some(e.to_string())),
    };
    core.shared.update(|s| {
        s.topic_loading = true;
        s.topic_info = None;
        s.error = None;
    });
    let core2 = core.clone();
    tokio::spawn(async move {
        let built = tokio::task::spawn_blocking(move || crate::docs::load_chunks(&dir).map(Index::build)).await;
        match built.map_err(anyhow::Error::from).and_then(|r| r) {
            Ok(index) => {
                let n = index.len();
                *core2.index.write().unwrap() = Some(Arc::new(index));
                // A hint prepared from the previous folder is no longer valid.
                core2.cancel_hint();
                core2.shared.update(|s| {
                    s.topic = Some(topic.clone());
                    s.topic_loading = false;
                    s.pending = None;
                    s.hint = None;
                    s.topic_info = Some(format!("{n} chunks"));
                    if n == 0 {
                        s.error = Some(format!("No text found in {topic} (scanned PDFs are not supported)"));
                    }
                });
            }
            Err(e) => core2.shared.update(|s| {
                s.topic_loading = false;
                s.error = Some(format!("Could not load {topic}: {e}"));
            }),
        }
    });
}
