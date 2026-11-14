use crate::core::Core;
use crate::state::{Hint, Shared, Speaker};
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
            core2.shared.update(|s| s.error = Some(format!("Hint: {e}")));
        }
    });
    if let Some(old) = core.prep.lock().unwrap().replace(task.abort_handle()) {
        old.abort();
    }
}

async fn run(core: &Arc<Core>, show: bool) -> anyhow::Result<()> {
    let index = core.index.read().unwrap().clone().ok_or_else(|| anyhow::anyhow!("select a document folder first"))?;
    let window = Duration::from_secs(core.cfg.hint_window_secs);
    let lines: Vec<(Speaker, String)> = core.shared.read(|s| {
        s.segments.iter().filter(|x| x.at.elapsed() <= window).map(|x| (x.speaker, x.es.clone())).collect()
    });
    anyhow::ensure!(!lines.is_empty(), "no recognized speech in the last {} s", core.cfg.hint_window_secs);

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
    let found = index.search(&[(focus, 3.0), (&all, 1.0)], core.cfg.hint_chunks.max(1));

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

    let text = core
        .llm
        .hedged_stream(&core.cfg.llm.hint_models, messages, |raw| {
            hint.apply_raw(raw);
            publish(&core.shared, &hint, show);
        })
        .await?;
    hint.apply_raw(&text);
    hint.done = true;
    publish(&core.shared, &hint, show);
    if let Some(r) = core.rec() {
        r.hint(&hint, show);
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
