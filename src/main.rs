mod audio;
mod config;
mod core;
mod docs;
mod hint;
mod interview;
mod llm;
mod logfile;
mod realtime;
mod record;
mod search;
mod state;
mod ui;
mod usage;
mod video;
#[cfg(feature = "updater")]
mod update;

use anyhow::{Result, anyhow};
use std::sync::Arc;

fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--version") {
        println!("assistant {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    // Relative paths in arguments (e.g. `--persona`) are relative to where the user started us.
    let launch_dir = std::env::current_dir()?;
    let workdir = config::prepare_workdir()?;
    let log_path = logfile::redirect_stderr(&workdir);
    #[cfg(feature = "updater")]
    update::check_and_restart();
    let cfg = Arc::new(config::Config::load("config.toml")?);
    let keys_msg = {
        let m = cfg.missing_keys();
        (!m.is_empty()).then(|| {
            format!("API key missing ({}). Add your keys to {} and restart the app.", m.join(", "), workdir.join("config.toml").display())
        })
    };
    if let Some(m) = &keys_msg {
        eprintln!("{m}");
    }
    let args: Vec<String> = std::env::args().collect();
    let has = |name: &str| args.iter().any(|a| a == name);
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1));

    if has("--test-audio") {
        eprintln!("Recording 5 seconds of speaker output to test.wav…");
        audio::record_wav("test.wav", 5)?;
        eprintln!("Done: test.wav");
        return Ok(());
    }

    // `--topic <name>` selects a document folder at startup.
    let topic_arg = flag("--topic").cloned();

    if has("--test-search") {
        let topic = topic_arg.ok_or_else(|| anyhow!("usage: --test-search --topic <name> \"question\""))?;
        let (chunks, report) = docs::load_chunks(&docs::topic_dir(&cfg.docs_dir, &topic)?)?;
        for r in &report {
            eprintln!("{:?} {} ({} pages, {} chunks)", r.status, r.name, r.pages, r.chunks);
        }
        let index = search::Index::build(chunks);
        eprintln!("Indexed chunks: {}", index.len());
        let q = args.last().map(String::as_str).unwrap_or("");
        for (score, c) in index.search(&[(q, 1.0)], 5) {
            println!("[{score:.2}] {} p.{}: {}…", c.file, c.page, c.text.chars().take(120).collect::<String>());
        }
        return Ok(());
    }

    // `--interview --topic <name> [--persona <file>] [--questions N]`: the interviewer simulator (no window).
    if has("--interview") {
        if let Some(m) = keys_msg {
            return Err(anyhow!(m));
        }
        let topic = topic_arg.ok_or_else(|| anyhow!("usage: --interview --topic <name> [--persona <file>] [--questions N]"))?;
        let persona = flag("--persona")
            .map(|p| std::fs::read_to_string(launch_dir.join(p)).map_err(|e| anyhow!("cannot read {p}: {e}")))
            .transpose()?
            .map(|t| t.trim().to_string());
        let questions = flag("--questions").map(|q| q.parse::<usize>().map_err(|_| anyhow!("--questions needs a number"))).transpose()?;
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        return rt.block_on(interview::run(&cfg, &topic, persona, questions));
    }

    let (topics, topics_err) = match docs::list_topics(&cfg.docs_dir) {
        Ok(t) => (t, None),
        Err(e) => (vec![], Some(e.to_string())),
    };

    // `--record-video`: record the window into the session folder (debug).
    let record_video = has("--record-video");
    let hint_max_age = std::time::Duration::from_secs(cfg.hint_max_age_secs);
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let handle = rt.handle().clone();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([440.0, 820.0]).with_always_on_top().with_title("Assistant"),
        // Remember window size and position between runs.
        persist_window: true,
        ..Default::default()
    };
    eframe::run_native(
        "Assistant",
        options,
        Box::new(move |cc| {
            ui::install_fonts(&cc.egui_ctx);
            let shared = state::Shared::new(Some(cc.egui_ctx.clone()));
            // A single folder, or one given via --topic, is selected automatically.
            let auto = topic_arg.or_else(|| (topics.len() == 1).then(|| topics[0].clone()));
            shared.update(|s| {
                s.status = "Stopped".into();
                s.error = match (keys_msg, topics_err) {
                    (Some(k), Some(t)) => Some(format!("{k}\n{t}")),
                    (k, t) => k.or(t),
                };
                s.topics = topics;
                s.log_path = log_path;
            });
            let core = Arc::new(core::Core::new(cfg, shared.clone()));
            let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
            if let Some(t) = auto {
                let _ = cmd_tx.send(core::Cmd::SelectTopic(t));
            }
            handle.spawn(core::run(core, cmd_rx));
            Ok(Box::new(ui::App::new(shared, cmd_tx, hint_max_age, cc.storage, record_video)))
        }),
    )
    .map_err(|e| anyhow!("{e}"))?;
    drop(rt);
    Ok(())
}
