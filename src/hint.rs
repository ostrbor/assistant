use crate::core::Core;
use crate::record::HintTiming;
use crate::state::{Hint, Segment, Shared, Speaker};
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
Output exactly four lines and nothing else:\nSRC: <DOCS or OWN>\nQ: <the question, short, in English>\nES: <the answer in Spanish>\nEN: <English translation of the answer>";

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

pub fn is_question(es: &str) -> bool {
    let t = es.trim().to_lowercase();
    if t.contains('?') || t.contains('¿') {
        return true;
    }
    const W: &[&str] = &["qué ", "que ", "cómo ", "como ", "cuándo ", "cuando ", "dónde ", "donde ", "por qué", "cuál ", "cuáles", "quién ", "cuánto", "cuántos", "cuántas", "puede ", "podría ", "tiene "];
    W.iter().any(|w| t.starts_with(w))
}

/// Hint button: show the one prepared in the background or generate it now.
pub fn ask(core: &Arc<Core>) {
    let max_age = Duration::from_secs(core.cfg.hint_max_age_secs);
    let (fresh, on_screen) = core.shared.read(|s| {
        (
            s.pending.as_ref().is_some_and(|h| h.started.is_some_and(|t| t.elapsed() < max_age)),
            // The finished hint is already on screen: pressing the button again asks for a new one.
            s.pending_shown && s.hint.as_ref().is_some_and(|h| h.done),
        )
    });
    if fresh && !on_screen {
        let age = core.shared.read(|s| s.pending.as_ref().and_then(|h| h.started).map(|t| t.elapsed()));
        if let Some(a) = age {
            core.usage.event("hint_shown", json!({ "prepared_ago_ms": a.as_millis() as u64 }));
            if let Some(r) = core.rec() {
                r.note(&format!("HINT shown (prepared {:.1} s ago)", a.as_secs_f64()));
            }
        }
        core.shared.update(|s| {
            s.pending_shown = true;
            s.hint = s.pending.clone();
        });
    } else {
        generate(core, true);
    }
}

/// Background preparation when a question is detected.
pub fn prepare(core: &Arc<Core>) {
    generate(core, false);
}

fn generate(core: &Arc<Core>, show: bool) {
    if !show {
        // A new background preparation must not overwrite the hint already on screen.
        core.shared.update(|s| s.pending_shown = false);
    }
    let core2 = core.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = run(&core2, show).await {
            if let Some(r) = core2.rec() {
                r.error(&format!("Hint: {e}"));
            }
            core2.usage.event("hint_error", json!({ "requested": show, "error": e.to_string() }));
            // The user never asked for a background hint: its failure goes to the log only.
            if show {
                core2.shared.update(|s| s.error = Some(format!("Hint: {e}")));
            }
        }
    });
    if let Some(old) = core.prep.lock().unwrap().replace(task.abort_handle()) {
        old.abort();
    }
}

async fn run(core: &Arc<Core>, show: bool) -> anyhow::Result<()> {
    let index = core.index.read().unwrap().clone().ok_or_else(|| anyhow::anyhow!("select a document folder first"))?;
    let window = Duration::from_secs(core.cfg.hint_window_secs);
    let t0 = Instant::now();
    core.usage.event("hint_start", json!({ "requested": show }));
    let lines = core.shared.read(|s| pick_lines(&s.segments, window));
    if lines.is_empty() {
        // Nothing has been said yet: say so in the hint panel, not as an error.
        if show {
            let raw = "Nothing has been said yet. Start the conversation, then press Hint.".to_string();
            core.shared.update(|s| {
                s.pending_shown = true;
                s.hint = Some(Hint { raw, done: true, ..Default::default() });
            });
        }
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
    let messages = json!([{ "role": "system", "content": SYSTEM }, { "role": "user", "content": user }]);

    let mut hint = Hint { sources, started: Some(Instant::now()), ..Default::default() };
    publish(&core.shared, &hint, show);

    let mut first_token = None;
    let streamed = core
        .llm
        .hedged_stream(&core.cfg.llm.hint_models, messages, |raw| {
            first_token.get_or_insert_with(|| t0.elapsed());
            hint.apply_raw(raw);
            publish(&core.shared, &hint, show);
        })
        .await?;
    hint.apply_raw(&streamed.text);
    hint.done = true;
    publish(&core.shared, &hint, show);
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
            "chars": hint.es.chars().count() + hint.en.chars().count(),
        }),
    );
    if let Some(r) = core.rec() {
        r.hint(&hint, show, &timing);
    }
    Ok(())
}

fn publish(shared: &Shared, h: &Hint, show: bool) {
    shared.update(|s| {
        s.pending = Some(h.clone());
        if show {
            s.pending_shown = true;
        }
        // If the user pressed the button while the hint is still streaming, show the updates.
        if s.pending_shown {
            s.hint = Some(h.clone());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(text: &str, age_secs: u64) -> Segment {
        let at = Instant::now().checked_sub(Duration::from_secs(age_secs)).expect("clock");
        Segment { item_id: text.into(), speaker: Speaker::Them, es: text.into(), en: None, partial: false, at }
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
