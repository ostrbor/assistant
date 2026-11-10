use crate::config;
use anyhow::{Result, anyhow, bail};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;

pub struct Llm {
    http: reqwest::Client,
    base_url: String,
    key: Option<String>,
}

enum StreamEv {
    Delta(String),
    Done,
    Err(String),
}

impl Llm {
    pub fn new(cfg: &config::Llm) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            key: config::env_key(&cfg.api_key_env).ok(),
        }
    }

    fn body(model: &str, messages: &Value, stream: bool, max_tokens: u32) -> Value {
        let mut b = json!({ "model": model, "messages": messages, "stream": stream, "max_tokens": max_tokens });
        if model.contains("gpt-oss") {
            b["reasoning_effort"] = json!("low");
        }
        b
    }

    pub async fn translate(&self, model: &str, context: &[String], es: &str) -> Result<String> {
        let key = self.key.as_deref().ok_or_else(|| anyhow!("LLM token is not set (see api_key_env in config.toml)"))?;
        let mut user = String::new();
        if !context.is_empty() {
            user.push_str("Previous utterances (context only):\n");
            for c in context {
                user.push_str(&format!("- {c}\n"));
            }
        }
        user.push_str(&format!("Utterance to translate:\n{es}"));
        let messages = json!([
            { "role": "system", "content": "Translate the Spanish utterance from a video call into natural English. Output only the translation of the last utterance, nothing else." },
            { "role": "user", "content": user }
        ]);
        let res = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(key)
            .json(&Self::body(model, &messages, false, 600))
            .send()
            .await?;
        let status = res.status();
        let v: Value = res.json().await?;
        if !status.is_success() {
            bail!("LLM {status}: {}", v["error"]["message"].as_str().unwrap_or("error"));
        }
        Ok(v["choices"][0]["message"]["content"].as_str().unwrap_or_default().trim().to_string())
    }

    /// Streams from several models/providers at once and keeps whichever starts answering first.
    pub async fn hedged_stream(&self, models: &[String], messages: Value, mut on_update: impl FnMut(&str)) -> Result<String> {
        let key = self.key.clone().ok_or_else(|| anyhow!("LLM token is not set (see api_key_env in config.toml)"))?;
        let (tx, mut rx) = mpsc::unbounded_channel::<(usize, StreamEv)>();
        let handles: Vec<_> = models
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let (http, url, key, tx) = (self.http.clone(), format!("{}/chat/completions", self.base_url), key.clone(), tx.clone());
                let body = Self::body(m, &messages, true, 800);
                tokio::spawn(async move {
                    if let Err(e) = stream_one(&http, &url, &key, body, i, &tx).await {
                        let _ = tx.send((i, StreamEv::Err(e.to_string())));
                    }
                })
            })
            .collect();
        drop(tx);

        let (mut winner, mut text, mut failed) = (None, String::new(), 0);
        let result = loop {
            let Some((i, ev)) = rx.recv().await else { break Err(anyhow!("empty LLM response")) };
            match ev {
                StreamEv::Delta(d) => {
                    if winner.is_none() {
                        winner = Some(i);
                        for (j, h) in handles.iter().enumerate() {
                            if j != i {
                                h.abort();
                            }
                        }
                    }
                    if winner == Some(i) {
                        text.push_str(&d);
                        on_update(&text);
                    }
                }
                StreamEv::Done if winner == Some(i) => break Ok(text.clone()),
                StreamEv::Done => {}
                StreamEv::Err(e) => {
                    if winner == Some(i) {
                        break Err(anyhow!(e));
                    }
                    failed += 1;
                    if winner.is_none() && failed == models.len() {
                        break Err(anyhow!(e));
                    }
                }
            }
        };
        for h in &handles {
            h.abort();
        }
        result
    }
}

async fn stream_one(http: &reqwest::Client, url: &str, key: &str, body: Value, idx: usize, tx: &mpsc::UnboundedSender<(usize, StreamEv)>) -> Result<()> {
    let res = http.post(url).bearer_auth(key).json(&body).send().await?;
    let status = res.status();
    if !status.is_success() {
        let t = res.text().await.unwrap_or_default();
        bail!("LLM {status}: {}", t.chars().take(200).collect::<String>());
    }
    let mut stream = res.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk?);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" {
                let _ = tx.send((idx, StreamEv::Done));
                return Ok(());
            }
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                if let Some(d) = v["choices"][0]["delta"]["content"].as_str() {
                    if !d.is_empty() {
                        let _ = tx.send((idx, StreamEv::Delta(d.to_string())));
                    }
                }
            }
        }
    }
    let _ = tx.send((idx, StreamEv::Done));
    Ok(())
}
