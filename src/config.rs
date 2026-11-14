use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub docs_dir: String,
    pub hint_window_secs: u64,
    pub hint_max_age_secs: u64,
    pub transcribe: Transcribe,
    pub llm: Llm,
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
}

fn default_silence_threshold() -> f64 {
    300.0
}

fn default_mic_silence_threshold() -> f64 {
    500.0
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
    if !Path::new("config.toml").exists() {
        let beside_exe = std::env::current_exe().ok().and_then(|e| e.parent().map(PathBuf::from)).filter(|d| d.join("config.toml").exists());
        let dir = match beside_exe {
            Some(d) => d,
            None => {
                let home = std::env::var_os("HOME").context("HOME is not set")?;
                let dir = Path::new(&home).join("assistant");
                std::fs::create_dir_all(dir.join("docs")).with_context(|| format!("cannot create {}", dir.display()))?;
                let cfg = dir.join("config.toml");
                if !cfg.exists() {
                    write_private(&cfg, CONFIG_TEMPLATE).with_context(|| format!("cannot write {}", cfg.display()))?;
                    eprintln!("Created {} with a default config.toml and docs/", dir.display());
                }
                dir
            }
        };
        std::env::set_current_dir(&dir).with_context(|| format!("cannot enter {}", dir.display()))?;
    }
    std::env::current_dir().context("cannot determine the working directory")
}

/// Writes a file readable only by the owner (it will hold API keys).
fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?.write_all(text.as_bytes())
}
