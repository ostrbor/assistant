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
