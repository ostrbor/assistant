use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub docs_dir: String,
    pub hint_window_secs: u64,
    pub hint_max_age_secs: u64,
    pub transcribe: Transcribe,
    pub llm: Llm,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Transcribe {
    pub api_key_env: String,
    pub model: String,
    pub language: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Llm {
    pub base_url: String,
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

pub fn env_key(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("environment variable {name} is not set"))
}
