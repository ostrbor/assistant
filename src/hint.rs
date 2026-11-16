use crate::core::Core;
use crate::record::HintTiming;
use crate::state::{Hint, HintStatus, Segment, Shared, Speaker};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SYSTEM: &str = "You help the user answer questions in a Spanish-language video call. The user speaks Spanish only at A1-A2 level. \
You receive the recent transcript of the call, where lines starting with 'Them:' are the other person and lines starting with 'Me:' are the user, and excerpts from the user's own documents. \
Find the last question from Them that the user (Me) has not fully answered yet and answer it in the first person. \
If the user already answered it, suggest a short natural addition or continuation that does not repeat what the user said. \
Use 2-3 very short, simple Spanish sentences (A1-A2: present tense, basic vocabulary, no subjunctive). \
If the excerpts contain the answer, base the answer ONLY on them and write 'SRC: DOCS'. \
If they do not contain it (or only a part of it), NEVER say that you have no information: answer the question yourself from your general knowledge, correctly and briefly, and write 'SRC: OWN'. \
For personal facts about the user that are not in the excerpts (employers, dates, numbers, names, years of experience) do not invent specifics: give a plain generic answer and write 'SRC: OWN'. \
Output exactly four lines and nothing else:\nSRC: <DOCS or OWN>\nQ: <the question, short, in Russian>\nES: <the answer in Spanish>\nRU: <Russian translation of the answer>";

/// Added to the prompt of automatic hints, which fire after every turn of the other person.
const AUTO_RULE: &str = "\nThis request is automatic, made after every turn of Them. Look at the latest turn of Them: all 'Them:' lines after the last 'Me:' line. \
If that turn contains a question or request addressed to the user that the user has not answered yet, answer it, even if Them added an explanation, example or comment after the question. \
Output exactly the single word NONE and nothing else only when the whole turn has no such question (small talk, thanks, an acknowledgement, or they are still explaining before asking anything).";

/// How a hint was started.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// After a turn of the other person, without the user asking; the model may answer NONE.
    Auto,
    /// The Hint button: always produces a hint.
    Requested,
}

fn system_prompt(mode: Mode) -> String {
    match mode {
        Mode::Auto => format!("{SYSTEM}{AUTO_RULE}"),
        Mode::Requested => SYSTEM.to_string(),
    }
}

/// With nothing said inside the window, the hint still works from the last few phrases.
const FALLBACK_LINES: usize = 8;

/// The phrases a hint is built from: those inside `window`, or else the last `FALLBACK_LINES`.
fn pick_lines(segments: &[Segment], window: Duration) -> Vec<(Speaker, String)> {
    let lines = |it: &mut dyn Iterator<Item = &Segment>| it.map(|x| (x.speaker, x.es.clone())).collect::<Vec<_>>();
    let recent = lines(&mut segments.iter().filter(|x| x.at.elapsed() <= window));
    if !recent.is_empty() {
        return recent;
    }
    let from = segments.len().saturating_sub(FALLBACK_LINES);
    lines(&mut segments[from..].iter())
}

/// Does this phrase look like a question or a request? Only steers the document search: whether a
/// hint is shown is decided by the model.
pub fn is_question(es: &str) -> bool {
    let t = es.trim().to_lowercase();
    if t.contains('?') || t.contains('¿') {
        return true;
    }
    const W: &[&str] = &[
        "qué ", "cómo ", "cuándo ", "dónde ", "por qué", "cuál ", "cuáles", "quién ", "cuánto", "cuántos", "cuántas", "puede ", "puedes ", "podría ", "podrías ", "tiene ", "tienes ",
        "cuéntame", "cuéntanos", "háblame", "hábleme", "dime", "dígame", "explícame", "descríbeme", "describe ", "me gustaría saber", "quisiera saber", "quiero saber",
    ];
    W.iter().any(|w| t.starts_with(w) || t.contains(&format!(" {w}")))
}

/// Hint button: generate a hint now and show it as it streams.
pub fn ask(core: &Arc<Core>) {
    generate(core, Mode::Requested);
}

/// After a turn of the other person: the model decides whether there is a question to answer.
pub fn auto(core: &Arc<Core>) {
    if !core.cfg.hint_auto {
        return;
    }
    // A hint the user is reading is still being written: do not cut it off.
    if core.shared.read(|s| s.hint.as_ref().is_some_and(|h| !h.done)) {
        return;
    }
    generate(core, Mode::Auto);
}

fn generate(core: &Arc<Core>, mode: Mode) {
    let core2 = core.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = run(&core2, mode).await {
            if let Some(r) = core2.rec() {
                r.error(&format!("Hint: {e}"));
            }
            core2.usage.event("hint_error", json!({ "requested": mode == Mode::Requested, "error": e.to_string() }));
            core2.shared.update(|s| {
                // Do not leave a half-written hint with a spinner.
                match s.hint.as_mut() {
                    Some(h) if !h.done && h.has_content() => h.done = true,
                    Some(h) if !h.done => s.hint = None,
                    _ => {}
                }
                match mode {
                    Mode::Requested => {
                        s.hint_status = HintStatus::Idle;
                        s.error = Some(format!("Hint: {e}"));
                    }
                    Mode::Auto => s.hint_status = HintStatus::Error(format!("Hint: {e}")),
                }
            });
        }
    });
    if let Some(old) = core.prep.lock().unwrap().replace(task.abort_handle()) {
        old.abort();
    }
}

async fn run(core: &Arc<Core>, mode: Mode) -> anyhow::Result<()> {
    let index = core.index.read().unwrap().clone().ok_or_else(|| anyhow::anyhow!("select a document folder first"))?;
    let window = Duration::from_secs(core.cfg.hint_window_secs);
    let t0 = Instant::now();
    let show = mode == Mode::Requested;
    core.shared.update(|s| s.hint_status = if show { HintStatus::Writing } else { HintStatus::Checking });
    core.usage.event("hint_start", json!({ "requested": show }));
    let lines = core.shared.read(|s| pick_lines(&s.segments, window));
    if lines.is_empty() {
        // Nothing has been said yet: say so in the hint panel, not as an error.
        if show {
            let raw = "Nothing has been said yet. Start the conversation, then press Hint.".to_string();
            core.shared.update(|s| s.hint = Some(Hint { raw, done: true, ..Default::default() }));
        }
        core.shared.update(|s| s.hint_status = HintStatus::Idle);
        return Ok(());
    }

    let transcript: Vec<String> = lines
        .iter()
        .map(|(sp, t)| format!("{}: {t}", if *sp == Speaker::Me { "Me" } else { "Them" }))
        .collect();
    // Search is driven by what the other person said, not by the user's own answers.
    let them: Vec<&str> = lines.iter().filter(|(sp, _)| *sp == Speaker::Them).map(|(_, t)| t.as_str()).collect();
    let focus = them
        .iter()
        .rev()
        .find(|t| is_question(t))
        .or(them.last())
        .copied()
        .unwrap_or(lines.last().unwrap().1.as_str());
    let all = if them.is_empty() { focus.to_string() } else { them.join(" ") };
    let searched = Instant::now();
    let found = index.search(&[(focus, 3.0), (&all, 1.0)], core.cfg.hint_chunks.max(1));
    let search = searched.elapsed();
    core.usage.event("hint_search", json!({ "ms": search.as_millis() as u64, "chunks": found.len(), "lines": lines.len() }));

    let mut sources: Vec<String> = Vec::new();
    let mut excerpts = String::new();
    for (_, c) in &found {
        excerpts.push_str(&format!("[{} p.{}]\n{}\n\n", c.file, c.page, c.text));
        let tag = format!("{} p.{}", c.file, c.page);
        if !sources.contains(&tag) {
            sources.push(tag);
        }
    }
    let user = format!("Transcript (latest last):\n{}\n\nDocument excerpts:\n{}", transcript.join("\n"), excerpts);
    let messages = json!([{ "role": "system", "content": system_prompt(mode) }, { "role": "user", "content": user }]);

    let mut hint = Hint { sources, started: Some(Instant::now()), ..Default::default() };
    publish(&core.shared, &hint, mode);

    let mut first_token = None;
    let streamed = core
        .llm
        .hedged_stream(&core.cfg.llm.hint_models, messages, |raw| {
            first_token.get_or_insert_with(|| t0.elapsed());
            hint.apply_raw(raw);
            publish(&core.shared, &hint, mode);
        })
        .await?;
    hint.apply_raw(&streamed.text);
    hint.done = true;
    if hint.is_none() && !show {
        // No question in what they said: keep the hint that is on screen and say so.
        let last = lines.iter().rev().find(|(sp, _)| *sp == Speaker::Them).map_or("", |(_, t)| t.as_str());
        core.shared.update(|s| s.hint_status = HintStatus::NoQuestion);
        core.usage.event("hint_none", json!({ "model": streamed.model, "total_ms": t0.elapsed().as_millis() as u64, "last": last }));
        if let Some(r) = core.rec() {
            r.note(&format!("HINT none · {} · {:.1} s · no question in: {last}", streamed.model, t0.elapsed().as_secs_f64()));
        }
        return Ok(());
    }
    publish(&core.shared, &hint, mode);
    core.shared.update(|s| s.hint_status = HintStatus::Idle);
    let timing = HintTiming { model: streamed.model, search, first_token, total: t0.elapsed() };
    core.usage.event(
        "hint_done",
        json!({
            "requested": show,
            "model": timing.model,
            "search_ms": search.as_millis() as u64,
            "first_token_ms": timing.first_token.map(|d| d.as_millis() as u64),
            "total_ms": timing.total.as_millis() as u64,
            "own": hint.own,
            "chars": hint.es.chars().count() + hint.ru.chars().count(),
        }),
    );
    if let Some(r) = core.rec() {
        r.hint(&hint, mode, &timing);
    }
    Ok(())
}

/// Puts the hint on screen. An automatic hint stays hidden (the previous one remains) until the
/// model has written something, so a NONE answer never wipes the screen.
fn publish(shared: &Shared, h: &Hint, mode: Mode) {
    if mode == Mode::Auto && !h.has_content() && !h.done {
        return;
    }
    shared.update(|s| {
        s.hint = Some(h.clone());
        if s.hint_status == HintStatus::Checking {
            s.hint_status = HintStatus::Writing;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(text: &str, age_secs: u64) -> Segment {
        let at = Instant::now().checked_sub(Duration::from_secs(age_secs)).expect("clock");
        Segment { item_id: text.into(), speaker: Speaker::Them, es: text.into(), ru: None, partial: false, at }
    }

    #[test]
    fn requests_and_questions_are_recognised() {
        assert!(is_question("¿Cuánto tiempo trabajaste allí?"));
        assert!(is_question("Cuéntame sobre tu experiencia"));
        assert!(is_question("Vale, y me gustaría saber por qué quieres el puesto"));
        assert!(is_question("Dime algo sobre ti"));
        assert!(!is_question("Perfecto, gracias."));
        assert!(!is_question("Dijo que sí"));
    }

    #[test]
    fn recent_lines_win_over_the_fallback() {
        let segs = [seg("old", 300), seg("new", 5)];
        let got: Vec<String> = pick_lines(&segs, Duration::from_secs(60)).into_iter().map(|l| l.1).collect();
        assert_eq!(got, ["new"]);
    }

    #[test]
    fn old_speech_still_gives_the_last_lines() {
        let segs: Vec<Segment> = (0..12).map(|i| seg(&format!("l{i}"), 300 + i)).collect();
        let got = pick_lines(&segs, Duration::from_secs(60));
        assert_eq!(got.len(), FALLBACK_LINES);
        assert_eq!(got.last().unwrap().1, "l11");
        assert!(pick_lines(&[], Duration::from_secs(60)).is_empty());
    }
}
