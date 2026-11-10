use crate::core::Core;
use crate::state::Hint;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SYSTEM: &str = "You help the user answer questions in a Spanish-language video call. The user speaks Spanish only at A1-A2 level. \
You receive the recent transcript of the call (the other person speaks to the user) and excerpts from the user's own documents. \
Find the last question addressed to the user and answer it in the first person, using ONLY facts from the excerpts. \
Use 2-3 very short, simple Spanish sentences (A1-A2: present tense, basic vocabulary, no subjunctive). \
If the excerpts do not contain the answer, say so in simple Spanish (for example: 'No tengo esa informacion ahora. Lo reviso y le respondo.'). \
Output exactly three lines and nothing else:\nQ: <the question, short, in English>\nES: <the answer in Spanish>\nEN: <English translation of the answer>";

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
    let used = core.shared.read(|s| s.pending.as_ref().is_some_and(|h| h.started.is_some_and(|t| t.elapsed() < max_age)));
    if used {
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
    let lines: Vec<(String, bool)> = core.shared.read(|s| {
        s.segments.iter().filter(|x| x.at.elapsed() <= window).map(|x| (x.es.clone(), x.partial)).collect()
    });
    anyhow::ensure!(!lines.is_empty(), "no recognized speech in the last {} s", core.cfg.hint_window_secs);

    let transcript: Vec<&str> = lines.iter().map(|(t, _)| t.as_str()).collect();
    let focus = lines.iter().rev().find(|(t, _)| is_question(t)).map(|(t, _)| t.as_str()).unwrap_or(*transcript.last().unwrap());
    let all = transcript.join(" ");
    let found = index.search(&[(focus, 3.0), (&all, 1.0)], 5);

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

    let started = Instant::now();
    let mut hint = Hint { sources, started: Some(started), ..Default::default() };
    publish(core, &hint, show);

    let shared = core.shared.clone();
    let mut cur = hint.clone();
    let text = core
        .llm
        .hedged_stream(&core.cfg.llm.hint_models, messages, |raw| {
            cur.apply_raw(raw);
            publish_with(&shared, &cur, show);
        })
        .await?;
    hint.apply_raw(&text);
    hint.done = true;
    publish(core, &hint, show);
    Ok(())
}

fn publish(core: &Arc<Core>, h: &Hint, show: bool) {
    publish_with(&core.shared, h, show);
}

fn publish_with(shared: &crate::state::Shared, h: &Hint, show: bool) {
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
