# Zoom Assistant

A small desktop window for Linux that helps you follow a Spanish-language call (Zoom or anything else that plays through your speakers):

- **Live subtitles.** Press *Listen* and the other person's speech appears as Spanish text within about a second, with an English translation under each finished sentence. Press *Stop* and nothing is captured or sent anywhere, so you only pay while it is on.
- **Answer hints.** Press *Hint* and the app finds the latest question in the last ~60 seconds of the call, looks it up in your own PDF documents, and writes a 2–3 sentence answer in very simple Spanish (A1–A2) with an English translation. If the other person's sentence looks like a question, the hint is prepared in the background, so it is usually already there when you press the button.

The window stays on top of other windows. Your own microphone is never captured, only what plays through the speakers.

## How it works

```
speaker output ──► PulseAudio monitor ──► OpenAI Realtime (transcription) ──► subtitles
                                                                  │
                          finished sentence ──► LLM (translation) ──► English line
                                                                  │
                   question detected ──► BM25 search in your PDFs ──► LLM ──► hint
```

- **Transcription:** OpenAI Realtime API over a WebSocket (words arrive while the person is still speaking).
- **Translation and hints:** any OpenAI-compatible chat endpoint. The default is the [Hugging Face Inference Providers](https://huggingface.co/docs/inference-providers) router with open models (`gpt-oss`). Hints are sent to two providers in parallel and the first answer wins.
- **Document search:** no vector database. PDFs are converted with `pdftotext`, cut into ~200-word chunks and indexed in memory with BM25. Only the top 5 chunks go to the LLM, which keeps hints fast and cheap.
- **Reconnects:** if the connection drops, the app reconnects on its own (1, 2, 4, 8, then 10 s between attempts) and keeps the last ~5 s of audio. It stops only on errors retrying can't fix, such as an invalid API key.

## Requirements

- Linux with PipeWire or PulseAudio (the app records the default output's `.monitor` source via `pactl`)
- Rust toolchain (`rustup`). If `cargo` is not found, add `~/.cargo/bin` to your `PATH`.
- Development packages and tools:
  ```bash
  # Debian / Ubuntu
  sudo apt install libpulse-dev poppler-utils pkg-config
  ```
- An [OpenAI API key](https://platform.openai.com/api-keys) (transcription only)
- A [Hugging Face token](https://huggingface.co/settings/tokens/new?tokenType=fineGrained) with the *Make calls to Inference Providers* permission, and either purchased credits or a PRO subscription (translation and hints)

## Setup

```bash
export OPENAI_API_KEY=sk-...
export HF_TOKEN=hf_...

cargo build --release
```

Put your PDFs in subfolders of `docs/`, one subfolder per call or topic:

```
docs/
  interview_acme/
    cv.pdf
    offer.pdf
  project_x/
    architecture.pdf
```

Only text PDFs are supported (scanned images have no text to search). The extracted text is cached as `.txt` next to each PDF and refreshed when the PDF changes. The contents of `docs/` are git-ignored.

## Usage

```bash
cargo run --release                          # pick the document folder in the window
cargo run --release -- --topic project_x     # preselect a document folder
```

| Control | Action |
|---|---|
| **Listen / Stop** (`Space`) | Start or stop capturing and transcribing. Nothing is sent while stopped. |
| **Hint** (`H`) | Show the answer hint for the latest question. |
| **Docs** drop-down | Choose which folder under `docs/` to answer from. You can switch at any time; the index rebuilds in a moment. |
| **↻** | Rescan `docs/` after adding a folder. |
| **Clear** | Clear the transcript, hint and errors. |

If there is a single folder under `docs/`, it is selected automatically. Pick a folder **before** pressing *Hint*; without one you get `select a document folder first`.

Hint panel layout: the sources (file and page), the question as the app understood it (in English), the Spanish answer in large type, and its English translation below.

A typical call: start the app and choose the folder. Press `Space` when the conversation turns to you, press `H` after a question, and press `Space` again when the conversation moves on to someone else.

## Configuration

Everything is in `config.toml`:

| Key | Meaning |
|---|---|
| `docs_dir` | Root folder that contains the per-call subfolders. |
| `hint_window_secs` | How much recent speech the hint generator sees (default 60). |
| `hint_max_age_secs` | A hint prepared in the background older than this is regenerated when you press *Hint* (default 20). |
| `[transcribe] model, language` | Realtime transcription model and the spoken language. |
| `[llm] base_url, api_key_env` | Any OpenAI-compatible endpoint and the environment variable holding its key. |
| `[llm] translate_model` | Model for sentence translation (pick a fast one). |
| `[llm] hint_models` | Models tried in parallel for hints; the first to answer wins. A `:provider` suffix pins a Hugging Face provider, e.g. `openai/gpt-oss-120b:groq`. |

Model and provider names change over time. If you see an HTTP error in the red line under the toolbar, check the name in `config.toml` against the provider's current catalog. To use OpenAI instead of Hugging Face for the LLM, set `base_url = "https://api.openai.com/v1"`, `api_key_env = "OPENAI_API_KEY"` and an OpenAI model name.

## Cost

Rough figures; check current pricing. Transcription is the main cost, on the order of a few tenths of a dollar per hour while *Listen* is on. Translation and hints send only a few thousand tokens per request, which amounts to cents per call.

## Checking your setup

```bash
cargo run --release -- --test-audio
```
records 5 seconds of speaker output to `test.wav`. Play something while it runs and check that you can hear it in the file. If it is silent, see Troubleshooting.

```bash
cargo run --release -- --test-search --topic project_x "¿Cuántas personas tiene el equipo?"
```
prints the top matching chunks from a folder, which is handy for judging whether your documents are searchable.

```bash
cargo test
```
runs the unit tests and an integration test that drops a local WebSocket connection and checks that the client reconnects.

## Troubleshooting

- **No subtitles, `test.wav` is silent.** The app records the monitor of the *default* output device. Make sure Zoom plays through the default output (check with `pactl get-default-sink`) or change the default in your sound settings.
- **Your own voice is not transcribed.** Only speaker output is captured, not the microphone. That is intended.
- **`environment variable … is not set`.** Export `OPENAI_API_KEY` and `HF_TOKEN` in the shell that launches the app.
- **`Transcription: HTTP error: 401`.** The OpenAI key is invalid or has no access to the Realtime API. The app stops instead of retrying.
- **Translation shows `[translation error: …]`.** Check `HF_TOKEN`, your credits, and the model name in `config.toml`.
- **`No text found in <folder>`.** The PDFs are scans without a text layer.
- **Hints miss the right passage.** Search is keyword-based (BM25 with Spanish stemming), so it works best when the question and the document use the same words. Splitting very long documents into topical folders helps.
- **Status says `Reconnecting…`.** The connection dropped and the app is retrying; subtitles resume by themselves. The red line shows the reason.
