use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

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

/// Reads all PDFs in a folder (.txt cache next to each file) and splits them into chunks.
pub fn load_chunks(dir: &Path) -> Result<Vec<Chunk>> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("document folder not found: {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")))
        .collect();
    paths.sort();

    let mut chunks = Vec::new();
    let mut loaded = 0;
    let mut last_err = None;
    for p in &paths {
        let text = match pdf_text(p) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("skip {}: {e:#}", p.display());
                last_err = Some(e);
                continue;
            }
        };
        loaded += 1;
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let before = chunks.len();
        // pages are separated by a form feed character
        for (i, page) in text.split('\u{c}').enumerate() {
            chunk_page(&name, i + 1, page, &mut chunks);
        }
        if chunks.len() == before {
            eprintln!("warning: {name} has no text layer (scanned PDF? OCR needed)");
        }
    }
    if loaded == 0 && let Some(e) = last_err {
        return Err(e);
    }
    Ok(chunks)
}

const CACHE_VERSION: &str = "v1";

/// Cache header identifying the exact PDF (size + mtime) and extraction logic version.
fn cache_header(meta: &std::fs::Metadata) -> String {
    let nanos = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    format!("{CACHE_VERSION} {} {nanos}", meta.len())
}

fn pdf_text(pdf: &Path) -> Result<String> {
    let name = pdf.file_name().unwrap().to_string_lossy().to_string();
    let cache_dir = pdf.parent().unwrap_or(Path::new(".")).join(".cache");
    let cache = cache_dir.join(format!("{name}.txt"));
    let header = cache_header(&std::fs::metadata(pdf).with_context(|| format!("cannot read {}", pdf.display()))?);
    if let Ok(c) = std::fs::read_to_string(&cache)
        && let Some((h, body)) = c.split_once('\n')
        && h == header
    {
        return Ok(body.to_string());
    }
    let pages = extract_pages(pdf).with_context(|| format!("cannot read {}", pdf.display()))?;
    // Pages are separated by a form feed character (also in the cache file).
    let text = pages.join("\u{c}");
    // Best-effort atomic cache write; never touches user files next to the PDF.
    if std::fs::create_dir_all(&cache_dir).is_ok() {
        let tmp = cache_dir.join(format!("{name}.txt.tmp"));
        if std::fs::write(&tmp, format!("{header}\n{text}")).is_ok() {
            let _ = std::fs::rename(&tmp, &cache);
        }
    }
    Ok(text)
}

/// Extracts text page by page. A failing or panicking page becomes an empty page
/// (numbering is preserved) instead of truncating or losing the whole document.
fn extract_pages(pdf: &Path) -> Result<Vec<String>> {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let mut doc = pdf_extract::Document::load(pdf)?;
    if doc.is_encrypted() {
        doc.decrypt("").map_err(|_| anyhow::anyhow!("encrypted PDF"))?;
    }
    let n = doc.get_pages().len() as u32;
    let mut failed = 0;
    let pages: Vec<String> = (1..=n)
        .map(|i| {
            let r = catch_unwind(AssertUnwindSafe(|| {
                let mut s = String::new();
                pdf_extract::output_doc_page(&doc, &mut pdf_extract::PlainTextOutput::new(&mut s), i).map(|_| s)
            }));
            match r {
                // a form feed inside page text would shift page numbers
                Ok(Ok(s)) => s.replace('\u{c}', " "),
                _ => {
                    failed += 1;
                    String::new()
                }
            }
        })
        .collect();
    if n > 0 && failed == n {
        bail!("unsupported PDF (no page could be read)");
    }
    if failed > 0 {
        eprintln!("warning: {}: {failed} of {n} pages could not be read", pdf.display());
    }
    Ok(pages)
}

/// Joins hyphenated line breaks and expands ligatures for better search.
fn normalize(text: &str) -> String {
    let t = text
        .replace('\u{fb00}', "ff")
        .replace('\u{fb01}', "fi")
        .replace('\u{fb02}', "fl")
        .replace('\u{fb03}', "ffi")
        .replace('\u{fb04}', "ffl");
    let mut out = String::with_capacity(t.len());
    let chars: Vec<char> = t.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '-'
            && i > 0
            && chars[i - 1].is_alphabetic()
            && matches!(chars.get(i + 1), Some('\n'))
            && chars.get(i + 2).is_some_and(|c| c.is_lowercase())
        {
            i += 2;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn chunk_page(file: &str, page: usize, text: &str, out: &mut Vec<Chunk>) {
    let text = normalize(text);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks_of(text: &str) -> Vec<Chunk> {
        let mut v = Vec::new();
        chunk_page("f.pdf", 1, text, &mut v);
        v
    }

    fn words(n: usize) -> String {
        (0..n).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn empty_page_has_no_chunks() {
        assert!(chunks_of("  \n ").is_empty());
    }

    #[test]
    fn exact_chunk_size_is_single_chunk() {
        assert_eq!(chunks_of(&words(200)).len(), 1);
    }

    #[test]
    fn overlap_between_chunks() {
        let c = chunks_of(&words(201));
        assert_eq!(c.len(), 2);
        assert!(c[1].text.starts_with("w160 "));
    }

    #[test]
    fn hyphenation_and_ligatures() {
        let c = chunks_of("infor-\nmation \u{fb01}nd e-mail");
        assert_eq!(c[0].text, "information find e-mail");
    }
}
