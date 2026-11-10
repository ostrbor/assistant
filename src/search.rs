use crate::docs::Chunk;
use rust_stemmers::{Algorithm, Stemmer};
use std::collections::HashMap;
use unicode_normalization::UnicodeNormalization;

const K1: f32 = 1.2;
const B: f32 = 0.75;

const STOPWORDS: &[&str] = &[
    "a", "al", "algo", "como", "con", "cual", "cuando", "de", "del", "donde", "el", "ella", "ellos", "en", "es",
    "esa", "ese", "esta", "este", "esto", "la", "las", "le", "les", "lo", "los", "me", "mi", "mis", "muy", "no",
    "nos", "o", "para", "pero", "por", "porque", "que", "quien", "se", "ser", "si", "sin", "su", "sus", "te",
    "tu", "tus", "un", "una", "uno", "unos", "y", "yo", "usted", "ustedes", "hay", "ha", "han", "he", "ya",
];

pub struct Index {
    chunks: Vec<Chunk>,
    postings: HashMap<String, Vec<(usize, u32)>>,
    lens: Vec<u32>,
    avg_len: f32,
    stemmer: Stemmer,
}

impl Index {
    pub fn build(chunks: Vec<Chunk>) -> Self {
        let stemmer = Stemmer::create(Algorithm::Spanish);
        let mut postings: HashMap<String, Vec<(usize, u32)>> = HashMap::new();
        let mut lens = Vec::with_capacity(chunks.len());
        for (i, c) in chunks.iter().enumerate() {
            let terms = normalize(&stemmer, &c.text);
            lens.push(terms.len() as u32);
            let mut tf: HashMap<String, u32> = HashMap::new();
            for t in terms {
                *tf.entry(t).or_default() += 1;
            }
            for (t, n) in tf {
                postings.entry(t).or_default().push((i, n));
            }
        }
        let avg_len = lens.iter().sum::<u32>() as f32 / lens.len().max(1) as f32;
        Self { chunks, postings, lens, avg_len, stemmer }
    }

    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    /// `query` is a list of (text, weight). The question should weigh more than the other utterances.
    pub fn search(&self, query: &[(&str, f32)], k: usize) -> Vec<(f32, &Chunk)> {
        let n = self.chunks.len() as f32;
        let mut scores: HashMap<usize, f32> = HashMap::new();
        for (text, weight) in query {
            for term in normalize(&self.stemmer, text) {
                let Some(posts) = self.postings.get(&term) else { continue };
                let df = posts.len() as f32;
                let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                for &(doc, tf) in posts {
                    let tf = tf as f32;
                    let norm = 1.0 - B + B * self.lens[doc] as f32 / self.avg_len;
                    *scores.entry(doc).or_default() += weight * idf * tf * (K1 + 1.0) / (tf + K1 * norm);
                }
            }
        }
        let mut v: Vec<_> = scores.into_iter().collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1));
        v.into_iter().take(k).map(|(i, s)| (s, &self.chunks[i])).collect()
    }
}

/// Lowercase, no diacritics, no stopwords, stemmed.
fn normalize(stemmer: &Stemmer, text: &str) -> Vec<String> {
    let flat: String = text
        .nfd()
        .filter(|c| !('\u{300}'..='\u{36f}').contains(c))
        .collect::<String>()
        .to_lowercase();
    flat.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 1 && !STOPWORDS.contains(w))
        .map(|w| stemmer.stem(w).to_string())
        .collect()
}
