use crate::config::Config;
use crate::llm::Llm;
use crate::record::{SessionLog, SttTiming, WavWriter};
use crate::search::Index;
use crate::state::{HintStatus, Shared, Speaker};
use crate::usage::{Call, Outcome, Prices, Purpose, UsageMeter};
use crate::{audio, hint, realtime};
use anyhow::Result;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
    /// An utterance is final. `stt` says how long it took to become text (None: unknown).
    Completed { id: String, stt: Option<SttTiming> },
    Error(String),
    /// OpenAI reported what one transcribed utterance cost.
    TranscribeUsage { speaker: Speaker, input_tokens: u64, output_tokens: u64, seconds: f64 },
    /// A service or timing event: it goes to `events.jsonl`, and to `session.log` when `text` is not empty.
    Note { speaker: Speaker, tag: &'static str, kind: &'static str, text: String, fields: serde_json::Value },
}

/// Builds an [`Ev::Note`]. `tag` is the word before the speaker in `session.log` (CONN, AUDIO, STT).
pub fn note(speaker: Speaker, tag: &'static str, kind: &'static str, text: String, fields: serde_json::Value) -> Ev {
    Ev::Note { speaker, tag, kind, text, fields }
}

pub struct Core {
    pub cfg: Arc<Config>,
    pub shared: Shared,
    /// Index of the selected document folder; None until one is selected.
    pub index: RwLock<Option<Arc<Index>>>,
    pub llm: Llm,
    /// Paid requests of the current session (for the cost report).
    pub usage: Arc<UsageMeter>,
    /// Hint requests that are running; they never wait for or cancel each other.
    pub prep: Mutex<Vec<AbortHandle>>,
    /// Number of the last hint request started (they are numbered in the order they start).
    pub hint_seq: AtomicU64,
    /// How many hint requests are running now.
    pub hint_runs: AtomicUsize,
    /// Debug record of the current Listen session (text + audio).
    pub rec: Mutex<Option<Arc<SessionLog>>>,
}

impl Core {
    pub fn new(cfg: Arc<Config>, shared: Shared) -> Self {
        let usage = Arc::new(UsageMeter::new(Prices::new(&cfg.prices)));
        let llm = Llm::new(&cfg.llm, usage.clone());
        shared.update(|s| {
            audio::threshold_set(&s.meters[Speaker::Them.idx()].threshold, cfg.transcribe.silence_threshold);
            audio::threshold_set(&s.meters[Speaker::Me.idx()].threshold, cfg.transcribe.mic_silence_threshold);
        });
        Self { cfg, shared, index: RwLock::new(None), llm, usage, prep: Mutex::new(Vec::new()), hint_seq: AtomicU64::new(0), hint_runs: AtomicUsize::new(0), rec: Mutex::new(None) }
    }

    /// The record of the current session, if one is open.
    pub fn rec(&self) -> Option<Arc<SessionLog>> {
        self.rec.lock().unwrap().clone()
    }

    /// Stops every hint that is still being prepared or streamed (the dialog was cleared or the
    /// documents changed, so those hints are no longer wanted).
    pub fn cancel_hint(&self) {
        for h in self.prep.lock().unwrap().drain(..) {
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
                        s.clear_hints();
                        s.error = None;
                    });
                }
            },
            ev = ev_rx.recv() => match ev {
                Some(Ev::Completed { id, stt }) => on_completed(&core, &id, stt),
                Some(Ev::Note { speaker, tag, kind, text, mut fields }) => {
                    if session.is_some() {
                        if speaker == Speaker::Them && kind == "speech_started" {
                            on_them_speaking(&core);
                        }
                        if let Some(o) = fields.as_object_mut() {
                            o.insert("speaker".into(), if speaker == Speaker::Me { "me" } else { "them" }.into());
                        }
                        core.usage.event(kind, fields);
                        if let (Some(r), false) = (core.rec(), text.is_empty()) {
                            r.note(&format!("{tag} {}: {text}", if speaker == Speaker::Me { "ME" } else { "THEM" }));
                        }
                    }
                }
                Some(Ev::TranscribeUsage { speaker, input_tokens, output_tokens, seconds }) => {
                    if session.is_some() {
                        core.usage.record(Call {
                            purpose: if speaker == Speaker::Me { Purpose::TranscribeMe } else { Purpose::TranscribeThem },
                            model: core.cfg.transcribe.model.clone(),
                            input_tokens,
                            output_tokens,
                            audio_secs: seconds,
                            duration: std::time::Duration::ZERO,
                            outcome: Outcome::Won,
                            estimated: false,
                        });
                    }
                }
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
    core.usage.begin(rec.as_ref().map(|r| r.dir()));
    let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    core.usage.event("session_start", serde_json::json!({ "unix": unix, "topic": topic, "version": env!("CARGO_PKG_VERSION") }));
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
            spawn_capture(&core.shared, source, threshold, &stop, tx, None, None);
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
pub fn spawn_capture(
    shared: &Shared,
    source: audio::Source,
    threshold: audio::Threshold,
    stop: &Arc<AtomicBool>,
    audio_tx: UnboundedSender<Vec<u8>>,
    mut wav: Option<WavWriter>,
    events: Option<UnboundedSender<Ev>>,
) {
    let lost = match source {
        audio::Source::Speakers => "Audio device lost",
        audio::Source::Mic => "Microphone lost",
    };
    let (stop2, shared2) = (stop.clone(), shared.clone());
    let idx = meter_idx(source);
    let speaker = if source == audio::Source::Mic { Speaker::Me } else { Speaker::Them };
    std::thread::spawn(move || {
        let mut gate = audio::SilenceGate::new(threshold);
        let mut recovering = false;
        // The audio device is gone since then; only the first failure of an outage is reported.
        let mut lost_at: Option<Instant> = None;
        let send = |kind: &'static str, text: String, fields: serde_json::Value| {
            if let Some(tx) = &events {
                let _ = tx.send(note(speaker, "AUDIO", kind, text, fields));
            }
        };
        while !stop2.load(Ordering::Relaxed) {
            let res = audio::capture(source, &stop2, |c| {
                if recovering {
                    recovering = false;
                    if let Some(t) = lost_at.take() {
                        let off = t.elapsed();
                        send("audio_recovered", format!("{} recovered after {:.1} s", lost.to_lowercase(), off.as_secs_f64()), serde_json::json!({ "offline_ms": off.as_millis() as u64 }));
                    }
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
                    if lost_at.is_none() {
                        lost_at = Some(Instant::now());
                        send("audio_lost", format!("{} ({e})", lost.to_lowercase()), serde_json::json!({ "reason": e.to_string() }));
                    }
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
    spawn_capture(&core.shared, source, threshold, stop, audio_tx, wav, Some(ev_tx.clone()));

    // The chosen document folder is the topic of the call: it goes to the recognizer as context.
    let mut tcfg = core.cfg.transcribe.clone();
    let topic = core.shared.read(|s| s.topic.clone());
    tcfg.prompt = realtime::stt_prompt(&tcfg.language, topic.as_deref(), &tcfg.prompt);
    let (core2, ev2, key) = (core.clone(), ev_tx.clone(), key.to_string());
    tokio::spawn(async move {
        let shared = core2.shared.clone();
        if let Err(e) = realtime::run_with_reconnect(&tcfg, realtime::REALTIME_URL, &key, audio_rx, shared, ev2.clone(), speaker).await {
            // Only fatal errors (bad key, no access) get here; everything else is retried.
            let _ = ev2.send(Ev::Error(format!("Transcription: {e}")));
        }
    })
}

/// 24 kHz, 16-bit, mono.
const BYTES_PER_SEC: f64 = audio::RATE as f64 * 2.0;

/// Stops capture and closes the WebSocket: nothing is spent while stopped.
fn stop(session: &mut Option<Session>, core: &Arc<Core>) {
    let running = session.take();
    if let Some(s) = &running {
        s.stop.store(true, Ordering::Relaxed);
        for t in &s.tasks {
            t.abort();
        }
    }
    // Audio actually sent per source, read before the meters are touched again.
    let audio_sent = core.shared.read(|s| [Speaker::Them, Speaker::Me].map(|k| s.meters[k.idx()].bytes_sent as f64 / BYTES_PER_SEC));
    // Only a session that was really running has a cost to report.
    let cost = running.is_some().then(|| core.usage.summary(&core.cfg.transcribe.model, audio_sent));
    if let Some(r) = core.rec.lock().unwrap().take() {
        r.stopped();
        if let Some(c) = &cost {
            r.cost(c);
        }
    }
    core.shared.update(|s| {
        if s.hint_status != HintStatus::Writing && s.hint_status != HintStatus::Checking {
            s.hint_status = HintStatus::Idle;
        }
        if cost.is_some() {
            s.cost_open = true;
            s.cost_report = cost;
        }
        s.listening = false;
        s.listening_since = None;
        s.status = "Stopped".into();
        s.conn_error = None;
        for seg in &mut s.segments {
            seg.partial = false;
        }
    });
}

/// The other person started talking. Hints in progress are left alone: an answer that is already
/// being written is never thrown away because they went on talking.
fn on_them_speaking(core: &Arc<Core>) {
    if !core.cfg.hint_auto {
        return;
    }
    core.shared.update(|s| {
        if matches!(s.hint_status, HintStatus::Idle | HintStatus::NoQuestion | HintStatus::Error(_)) {
            s.hint_status = HintStatus::Listening;
        }
    });
}

fn on_completed(core: &Arc<Core>, item_id: &str, stt: Option<SttTiming>) {
    let found = core.shared.read(|s| {
        let idx = s.segments.iter().position(|x| x.item_id == item_id)?;
        let ctx: Vec<String> = s.segments[..idx].iter().rev().filter(|x| !x.partial).take(2).map(|x| x.es.clone()).collect();
        Some((s.segments[idx].speaker, s.segments[idx].es.clone(), ctx.into_iter().rev().collect::<Vec<_>>()))
    });
    let Some((speaker, es, ctx)) = found else { return };
    let rec = core.rec();

    // A sound without words: the recognizer returned a symbol or echoed its own prompt. It is not
    // shown, not translated and does not start a hint.
    let topic = core.shared.read(|s| s.topic.clone());
    let prompt = realtime::stt_prompt(&core.cfg.transcribe.language, topic.as_deref(), &core.cfg.transcribe.prompt);
    if realtime::is_junk(&es, &prompt) {
        core.shared.update(|s| {
            s.segments.retain(|x| x.item_id != item_id);
            // "They are speaking" was set by the sound that turned out to be nothing.
            if speaker == Speaker::Them && s.hint_status == HintStatus::Listening {
                s.hint_status = HintStatus::Idle;
            }
        });
        let who = if speaker == Speaker::Me { "ME" } else { "THEM" };
        core.usage.event("dropped", serde_json::json!({ "id": item_id, "speaker": who, "text": es }));
        if let Some(r) = &rec {
            r.note(&format!("DROPPED {who}: {}", es.trim()));
        }
        return;
    }

    if let Some(r) = &rec {
        r.utterance(item_id, speaker, &es, stt);
    }

    // The user spoke: no translation needed. Hints are never touched by it: the one on screen stays.
    if speaker == Speaker::Me {
        return;
    }

    // Translation does not block receiving the next utterances.
    let (core2, id) = (core.clone(), item_id.to_string());
    let es2 = es.clone();
    tokio::spawn(async move {
        let started = Instant::now();
        let res = core2.llm.translate(&core2.cfg.llm.translate_model, topic.as_deref(), &ctx, &es2).await;
        let took = started.elapsed();
        core2.usage.event(
            "translate",
            serde_json::json!({
                "id": id,
                "model": core2.cfg.llm.translate_model,
                "ms": took.as_millis() as u64,
                "chars_in": es2.chars().count(),
                "chars_out": res.as_ref().map_or(0, |t| t.0.chars().count()),
                "ok": res.is_ok(),
                "retries": res.as_ref().map_or(0, |(_, n)| *n),
            }),
        );
        let (ru, retries) = match res {
            Ok((t, n)) => (t, n),
            Err(e) => (format!("[translation error: {e}]"), 0),
        };
        if let Some(r) = &rec {
            r.translation(&id, &ru, took, retries);
        }
        core2.shared.update(|s| {
            if let Some(seg) = s.segments.iter_mut().find(|x| x.item_id == id) {
                seg.ru = Some(ru);
            }
        });
    });

    // After every phrase of theirs, with no waiting for silence: the model sees the hint on screen
    // and the whole recent talk, and decides whether to keep, refine or replace the hint.
    if core.cfg.hint_auto {
        core.shared.update(|s| {
            if s.hint_status != HintStatus::Writing {
                s.hint_status = HintStatus::Checking;
            }
        });
        hint::auto(core);
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
                    s.clear_hints();
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
