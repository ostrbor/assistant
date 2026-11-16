use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub docs_dir: String,
    pub hint_window_secs: u64,
    pub hint_max_age_secs: u64,
    /// How many of the best matching document excerpts the hint generator sees.
    #[serde(default = "default_hint_chunks")]
    pub hint_chunks: usize,
    /// Prepare and show a hint by itself after every turn of the other person.
    #[serde(default = "default_true")]
    pub hint_auto: bool,
    pub transcribe: Transcribe,
    pub llm: Llm,
    /// Overrides of the built-in prices used in the session cost report, by model name.
    #[serde(default)]
    pub prices: std::collections::HashMap<String, crate::usage::Price>,
    #[serde(default)]
    pub interview: Interview,
}

/// Settings of the interviewer simulator (`--interview`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Interview {
    /// Who the interviewer is; can be overridden with `--persona <file>`.
    pub persona: String,
    /// Number of questions after the greeting.
    pub questions: usize,
    pub tts_model: String,
    pub voice: String,
    /// Tone of the synthesized voice (gpt-4o-mini-tts only).
    pub tts_instructions: String,
    /// Quiet time after which an answer counts as finished.
    pub answer_silence_secs: f64,
}

impl Default for Interview {
    fn default() -> Self {
        Self {
            persona: "Eres Laura, engineering manager en una empresa de software. Entrevistas a un candidato para un puesto de desarrollador. Tono amable y profesional.".into(),
            questions: 5,
            tts_model: "gpt-4o-mini-tts".into(),
            voice: "coral".into(),
            tts_instructions: "Habla en español, con tono amable y profesional y un ritmo natural.".into(),
            answer_silence_secs: 3.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Transcribe {
    /// OpenAI key; if empty, the environment variable named by `api_key_env` is used.
    #[serde(default)]
    pub api_key: String,
    pub api_key_env: String,
    pub model: String,
    pub language: String,
    /// RMS level (i16 units) of a 100 ms frame below which speaker audio counts as silence.
    #[serde(default = "default_silence_threshold")]
    pub silence_threshold: f64,
    /// Same for the microphone, which is usually noisier.
    #[serde(default = "default_mic_silence_threshold")]
    pub mic_silence_threshold: f64,
    /// Also transcribe your own voice (default input device) so hints know what you already said.
    #[serde(default = "default_true")]
    pub capture_mic: bool,
    /// Quiet time (ms) after which the other person's phrase counts as finished.
    #[serde(default = "default_silence_ms")]
    pub silence_ms: u64,
    /// Same for your microphone. Longer: you pause to think, and a phrase cut in pieces is
    /// transcribed worse (every piece without the context of the others).
    #[serde(default = "default_mic_silence_ms")]
    pub mic_silence_ms: u64,
    /// Extra words for the recognizer (names, terms). The language and the chosen folder are added by themselves.
    #[serde(default)]
    pub prompt: String,
}

fn default_silence_threshold() -> f64 {
    300.0
}

fn default_mic_silence_threshold() -> f64 {
    500.0
}

fn default_silence_ms() -> u64 {
    500
}

fn default_mic_silence_ms() -> u64 {
    900
}

fn default_hint_chunks() -> usize {
    12
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct Llm {
    pub base_url: String,
    /// LLM key; if empty, the environment variable named by `api_key_env` is used.
    #[serde(default)]
    pub api_key: String,
    pub api_key_env: String,
    pub translate_model: String,
    pub hint_models: Vec<String>,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {path}"))?;
        toml::from_str(&text).with_context(|| format!("error in {path}"))
    }
}

/// The key from the config file, or else from the environment variable.
pub fn resolve_key(api_key: &str, env: &str) -> Option<String> {
    let k = api_key.trim();
    if !k.is_empty() {
        return Some(k.to_string());
    }
    std::env::var(env).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

impl Transcribe {
    pub fn key(&self) -> Result<String> {
        resolve_key(&self.api_key, &self.api_key_env)
            .context("OpenAI API key is not set (set [transcribe] api_key in config.toml)")
    }
}

impl Llm {
    pub fn key(&self) -> Option<String> {
        resolve_key(&self.api_key, &self.api_key_env)
    }
}

impl Config {
    /// Names of the config entries whose API key is missing.
    pub fn missing_keys(&self) -> Vec<&'static str> {
        let mut v = vec![];
        if self.transcribe.key().is_err() {
            v.push("[transcribe] api_key");
        }
        if self.llm.key().is_none() {
            v.push("[llm] api_key");
        }
        v
    }
}

const CONFIG_TEMPLATE: &str = include_str!("../config.toml");

/// Picks the working directory (where `config.toml` and `docs/` live) and returns it:
/// the current folder (development), the binary's folder (tar.gz release), or else
/// `~/assistant`, created with a default config on first run (.deb install).
pub fn prepare_workdir() -> Result<std::path::PathBuf> {
    use std::path::{Path, PathBuf};
    let home = std::env::var_os("HOME").map(PathBuf::from);
    // An installed copy (.deb) always works from ~/assistant, whatever the current folder is.
    let installed = match (std::env::current_exe().ok(), &home) {
        (Some(exe), Some(home)) => installed_workdir(&exe, home),
        _ => None,
    };
    if let Some(dir) = installed {
        ensure_home_workdir(&dir)?;
        std::env::set_current_dir(&dir).with_context(|| format!("cannot enter {}", dir.display()))?;
    } else if !Path::new("config.toml").exists() {
        let beside_exe = std::env::current_exe().ok().and_then(|e| e.parent().map(PathBuf::from)).filter(|d| d.join("config.toml").exists());
        let dir = match beside_exe {
            Some(d) => d,
            None => {
                let dir = home.context("HOME is not set")?.join("assistant");
                ensure_home_workdir(&dir)?;
                dir
            }
        };
        std::env::set_current_dir(&dir).with_context(|| format!("cannot enter {}", dir.display()))?;
    }
    std::env::current_dir().context("cannot determine the working directory")
}

/// `~/assistant` when `exe` is the .deb installation (`/usr/bin/assistant-app`, or
/// `/usr/bin/assistant` in versions up to 0.1.3) or its auto-update copy
/// (`~/assistant/bin/assistant`); `None` for development builds and tar.gz releases.
fn installed_workdir(exe: &std::path::Path, home: &std::path::Path) -> Option<std::path::PathBuf> {
    let dir = home.join("assistant");
    let deb = ["/usr/bin/assistant-app", "/usr/bin/assistant"].iter().any(|p| exe == std::path::Path::new(p));
    (deb || exe.parent() == Some(dir.join("bin").as_path())).then_some(dir)
}

/// Creates `dir` with `docs/` and a default `config.toml` if they do not exist yet.
fn ensure_home_workdir(dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dir.join("docs")).with_context(|| format!("cannot create {}", dir.display()))?;
    let cfg = dir.join("config.toml");
    if !cfg.exists() {
        write_private(&cfg, CONFIG_TEMPLATE).with_context(|| format!("cannot write {}", cfg.display()))?;
        eprintln!("Created {} with a default config.toml and docs/", dir.display());
    }
    install_agent_files(dir);
    Ok(())
}

/// Files for AI coding agents started in the working folder: the skill that condenses the lesson
/// caches (Claude Code reads `.claude/skills`), and an `AGENTS.md` that points other agents
/// (Codex…) at it.
const SKILL_DIR: &str = ".claude/skills/summarize-lesson-cache";
const SKILL_FILES: &[(&str, &str)] = &[
    ("SKILL.md", include_str!("../packaging/agent/summarize-lesson-cache/SKILL.md")),
    ("scripts/cache_tool.py", include_str!("../packaging/agent/summarize-lesson-cache/scripts/cache_tool.py")),
];
const AGENTS_MD: &str = include_str!("../packaging/agent/AGENTS.md");

/// Copies the skill into `dir` when it is missing or comes from another app version (the skill is
/// part of the app: copy it under another name to customize it), and creates `AGENTS.md` only
/// if there is none, so a file of the user is never touched. Best effort: never fails startup.
fn install_agent_files(dir: &std::path::Path) {
    let skill = dir.join(SKILL_DIR);
    let stamp = skill.join(".app-version");
    let current = env!("CARGO_PKG_VERSION");
    let write = |path: &std::path::Path, text: &str| -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, text)
    };
    if std::fs::read_to_string(&stamp).ok().as_deref().map(str::trim) != Some(current) {
        let res = SKILL_FILES.iter().try_for_each(|(name, text)| write(&skill.join(name), text)).and_then(|()| write(&stamp, current));
        match res {
            Ok(()) => eprintln!("Installed the summarize-lesson-cache skill in {}", skill.display()),
            Err(e) => eprintln!("warning: cannot install the skill in {}: {e}", skill.display()),
        }
    }
    let agents = dir.join("AGENTS.md");
    if !agents.exists()
        && let Err(e) = std::fs::write(&agents, AGENTS_MD)
    {
        eprintln!("warning: cannot write {}: {e}", agents.display());
    }
}

/// Writes a file readable only by the owner (it will hold API keys).
fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?.write_all(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::{AGENTS_MD, SKILL_DIR, install_agent_files, installed_workdir};
    use std::path::Path;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("assistant-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn agent_files_are_installed_and_not_clobbered() {
        let d = tmp("agent");
        install_agent_files(&d);
        let skill = d.join(SKILL_DIR);
        assert!(std::fs::read_to_string(skill.join("SKILL.md")).unwrap().starts_with("---\nname: summarize-lesson-cache"));
        assert!(skill.join("scripts/cache_tool.py").exists());
        assert_eq!(std::fs::read_to_string(d.join("AGENTS.md")).unwrap(), AGENTS_MD);

        // The user's own AGENTS.md is never replaced.
        std::fs::write(d.join("AGENTS.md"), "mine").unwrap();
        install_agent_files(&d);
        assert_eq!(std::fs::read_to_string(d.join("AGENTS.md")).unwrap(), "mine");

        // Same app version: the skill is left alone; another version: it is refreshed.
        std::fs::write(skill.join("SKILL.md"), "edited").unwrap();
        install_agent_files(&d);
        assert_eq!(std::fs::read_to_string(skill.join("SKILL.md")).unwrap(), "edited");
        std::fs::write(skill.join(".app-version"), "0.0.0").unwrap();
        install_agent_files(&d);
        assert!(std::fs::read_to_string(skill.join("SKILL.md")).unwrap().starts_with("---"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn installed_copies_use_home_assistant() {
        let home = Path::new("/home/u");
        let want = Some(home.join("assistant"));
        assert_eq!(installed_workdir(Path::new("/usr/bin/assistant-app"), home), want);
        assert_eq!(installed_workdir(Path::new("/usr/bin/assistant"), home), want);
        assert_eq!(installed_workdir(Path::new("/home/u/assistant/bin/assistant"), home), want);
    }

    #[test]
    fn dev_and_tarball_builds_are_not_installed() {
        let home = Path::new("/home/u");
        assert_eq!(installed_workdir(Path::new("/repo/target/release/assistant"), home), None);
        assert_eq!(installed_workdir(Path::new("/opt/x/assistant"), home), None);
        assert_eq!(installed_workdir(Path::new("/home/u/assistant/assistant"), home), None);
    }
}
