mod audio;
mod config;
mod core;
mod docs;
mod hint;
mod llm;
mod realtime;
mod search;
mod state;
mod ui;
#[cfg(feature = "updater")]
mod update;

use anyhow::{Result, anyhow};
use std::sync::{Arc, Mutex, RwLock};

fn main() -> Result<()> {
    #[cfg(feature = "updater")]
    update::check_and_restart();
    // A release binary may be started from any directory: fall back to its own folder.
    if !std::path::Path::new("config.toml").exists() {
        if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_path_buf())) {
            let _ = std::env::set_current_dir(dir);
        }
    }
    let cfg = Arc::new(config::Config::load("config.toml")?);
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str);

    if mode == Some("--test-audio") {
        eprintln!("Recording 5 seconds of speaker output to test.wav…");
        audio::record_wav("test.wav", 5)?;
        eprintln!("Done: test.wav");
        return Ok(());
    }

    // `--topic <name>` selects a document folder at startup.
    let topic_arg = args.iter().position(|a| a == "--topic").and_then(|i| args.get(i + 1)).cloned();

    if mode == Some("--test-search") || args.iter().any(|a| a == "--test-search") {
        let topic = topic_arg.ok_or_else(|| anyhow!("usage: --test-search --topic <name> \"question\""))?;
        let dir = std::path::Path::new(&cfg.docs_dir).join(&topic);
        let index = search::Index::build(docs::load_chunks(&dir.to_string_lossy())?);
        eprintln!("Indexed chunks: {}", index.len());
        let q = args.last().map(String::as_str).unwrap_or("");
        for (score, c) in index.search(&[(q, 1.0)], 5) {
            println!("[{score:.2}] {} p.{}: {}…", c.file, c.page, c.text.chars().take(120).collect::<String>());
        }
        return Ok(());
    }

    let (topics, topics_err) = match docs::list_topics(&cfg.docs_dir) {
        Ok(t) => (t, None),
        Err(e) => (vec![], Some(e.to_string())),
    };

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let handle = rt.handle().clone();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([720.0, 820.0]).with_always_on_top().with_title("Zoom Assistant"),
        ..Default::default()
    };
    eframe::run_native(
        "Zoom Assistant",
        options,
        Box::new(move |cc| {
            let shared = state::Shared::new(Some(cc.egui_ctx.clone()));
            // A single folder, or one given via --topic, is selected automatically.
            let auto = topic_arg.or_else(|| (topics.len() == 1).then(|| topics[0].clone()));
            shared.update(|s| {
                s.status = "Stopped".into();
                s.error = topics_err;
                s.topics = topics;
            });
            let llm = llm::Llm::new(&cfg.llm);
            let core = Arc::new(core::Core { cfg, shared: shared.clone(), index: RwLock::new(None), llm, prep: Mutex::new(None) });
            let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
            if let Some(t) = auto {
                let _ = cmd_tx.send(core::Cmd::SelectTopic(t));
            }
            handle.spawn(core::run(core, cmd_rx));
            Ok(Box::new(ui::App::new(shared, cmd_tx)))
        }),
    )
    .map_err(|e| anyhow!("{e}"))?;
    drop(rt);
    Ok(())
}
