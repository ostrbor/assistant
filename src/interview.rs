//! Interviewer simulator (`--interview`): plays the other side of a call.
//!
//! It speaks into a virtual microphone (a PulseAudio null sink) that Zoom/Meet uses as its
//! microphone, and listens to the call through the default output's monitor, transcribing your
//! answers with the same realtime client the assistant uses.

use crate::config::{Config, Interview};
use crate::llm::Llm;
use crate::state::{Shared, Speaker};
use crate::{audio, core, docs, realtime};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::unbounded_channel;
use tokio::time::sleep;

/// Sink the interviewer speaks into; its monitor is exposed as `interviewer_mic`.
const SINK: &str = "interviewer_out";
const MIC: &str = "interviewer_mic";
/// Characters of document text given to the interviewer.
const DOC_CHARS: usize = 12_000;
/// How long to wait for the candidate to start answering.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// Upper bound for one answer.
const MAX_ANSWER: Duration = Duration::from_secs(180);

/// Creates the virtual sink and microphone if they do not exist yet.
fn setup_audio() -> Result<()> {
    audio::ensure_virtual_devices(SINK, MIC)
}

/// What the interviewer has to do on turn `turn` (0 = opening, `n` = closing).
fn turn_instruction(turn: usize, n: usize) -> String {
    if turn == 0 {
        format!("Turn 0 of {n}: greet the candidate, introduce yourself in one sentence and ask question 1.")
    } else if turn < n {
        format!(
            "Turn {turn} of {n}: react to the candidate's last answer in one short sentence (acknowledge it, no long comments), \
             then ask question {} on a different topic from the previous ones.",
            turn + 1
        )
    } else {
        "Final turn: react to the last answer in one short sentence, thank the candidate and say goodbye. Do not ask any more questions.".into()
    }
}

fn build_messages(persona: &str, docs_text: &str, history: &[String], turn: usize, n: usize) -> Value {
    let system = format!(
        "{persona}\n\nYou are the interviewer in a live video call with a candidate. Speak only Spanish, in simple, clear sentences (level A2-B1). \
         Say at most 2-3 short sentences per turn and ask exactly one question at a time. Never answer on behalf of the candidate and never \
         repeat a question. Base your questions on the documents below (the candidate's CV, the project or the job offer). \
         Output only the words you will say aloud: no names or labels, no lists, no markdown, no stage directions.\n\nDocuments:\n{docs_text}"
    );
    let user = format!("Conversation so far:\n{}\n\n{}", if history.is_empty() { "(nothing yet)".into() } else { history.join("\n") }, turn_instruction(turn, n));
    json!([{ "role": "system", "content": system }, { "role": "user", "content": user }])
}

/// Removes a leading speaker label and surrounding quotes/markdown from a model reply.
fn clean_reply(raw: &str) -> String {
    let t = raw.trim().trim_matches(|c| c == '"' || c == '*' || c == '«' || c == '»').trim();
    for label in ["Interviewer:", "Entrevistador:", "Entrevistadora:"] {
        if let Some(r) = t.strip_prefix(label) {
            return r.trim().to_string();
        }
    }
    t.to_string()
}

/// An answer is finished once something was said, nothing is being transcribed any more,
/// and it has been quiet for `silence`.
fn answer_finished(completed: usize, partial: bool, quiet_for: Duration, silence: Duration) -> bool {
    completed > 0 && !partial && quiet_for >= silence
}

async fn tts(http: &reqwest::Client, key: &str, cfg: &Interview, text: &str) -> Result<Vec<u8>> {
    let res = http
        .post("https://api.openai.com/v1/audio/speech")
        .bearer_auth(key)
        .json(&json!({
            "model": cfg.tts_model,
            "voice": cfg.voice,
            "input": text,
            "instructions": cfg.tts_instructions,
            "response_format": "pcm",
        }))
        .send()
        .await?;
    let status = res.status();
    if !status.is_success() {
        let body = res.text().await.unwrap_or_default();
        bail!("TTS {status}: {}", body.chars().take(200).collect::<String>());
    }
    Ok(res.bytes().await?.to_vec())
}

async fn say(http: &reqwest::Client, key: &str, cfg: &Interview, text: &str) -> Result<()> {
    println!("\n[Interviewer] {text}");
    let pcm = tts(http, key, cfg, text).await?;
    tokio::task::spawn_blocking(move || audio::play(SINK, &pcm)).await??;
    Ok(())
}

/// Waits for the candidate to answer. Returns the transcript (empty if nothing was said).
/// `seen` holds segments of earlier turns; the ones consumed here are added to it.
async fn listen(shared: &Shared, seen: &mut HashSet<String>, silence: Duration, timeout: Duration) -> String {
    let started = Instant::now();
    let mut last_change = Instant::now();
    let mut snapshot = (0usize, 0usize);
    let mut first_activity: Option<Instant> = None;
    loop {
        sleep(Duration::from_millis(200)).await;
        let (completed, partial, chars, text, ids) = shared.read(|s| {
            let new: Vec<_> = s.segments.iter().filter(|x| x.speaker == Speaker::Them && !seen.contains(&x.item_id)).collect();
            (
                new.iter().filter(|x| !x.partial).count(),
                new.iter().any(|x| x.partial),
                new.iter().map(|x| x.es.len()).sum::<usize>(),
                new.iter().filter(|x| !x.partial).map(|x| x.es.as_str()).collect::<Vec<_>>().join(" "),
                new.iter().map(|x| x.item_id.clone()).collect::<Vec<_>>(),
            )
        });
        if (ids.len(), chars) != snapshot {
            snapshot = (ids.len(), chars);
            last_change = Instant::now();
            first_activity.get_or_insert_with(Instant::now);
        }
        let done = answer_finished(completed, partial, last_change.elapsed(), silence)
            || first_activity.is_some_and(|t| t.elapsed() > MAX_ANSWER)
            || (first_activity.is_none() && started.elapsed() > timeout);
        if done {
            seen.extend(ids);
            return text;
        }
    }
}

pub async fn run(cfg: &Config, topic: &str, persona_override: Option<String>, questions: Option<usize>) -> Result<()> {
    let icfg = &cfg.interview;
    let persona = persona_override.unwrap_or_else(|| icfg.persona.clone());
    let n = questions.unwrap_or(icfg.questions).max(1);
    let key = cfg.transcribe.key()?;

    let dir = docs::topic_dir(&cfg.docs_dir, topic)?;
    let chunks = tokio::task::spawn_blocking(move || docs::load_chunks(&dir)).await??.0;
    anyhow::ensure!(!chunks.is_empty(), "no text found in docs/{topic} (scanned PDFs are not supported)");
    let mut docs_text = String::new();
    for c in &chunks {
        // Bytes >= chars, so this may stop slightly early; the exact cut is made below.
        if docs_text.len() >= DOC_CHARS {
            break;
        }
        docs_text.push_str(&format!("[{}]\n{}\n\n", c.file, c.text));
    }
    let docs_text: String = docs_text.chars().take(DOC_CHARS).collect();

    setup_audio()?;
    println!("Virtual microphone ready. In Zoom / Meet choose the microphone \"Interviewer_mic\".");
    println!("They disappear when the sound server restarts (or log out); to remove them now: pactl unload-module module-remap-source && pactl unload-module module-null-sink");
    println!("Persona: {persona}\nQuestions: {n}\n");
    println!("Join the call, then press Enter to start.");
    tokio::task::spawn_blocking(|| std::io::stdin().read_line(&mut String::new())).await??;

    // Listen to the call: default output monitor → realtime transcription (headless).
    let shared = Shared::new(None);
    let stop = Arc::new(AtomicBool::new(false));
    let (audio_tx, audio_rx) = unbounded_channel::<Vec<u8>>();
    core::spawn_capture(&shared, audio::Source::Speakers, audio::threshold(cfg.transcribe.silence_threshold), &stop, audio_tx, None, None);
    let (ev_tx, _ev_rx) = unbounded_channel();
    let transcribe = {
        let mut tcfg = cfg.transcribe.clone();
        tcfg.prompt = realtime::stt_prompt(&tcfg.language, None, &tcfg.prompt);
        let (shared, key) = (shared.clone(), key.clone());
        tokio::spawn(async move { realtime::run_with_reconnect(&tcfg, realtime::REALTIME_URL, &key, audio_rx, shared, ev_tx, Speaker::Them).await })
    };

    let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(5)).timeout(Duration::from_secs(60)).build()?;
    // The simulator's own requests are not part of any session cost report.
    let llm = Llm::new(&cfg.llm, std::sync::Arc::new(crate::usage::UsageMeter::new(crate::usage::Prices::new(&cfg.prices))));
    let silence = Duration::from_secs_f64(icfg.answer_silence_secs);
    let mut seen: HashSet<String> = HashSet::new();
    let mut history: Vec<String> = Vec::new();

    let result: Result<()> = async {
        for turn in 0..=n {
            if transcribe.is_finished() {
                bail!("transcription stopped (check OPENAI_API_KEY and access to the Realtime API)");
            }
            let messages = build_messages(&persona, &docs_text, &history, turn, n);
            let reply = clean_reply(&llm.hedged_stream(&cfg.llm.hint_models, messages, |_| {}).await?.text);
            anyhow::ensure!(!reply.is_empty(), "the model returned an empty reply");
            say(&http, &key, icfg, &reply).await?;
            history.push(format!("Interviewer: {reply}"));
            if turn == n {
                break;
            }

            let mut answer = listen(&shared, &mut seen, silence, START_TIMEOUT).await;
            if answer.is_empty() {
                say(&http, &key, icfg, "¿Me escuchas? Te repito la pregunta, por favor.").await?;
                answer = listen(&shared, &mut seen, silence, START_TIMEOUT).await;
            }
            if answer.is_empty() {
                answer = "(no answer)".into();
            }
            println!("[Candidate] {answer}");
            history.push(format!("Candidate: {answer}"));
        }
        Ok(())
    }
    .await;

    stop.store(true, Ordering::Relaxed);
    transcribe.abort();
    result?;
    println!("\nInterview finished.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_finishes_only_after_speech_and_quiet() {
        let s = Duration::from_secs(3);
        assert!(!answer_finished(0, false, Duration::from_secs(10), s), "nothing said yet");
        assert!(!answer_finished(1, true, Duration::from_secs(10), s), "still transcribing");
        assert!(!answer_finished(1, false, Duration::from_secs(2), s), "not quiet long enough");
        assert!(answer_finished(2, false, Duration::from_secs(3), s));
    }

    #[test]
    fn instructions_follow_the_turns() {
        assert!(turn_instruction(0, 5).contains("greet"));
        assert!(turn_instruction(1, 5).contains("question 2"));
        assert!(turn_instruction(4, 5).contains("question 5"));
        assert!(turn_instruction(5, 5).contains("goodbye"));
    }

    #[test]
    fn messages_contain_persona_docs_and_history() {
        let m = build_messages("Eres Pedro.", "DOC TEXT", &["Interviewer: Hola".into(), "Candidate: Buenas".into()], 1, 5);
        let sys = m[0]["content"].as_str().unwrap();
        let user = m[1]["content"].as_str().unwrap();
        assert!(sys.contains("Eres Pedro.") && sys.contains("DOC TEXT"));
        assert!(user.contains("Candidate: Buenas") && user.contains("question 2"));
    }

    #[test]
    fn reply_is_cleaned() {
        assert_eq!(clean_reply("  \"Hola, ¿qué tal?\"  "), "Hola, ¿qué tal?");
        assert_eq!(clean_reply("Entrevistadora: Hola"), "Hola");
        assert_eq!(clean_reply("Hola"), "Hola");
    }
}
