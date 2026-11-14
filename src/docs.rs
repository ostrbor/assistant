use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct Chunk {
    pub file: String,
    pub page: usize,
    pub text: String,
}

const CHUNK_WORDS: usize = 200;
const OVERLAP_WORDS: usize = 40;

/// Names of subfolders of `root` that contain at least one PDF, sorted.
pub fn list_topics(root: &str) -> Result<Vec<String>> {
    let mut topics: Vec<String> = std::fs::read_dir(root)
        .with_context(|| format!("document folder not found: {root}"))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter(|e| has_pdf(&e.path()))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    topics.sort();
    Ok(topics)
}

fn has_pdf(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).any(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("pdf"))))
        .unwrap_or(false)
}

/// Path of a topic folder. The name comes from our own folder list, but never allow path traversal.
pub fn topic_dir(docs_dir: &str, topic: &str) -> Result<PathBuf> {
    if topic.is_empty() || topic.contains(['/', '\\']) || topic.starts_with('.') {
        bail!("Invalid folder name: {topic}");
    }
    Ok(Path::new(docs_dir).join(topic))
}

/// Reads all PDFs in a folder (pdftotext, .txt cache next to each file) and splits them into chunks.
pub fn load_chunks(dir: &Path) -> Result<Vec<Chunk>> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("document folder not found: {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")))
        .collect();
    paths.sort();

    let mut chunks = Vec::new();
    for p in &paths {
        let text = pdf_text(p)?;
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        // pdftotext separates pages with a form feed character
        for (i, page) in text.split('\u{c}').enumerate() {
            chunk_page(&name, i + 1, page, &mut chunks);
        }
    }
    Ok(chunks)
}

fn pdf_text(pdf: &Path) -> Result<String> {
    let cache = pdf.with_extension("txt");
    let fresh = match (std::fs::metadata(&cache), std::fs::metadata(pdf)) {
        (Ok(c), Ok(p)) => c.modified()? >= p.modified()?,
        _ => false,
    };
    if fresh {
        return Ok(std::fs::read_to_string(&cache)?);
    }
    let out = Command::new("pdftotext")
        .args(["-layout"])
        .arg(pdf)
        .arg("-")
        .output()
        .context("could not run pdftotext (install poppler-utils)")?;
    anyhow::ensure!(out.status.success(), "pdftotext failed on {}", pdf.display());
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let _ = std::fs::write(&cache, &text);
    Ok(text)
}

fn chunk_page(file: &str, page: usize, text: &str, out: &mut Vec<Chunk>) {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return;
    }
    let step = CHUNK_WORDS - OVERLAP_WORDS;
    let mut start = 0;
    loop {
        let end = (start + CHUNK_WORDS).min(words.len());
        out.push(Chunk { file: file.into(), page, text: words[start..end].join(" ") });
        if end == words.len() {
            break;
        }
        start += step;
    }
}
