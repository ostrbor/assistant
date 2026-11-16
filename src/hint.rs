use crate::core::Core;
use crate::record::HintTiming;
use crate::state::{Addendum, Hint, HintStatus, PREV_HINTS, Segment, Shared, Speaker, Verdict};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const SYSTEM: &str = "You help the user answer questions in a Spanish-language video call. The user speaks Spanish only at A1-A2 level. \
You receive the recent transcript of the call, where lines starting with 'Them:' are the other person and lines starting with 'Me:' are the user, and excerpts from the user's own documents. \
Find the last question from Them that the user (Me) has not fully answered yet and answer it in the first person. \
If the user already answered it, suggest a short natural addition or continuation that does not repeat what the user said. \
Write the Spanish the way a beginner foreigner speaks: 2-3 very short, simple sentences (A1-A2: present tense, basic vocabulary, no subjunctive), as few words as possible, and NEVER use articles (no el, la, los, las, un, una, unos, unas; write 'a' instead of 'al' and 'de' instead of 'del'), e.g. 'Trabajo en banco desde dos años' and not 'Trabajo en el banco desde hace dos años'. \
In ES and in RU wrap the 2-4 most important words or short phrases (the facts of the answer: terms, numbers, names, the main verb) in **double asterisks**, the same ideas in both lines, so that one glance at the bold words shows what the answer is about. Use no other markup, and none in Q. \
If the excerpts contain the answer, base the answer ONLY on them and write 'SRC: DOCS'. \
If they do not contain it (or only a part of it), NEVER say that you have no information: answer the question yourself from your general knowledge, correctly and briefly, and write 'SRC: OWN'. \
For personal facts about the user that are not in the excerpts (employers, dates, numbers, names, years of experience) do not invent specifics: give a plain generic answer and write 'SRC: OWN'. \
Q must be a question or request that Them really asked in the transcript. An offer or a plan ('I can show you a matrix') is not a question. \
Never invent what you cannot see: if the answer needs something shown on a screen (a matrix, a picture, a formula) that is not in the transcript, do not guess, write in ES a polite request to show or read it, e.g. '¿Puede mostrar matriz, por favor?'. \
If the question is so badly recognised that you cannot tell what is asked, write in ES '¿Puede repetir pregunta, por favor?'. \
The user message names the topic of the call: write the Russian lines with the standard Russian terms of that field. \
Output exactly four lines and nothing else:\nSRC: <DOCS or OWN>\nQ: <the question, short, in Russian>\nES: <the answer in Spanish>\nRU: <Russian translation of the answer>";

/// Added to the prompt of automatic hints. They are asked after every phrase of Them, and the model
/// sees the hint that is on screen now, so it decides whether that hint still fits.
const AUTO_RULE: &str = "\nThis request is automatic: it is made after every phrase of Them, and the user is already reading the 'Current hint on screen' (if there is one). \
Compare the latest phrases of Them (all 'Them:' lines after the last 'Me:' line) with the current hint and start your answer with exactly one word on its own first line:\n\
KEEP - nothing to change. Use it when the latest phrases are filler, thanks, praise, a goodbye, an acknowledgement, small talk or talk to somebody else (\"muchas gracias\", \"perfecto\", \"hasta luego\" never start a new hint, even if an earlier question is still unanswered); when Them only repeat or rephrase the question the current hint already answers, without new information; when Them are still explaining and have not asked anything yet; or when the user has already fully answered. Output only the word KEEP and nothing else.\n\
UPDATE - the latest phrases are about the same question as the current hint, but they add an explanation, a narrowing, an example or a rephrasing that calls for more in the answer. Write UPDATE, then the four lines below, but ES and RU hold ONLY the addition: one or two very short Spanish sentences that continue the current answer with what is new, in the same style (no articles, key words in **double asterisks**). The user may be reading the current hint aloud right now, so never repeat or reword it. Q is the refined question.\n\
NEW - Them asked a different question or made a different request that the user has not answered. Write NEW, then the four lines below. Use NEW too when there is no current hint and a question is unanswered.\n\
When in doubt between KEEP and UPDATE, choose UPDATE; when in doubt between UPDATE and NEW, choose NEW. Never answer KEEP when there is an unanswered question and no current hint.";

/// How a hint was started.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// After a phrase of the other person, without the user asking; the model may answer KEEP.
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

/// After a phrase of the other person: the model decides whether the hint on screen still fits.
/// Requests never wait for each other and are never cancelled by new speech.
pub fn auto(core: &Arc<Core>) {
    if core.cfg.hint_auto {
        generate(core, Mode::Auto);
    }
}

/// Counts a running request, however it ends (finished, failed or aborted).
struct RunGuard(Arc<Core>);

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.0.hint_runs.fetch_sub(1, Ordering::Relaxed);
    }
}

fn generate(core: &Arc<Core>, mode: Mode) {
    // Requests are numbered in the order they start; a late answer of an older one never wins.
    let seq = core.hint_seq.fetch_add(1, Ordering::Relaxed) + 1;
    core.hint_runs.fetch_add(1, Ordering::Relaxed);
    let core2 = core.clone();
    let task = tokio::spawn(async move {
        let _guard = RunGuard(core2.clone());
        if let Err(e) = run(&core2, mode, seq).await {
            if let Some(r) = core2.rec() {
                r.error(&format!("Hint: {e}"));
            }
            core2.usage.event("hint_error", json!({ "seq": seq, "requested": mode == Mode::Requested, "error": e.to_string() }));
            core2.shared.update(|s| {
                // Do not leave a half-written hint with a spinner (only if it is this request's).
                if s.hint_owner == seq
                    && let Some(h) = s.hint.as_mut()
                {
                    h.done = true;
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
    let mut prep = core.prep.lock().unwrap();
    prep.retain(|h| !h.is_finished());
    prep.push(task.abort_handle());
}

/// Sets the final status, unless other requests are still running (the last one to end sets it).
fn settle(core: &Core, status: HintStatus) {
    if core.hint_runs.load(Ordering::Relaxed) <= 1 {
        core.shared.update(|s| s.hint_status = status);
    }
}

async fn run(core: &Arc<Core>, mode: Mode, seq: u64) -> anyhow::Result<()> {
    let index = core.index.read().unwrap().clone().ok_or_else(|| anyhow::anyhow!("select a document folder first"))?;
    let window = Duration::from_secs(core.cfg.hint_window_secs);
    let t0 = Instant::now();
    let requested = mode == Mode::Requested;
    core.shared.update(|s| {
        if requested {
            s.hint_status = HintStatus::Writing;
        } else if s.hint_status != HintStatus::Writing {
            s.hint_status = HintStatus::Checking;
        }
    });
    core.usage.event("hint_start", json!({ "seq": seq, "requested": requested }));
    let lines = core.shared.read(|s| pick_lines(&s.segments, window));
    if lines.is_empty() {
        // Nothing has been said yet: say so in the hint panel, not as an error.
        if requested {
            let raw = "Nothing has been said yet. Start the conversation, then press Hint.".to_string();
            core.shared.update(|s| s.hint = Some(Hint { raw, done: true, ..Default::default() }));
        }
        settle(core, HintStatus::Idle);
        return Ok(());
    }

    // What the user is reading now: an automatic request decides whether it still fits.
    let current = if requested { None } else { core.shared.read(|s| s.hint.clone()).filter(Hint::has_content) };
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
    let mut query = vec![(focus, 3.0), (all.as_str(), 1.0)];
    // A bare "¿Es verdad?" or "¿y?" carries no topic: the current answer does, so a follow-up
    // finds the same pages as the question it refers to.
    let shown: String = current.iter().flat_map(|h| std::iter::once(h.es.as_str()).chain(h.addenda.iter().map(|a| a.es.as_str()))).collect::<Vec<_>>().join(" ");
    if !shown.is_empty() {
        query.push((shown.as_str(), 1.5));
    }
    let searched = Instant::now();
    let found = index.search(&query, core.cfg.hint_chunks.max(1));
    let search = searched.elapsed();
    core.usage.event("hint_search", json!({ "seq": seq, "ms": search.as_millis() as u64, "chunks": found.len(), "lines": lines.len() }));

    let mut sources: Vec<String> = Vec::new();
    let mut excerpts = String::new();
    for (_, c) in &found {
        excerpts.push_str(&format!("[{} p.{}]\n{}\n\n", c.file, c.page, c.text));
        let tag = format!("{} p.{}", c.file, c.page);
        if !sources.contains(&tag) {
            sources.push(tag);
        }
    }
    let screen = match (&current, requested) {
        (_, true) => String::new(),
        (Some(h), _) => {
            let more: String = h.addenda.iter().map(|a| format!("\nAddition: {}", a.es)).collect();
            format!("\n\nCurrent hint on screen:\nQ: {}\nES: {}{more}", h.question, h.es)
        }
        (None, _) => "\n\nCurrent hint on screen: none".to_string(),
    };
    let topic = core.shared.read(|s| s.topic.clone()).map(|t| format!("Topic of the call: {t}\n\n")).unwrap_or_default();
    let user = format!("{topic}Transcript (latest last):\n{}{screen}\n\nDocument excerpts:\n{}", transcript.join("\n"), excerpts);
    let messages = json!([{ "role": "system", "content": system_prompt(mode) }, { "role": "user", "content": user }]);

    let mut hint = Hint { sources, started: Some(Instant::now()), ..Default::default() };
    let mut first_token = None;
    let streamed = core
        .llm
        .hedged_stream(&core.cfg.llm.hint_models, messages, |raw| {
            first_token.get_or_insert_with(|| t0.elapsed());
            hint.apply_raw(raw);
            // Nothing is shown before the model has said what it is (KEEP never shows anything).
            let verdict = if requested { Some(Verdict::New) } else { Verdict::parse(raw) };
            if let Some(v @ (Verdict::New | Verdict::Update)) = verdict
                && hint.has_content()
            {
                place(&core.shared, seq, mode, v, &hint);
            }
        })
        .await?;
    hint.apply_raw(&streamed.text);
    hint.done = true;
    let last = lines.iter().rev().find(|(sp, _)| *sp == Speaker::Them).map_or("", |(_, t)| t.as_str());
    // A reply that ignores the format is shown as it is, like a new hint.
    let verdict = if requested { Verdict::New } else { Verdict::parse(&streamed.text).unwrap_or(Verdict::New) };
    if verdict == Verdict::Keep {
        core.shared.update(|s| s.hint_kept = s.hint_kept.max(seq));
        settle(core, HintStatus::NoQuestion);
        core.usage.event("hint_keep", json!({ "seq": seq, "model": streamed.model, "total_ms": t0.elapsed().as_millis() as u64, "last": last }));
        if let Some(r) = core.rec() {
            r.note(&format!("HINT keep · {} · {:.1} s · nothing new after: {last}", streamed.model, t0.elapsed().as_secs_f64()));
        }
        return Ok(());
    }
    let placed = place(&core.shared, seq, mode, verdict, &hint);
    settle(core, HintStatus::Idle);
    let timing = HintTiming { model: streamed.model, search, first_token, total: t0.elapsed(), verdict: verdict.label(), placed };
    core.usage.event(
        "hint_done",
        json!({
            "seq": seq,
            "verdict": timing.verdict,
            "placed": placed,
            "requested": requested,
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

/// Puts the result of request `seq` on screen if that request may; false if a newer one has the
/// screen. The first words of a NEW hint move the hint that was on screen (with its additions) to
/// the earlier ones. An UPDATE never touches the text on screen: it is appended below as an
/// addition, so a hint the user is reading does not change under their eyes.
fn place(shared: &Shared, seq: u64, mode: Mode, verdict: Verdict, h: &Hint) -> bool {
    let mut placed = false;
    shared.update(|s| {
        let has_hint = s.hint.as_ref().is_some_and(Hint::has_content);
        if mode == Mode::Auto && verdict == Verdict::Update && has_hint && seq != s.hint_owner {
            let known = s.hint.as_ref().is_some_and(|c| c.addenda.iter().any(|a| a.seq == seq));
            if !known && seq <= s.hint_owner.max(s.hint_kept).max(s.hint_upd) {
                return;
            }
            let add = Addendum { seq, es: h.es.clone(), ru: h.ru.clone(), es_bold: h.es_bold.clone(), ru_bold: h.ru_bold.clone(), own: h.own };
            let cur = s.hint.as_mut().expect("checked above");
            match cur.addenda.iter_mut().find(|a| a.seq == seq) {
                Some(a) => *a = add,
                None => cur.addenda.push(add),
            }
            s.hint_upd = s.hint_upd.max(seq);
            if s.hint_status == HintStatus::Checking {
                s.hint_status = HintStatus::Writing;
            }
            placed = true;
            return;
        }
        let allowed = match mode {
            // The user asked: it wins over everything that started before it.
            Mode::Requested => seq >= s.hint_owner,
            Mode::Auto => seq == s.hint_owner || (seq > s.hint_owner && seq > s.hint_kept),
        };
        if !allowed {
            return;
        }
        if seq != s.hint_owner {
            if (mode == Mode::Requested || verdict == Verdict::New)
                && let Some(mut old) = s.hint.take().filter(Hint::has_content)
            {
                old.done = true;
                s.hint_prev.insert(0, old);
                s.hint_prev.truncate(PREV_HINTS);
            }
            s.hint_owner = seq;
        }
        // The text of this request keeps streaming into the same slot; additions stay.
        let mut shown = h.clone();
        if let Some(old) = &s.hint {
            shown.addenda = old.addenda.clone();
        }
        s.hint = Some(shown);
        if s.hint_status == HintStatus::Checking {
            s.hint_status = HintStatus::Writing;
        }
        placed = true;
    });
    placed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(text: &str, age_secs: u64) -> Segment {
        let at = Instant::now().checked_sub(Duration::from_secs(age_secs)).expect("clock");
        Segment { item_id: text.into(), speaker: Speaker::Them, es: text.into(), ru: None, partial: false, at }
    }

    fn hint(es: &str) -> Hint {
        Hint { question: format!("Q {es}"), es: es.into(), done: true, ..Default::default() }
    }

    fn screen(shared: &Shared) -> (String, Vec<String>) {
        shared.read(|s| (s.hint.as_ref().map_or(String::new(), |h| h.es.clone()), s.hint_prev.iter().map(|h| h.es.clone()).collect()))
    }

    fn additions(shared: &Shared) -> Vec<String> {
        shared.read(|s| s.hint.as_ref().map_or(vec![], |h| h.addenda.iter().map(|a| a.es.clone()).collect()))
    }

    #[test]
    fn new_hint_moves_the_old_one_down() {
        let shared = Shared::new(None);
        assert!(place(&shared, 1, Mode::Auto, Verdict::New, &hint("a")));
        assert!(place(&shared, 2, Mode::Auto, Verdict::New, &hint("b")));
        assert_eq!(screen(&shared), ("b".into(), vec!["a".into()]));
        // The same request keeps streaming into the same slot.
        assert!(place(&shared, 2, Mode::Auto, Verdict::New, &hint("b2")));
        assert_eq!(screen(&shared), ("b2".into(), vec!["a".into()]));
        // Only the last few earlier hints are kept.
        for (i, n) in ["c", "d", "e", "f"].iter().enumerate() {
            place(&shared, 3 + i as u64, Mode::Auto, Verdict::New, &hint(n));
        }
        assert_eq!(screen(&shared).1.len(), PREV_HINTS);
    }

    #[test]
    fn update_is_appended_and_never_touches_the_text_on_screen() {
        let shared = Shared::new(None);
        place(&shared, 1, Mode::Auto, Verdict::New, &hint("main"));
        assert!(place(&shared, 2, Mode::Auto, Verdict::Update, &hint("add 1")));
        assert_eq!(screen(&shared), ("main".into(), vec![]));
        assert_eq!(additions(&shared), ["add 1"]);
        // The same update keeps streaming into its own block.
        assert!(place(&shared, 2, Mode::Auto, Verdict::Update, &hint("add 1 longer")));
        assert_eq!(additions(&shared), ["add 1 longer"]);
        // A second update comes below the first.
        assert!(place(&shared, 4, Mode::Auto, Verdict::Update, &hint("add 2")));
        assert_eq!(additions(&shared), ["add 1 longer", "add 2"]);
        // One that started before it (3) and comes late is dropped.
        assert!(!place(&shared, 3, Mode::Auto, Verdict::Update, &hint("stale")));
        assert_eq!(additions(&shared).len(), 2);
    }

    #[test]
    fn main_text_keeps_streaming_after_an_update_and_keeps_the_additions() {
        let shared = Shared::new(None);
        place(&shared, 1, Mode::Auto, Verdict::New, &Hint { es: "ma".into(), question: "Q".into(), ..Default::default() });
        place(&shared, 2, Mode::Auto, Verdict::Update, &hint("add"));
        assert!(place(&shared, 1, Mode::Auto, Verdict::New, &hint("main complete")));
        assert_eq!(screen(&shared).0, "main complete");
        assert_eq!(additions(&shared), ["add"]);
    }

    #[test]
    fn a_new_hint_takes_its_additions_to_the_earlier_ones() {
        let shared = Shared::new(None);
        place(&shared, 1, Mode::Auto, Verdict::New, &hint("a"));
        place(&shared, 2, Mode::Auto, Verdict::Update, &hint("a+"));
        place(&shared, 3, Mode::Auto, Verdict::New, &hint("b"));
        assert_eq!(screen(&shared), ("b".into(), vec!["a".into()]));
        assert!(additions(&shared).is_empty());
        assert_eq!(shared.read(|s| s.hint_prev[0].addenda.len()), 1);
    }

    #[test]
    fn update_with_nothing_on_screen_shows_as_the_hint() {
        let shared = Shared::new(None);
        assert!(place(&shared, 1, Mode::Auto, Verdict::Update, &hint("whole")));
        assert_eq!(screen(&shared), ("whole".into(), vec![]));
        assert!(additions(&shared).is_empty());
        // It keeps streaming as the main text, not as an addition.
        assert!(place(&shared, 1, Mode::Auto, Verdict::Update, &hint("whole 2")));
        assert_eq!(screen(&shared).0, "whole 2");
        assert!(additions(&shared).is_empty());
    }

    #[test]
    fn an_update_older_than_a_keep_is_dropped() {
        let shared = Shared::new(None);
        place(&shared, 1, Mode::Auto, Verdict::New, &hint("a"));
        shared.update(|s| s.hint_kept = 5);
        assert!(!place(&shared, 4, Mode::Auto, Verdict::Update, &hint("late")));
        assert!(place(&shared, 6, Mode::Auto, Verdict::Update, &hint("fresh")));
    }

    #[test]
    fn a_late_older_answer_never_replaces_a_newer_one() {
        let shared = Shared::new(None);
        assert!(place(&shared, 3, Mode::Auto, Verdict::New, &hint("new")));
        assert!(!place(&shared, 2, Mode::Auto, Verdict::New, &hint("old")));
        assert_eq!(screen(&shared), ("new".into(), vec![]));
    }

    #[test]
    fn keep_blocks_older_requests_that_have_not_shown_anything() {
        let shared = Shared::new(None);
        place(&shared, 1, Mode::Auto, Verdict::New, &hint("a"));
        shared.update(|s| s.hint_kept = 5);
        // 4 started before the KEEP of 5 and has not claimed the screen yet.
        assert!(!place(&shared, 4, Mode::Auto, Verdict::New, &hint("late")));
        // A request that is already streaming is not cut off by a later KEEP.
        place(&shared, 6, Mode::Auto, Verdict::New, &hint("b"));
        shared.update(|s| s.hint_kept = 7);
        assert!(place(&shared, 6, Mode::Auto, Verdict::New, &hint("b done")));
        assert_eq!(screen(&shared).0, "b done");
    }

    #[test]
    fn the_hint_button_wins_over_everything_that_started_before_it() {
        let shared = Shared::new(None);
        place(&shared, 1, Mode::Auto, Verdict::New, &hint("a"));
        shared.update(|s| s.hint_kept = 5);
        assert!(place(&shared, 4, Mode::Requested, Verdict::New, &hint("asked")));
        assert_eq!(screen(&shared), ("asked".into(), vec!["a".into()]));
        // An automatic request that started earlier and comes late is dropped.
        assert!(!place(&shared, 3, Mode::Auto, Verdict::New, &hint("late")));
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
