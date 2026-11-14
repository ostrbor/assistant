use crate::config::Config;
use crate::llm::Llm;
use crate::record::{SessionLog, WavWriter};
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
    /// Capture audio only to show levels in the Audio window (nothing is sent anywhere).
    MonitorStart,
    MonitorStop,
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
    /// Debug record of the current Listen session (text + audio).
    pub rec: Mutex<Option<Arc<SessionLog>>>,
}

impl Core {
    pub fn new(cfg: Arc<Config>, shared: Shared) -> Self {
        let llm = Llm::new(&cfg.llm);
        shared.update(|s| {
            audio::threshold_set(&s.meters[Speaker::Them.idx()].threshold, cfg.transcribe.silence_threshold);
            audio::threshold_set(&s.meters[Speaker::Me.idx()].threshold, cfg.transcribe.mic_silence_threshold);
        });
        Self { cfg, shared, index: RwLock::new(None), llm, prep: Mutex::new(None), rec: Mutex::new(None) }
    }

    /// The record of the current session, if one is open.
    pub fn rec(&self) -> Option<Arc<SessionLog>> {
        self.rec.lock().unwrap().clone()
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
    let mut monitor: Option<Session> = None;
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                None => break,
                Some(Cmd::MonitorStart) => if session.is_none() && monitor.is_none() {
                    monitor = Some(start_monitor(&core));
                },
                Some(Cmd::MonitorStop) => stop_monitor(&mut monitor, &core),
                Some(Cmd::Start) => if session.is_none() {
                    stop_monitor(&mut monitor, &core);
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
                        if let Some(r) = core.rec() {
                            r.error(&e);
                        }
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
    let key = core.cfg.transcribe.key()?;
    core.shared.update(|s| {
        s.listening = true;
        s.listening_since = Some(Instant::now());
        s.status = "Connecting…".into();
        s.error = None;
        s.conn_error = None;
        s.meters.iter_mut().for_each(|m| m.reset());
    });

    // A failure to record must not prevent listening: it is only shown as an error.
    let base = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let topic = core.shared.read(|s| s.topic.clone());
    let rec = match SessionLog::create(&base, &core.cfg, topic.as_deref()) {
        Ok(r) => Some(Arc::new(r)),
        Err(e) => {
            core.shared.update(|s| s.error = Some(format!("Session record: {e:#}")));
            None
        }
    };
    let wav = |name: &str| {
        let r = rec.as_ref()?;
        match r.wav(name) {
            Ok(w) => Some(w),
            Err(e) => {
                r.error(&format!("{name}: {e:#}"));
                None
            }
        }
    };
    let (them_wav, me_wav) = (wav("them.wav"), if core.cfg.transcribe.capture_mic { wav("me.wav") } else { None });
    *core.rec.lock().unwrap() = rec;

    let stop = Arc::new(AtomicBool::new(false));
    let mut tasks = vec![spawn_stream(core, &key, Speaker::Them, audio::Source::Speakers, &stop, ev_tx, them_wav)];
    if core.cfg.transcribe.capture_mic {
        tasks.push(spawn_stream(core, &key, Speaker::Me, audio::Source::Mic, &stop, ev_tx, me_wav));
    }
    Ok(Session { stop, tasks })
}

/// Index of a source in `AppState::meters`.
fn meter_idx(source: audio::Source) -> usize {
    match source {
        audio::Source::Speakers => Speaker::Them.idx(),
        audio::Source::Mic => Speaker::Me.idx(),
    }
}

/// Captures through the gate and throws the frames away: levels only, no OpenAI connection.
fn start_monitor(core: &Arc<Core>) -> Session {
    core.shared.update(|s| {
        s.monitoring = true;
        s.meters.iter_mut().for_each(|m| m.reset());
    });
    let stop = Arc::new(AtomicBool::new(false));
    let mut sources = vec![audio::Source::Speakers];
    if core.cfg.transcribe.capture_mic {
        sources.push(audio::Source::Mic);
    }
    let tasks = sources
        .into_iter()
        .map(|source| {
            let (tx, mut rx) = unbounded_channel::<Vec<u8>>();
            let threshold = core.shared.read(|s| s.meters[meter_idx(source)].threshold.clone());
            spawn_capture(&core.shared, source, threshold, &stop, tx, None);
            tokio::spawn(async move { while rx.recv().await.is_some() {} })
        })
        .collect();
    Session { stop, tasks }
}

fn stop_monitor(monitor: &mut Option<Session>, core: &Arc<Core>) {
    if let Some(m) = monitor.take() {
        m.stop.store(true, Ordering::Relaxed);
        for t in &m.tasks {
            t.abort();
        }
    }
    core.shared.update(|s| s.monitoring = false);
}

/// Audio capture runs on its own thread (blocking pulse API). If the device goes away
/// (e.g. headphones switched), keep retrying; the default device is re-read on each attempt.
/// Silence is dropped before the frames are sent to `audio_tx`; `wav` gets the raw audio before the gate.
pub fn spawn_capture(shared: &Shared, source: audio::Source, threshold: audio::Threshold, stop: &Arc<AtomicBool>, audio_tx: UnboundedSender<Vec<u8>>, mut wav: Option<WavWriter>) {
    let lost = match source {
        audio::Source::Speakers => "Audio device lost",
        audio::Source::Mic => "Microphone lost",
    };
    let (stop2, shared2) = (stop.clone(), shared.clone());
    let idx = meter_idx(source);
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
                if let Some(w) = wav.as_mut() {
                    w.write(&c);
                }
                // An empty result (silence) counts as success; a closed receiver stops the capture.
                let out = gate.push(c);
                shared2.update(|s| s.meters[idx].record(out.rms, out.sent));
                out.frames.into_iter().all(|f| audio_tx.send(f).is_ok())
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
    stop: &Arc<AtomicBool>,
    ev_tx: &UnboundedSender<Ev>,
    wav: Option<WavWriter>,
) -> JoinHandle<()> {
    let (audio_tx, audio_rx) = unbounded_channel::<Vec<u8>>();
    let threshold = core.shared.read(|s| s.meters[meter_idx(source)].threshold.clone());
    spawn_capture(&core.shared, source, threshold, stop, audio_tx, wav);

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
    if let Some(r) = core.rec.lock().unwrap().take() {
        r.stopped();
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
    let rec = core.rec();
    if let Some(r) = &rec {
        r.utterance(item_id, speaker, &es);
    }

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
        if let Some(r) = &rec {
            r.translation(&id, &en);
        }
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
                s.docs_report = None;
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
        s.docs_report = None;
        s.error = None;
    });
    let core2 = core.clone();
    tokio::spawn(async move {
        let built = tokio::task::spawn_blocking(move || crate::docs::load_chunks(&dir).map(|(c, r)| (Index::build(c), r))).await;
        match built.map_err(anyhow::Error::from).and_then(|r| r) {
            Ok((index, files)) => {
                let n = index.len();
                *core2.index.write().unwrap() = Some(Arc::new(index));
                // A hint prepared from the previous folder is no longer valid.
                core2.cancel_hint();
                core2.shared.update(|s| {
                    s.topic = Some(topic.clone());
                    s.topic_loading = false;
                    s.pending = None;
                    s.hint = None;
                    s.docs_report = Some(crate::state::DocsReport { files, chunks: n });
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
