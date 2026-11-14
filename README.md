# Zoom Assistant

A small desktop window for Linux that helps you follow a Spanish-language call (Zoom or anything else that plays through your speakers):

- **Live subtitles.** Press *Listen* and the other person's speech appears as Spanish text within about a second, with an English translation under each finished sentence. Your own speech appears too, marked `Me:` and without translation. Press *Stop* and nothing is captured or sent anywhere, so you only pay while it is on. Silence is not sent either.
- **Answer hints.** Press *Hint* and the app finds the latest question in the last ~60 seconds of the call, looks it up in your own PDF documents, and writes a 2–3 sentence answer in very simple Spanish (A1–A2) with an English translation. Because it also sees what you already said, it does not answer a question you have just answered, and suggests a continuation instead. If the other person's sentence looks like a question, the hint is prepared in the background, so it is usually already there when you press the button.

The window stays on top of other windows. The other person is captured from what plays through the speakers, you from your default microphone. **Use headphones**: through speakers, their voice leaks into your microphone and shows up as `Me:` lines. To turn the microphone off, set `capture_mic = false`.

## How it works

```
speaker output ──► sound-server monitor ─┐  (silence dropped)
microphone ──────► default input ──────┴► OpenAI Realtime (one session each) ──► subtitles
                                                                  │
                          finished sentence ──► LLM (translation) ──► English line
                                                                  │
                   question detected ──► BM25 search in your PDFs ──► LLM ──► hint
```

- **Transcription:** OpenAI Realtime API over a WebSocket (words arrive while the person is still speaking).
- **Translation and hints:** any OpenAI-compatible chat endpoint. The default is the [Hugging Face Inference Providers](https://huggingface.co/docs/inference-providers) router with open models (`gpt-oss`). Hints are sent to two providers in parallel and the first answer wins.
- **Document search:** no vector database. PDF text is extracted by the app itself, cut into ~200-word chunks and indexed in memory with BM25. Only the top 5 chunks go to the LLM, which keeps hints fast and cheap.
- **Reconnects:** if the connection drops, the app reconnects on its own (1, 2, 4, 8, then 10 s between attempts) and keeps the last ~5 s of audio. It stops only on errors retrying can't fix, such as an invalid API key.

## Requirements

- Ubuntu 22.04 or newer (or another Linux with glibc ≥ 2.35) with PipeWire or PulseAudio (the app talks to the sound server directly and records the default output's `.monitor` source). Nothing else has to be installed: audio and PDF handling are built into the binary.
- Only to build from source: Rust ≥ 1.88 via `rustup` (the apt `rustc` on Ubuntu is too old). If `cargo` is not found, add `~/.cargo/bin` to your `PATH`. No development packages are needed.
- An [OpenAI API key](https://platform.openai.com/api-keys) (transcription only)
- A [Hugging Face token](https://huggingface.co/settings/tokens/new?tokenType=fineGrained) with the *Make calls to Inference Providers* permission, and either purchased credits or a PRO subscription (translation and hints)

## Install the .deb (recommended)

Download `assistant_X.Y.Z_amd64.deb` from the GitHub Releases page and install it:

```bash
sudo apt install ./assistant_X.Y.Z_amd64.deb
assistant            # or start "Zoom Assistant" from the applications menu
```

On the first start the app creates `~/assistant/` with a default `config.toml` and an empty `docs/`. Open `~/assistant/config.toml`, put your keys into `[transcribe] api_key` (OpenAI) and `[llm] api_key` (Hugging Face), and restart the app. Until the keys are set, the app shows a message saying so. Put your PDFs into `~/assistant/docs/<topic>/`. The config file is readable only by you.

**Logs.** When started from the menu (no terminal), warnings and errors are written to `~/assistant/assistant.log` (rotated to `assistant.log.old` at about 1 MB). When started from a terminal, they are printed there instead.

The app updates itself (see *Auto-update* below). `/usr/bin` is read-only for you, so updates are stored in `~/assistant/bin/assistant`, and the installed `assistant` command hands over to that copy automatically. Installing a newer .deb later also works: the newest of the two versions runs. Your `~/assistant/` is kept, also when you remove the package.

## Install from a release (no Rust needed)

```bash
tar xzf assistant-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz   # from the GitHub Releases page
./assistant
```

Release binaries are built on Ubuntu 22.04, so they run on 22.04 and newer. `config.toml` is read from the current folder, or from the folder of the binary if there is none there.

**Auto-update.** On start, release builds check the latest GitHub release of `ostrbor/assistant`. If it is newer, the app downloads it, replaces its own binary and restarts. Your `config.toml` and `docs/` are not touched. If the binary's folder is not writable (a .deb install), the new version is placed in `~/assistant/bin/` instead. If you are offline, the check is skipped with a warning. To skip it, use `./assistant --no-update` or `ASSISTANT_NO_UPDATE=1`. Builds from source (`cargo run`) never update themselves.

**Releasing.** Bump `version` in `Cargo.toml`, then `git tag vX.Y.Z && git push --tags`. GitHub Actions builds the binary and publishes the release.

## Setup

```bash
# Keys go into config.toml ([transcribe] api_key, [llm] api_key) or, if those are empty, into
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
| `[transcribe] api_key` | OpenAI API key. If empty, the variable named in `api_key_env` is used. |
| `[llm] api_key` | Key of the LLM endpoint (Hugging Face token by default). If empty, the variable named in `api_key_env` is used. |
| `[transcribe] model, language` | Realtime transcription model and the spoken language. |
| `[transcribe] silence_threshold, mic_silence_threshold` | RMS level (16-bit samples, default 300 and 500) below which audio from the speakers / the microphone counts as silence and is not sent. Raise it if background noise keeps the stream open, lower it if quiet speech is cut off. |
| `[transcribe] capture_mic` | `true` (default) also transcribes your own voice, so hints know what you already said. |
| `[llm] base_url, api_key_env` | Any OpenAI-compatible endpoint and the environment variable holding its key. |
| `[llm] translate_model` | Model for sentence translation (pick a fast one). |
| `[llm] hint_models` | Models tried in parallel for hints; the first to answer wins. A `:provider` suffix pins a Hugging Face provider, e.g. `openai/gpt-oss-120b:groq`. |

Model and provider names change over time. If you see an HTTP error in the red line under the toolbar, check the name in `config.toml` against the provider's current catalog. To use OpenAI instead of Hugging Face for the LLM, set `base_url = "https://api.openai.com/v1"`, `api_key_env = "OPENAI_API_KEY"` and an OpenAI model name.

## Cost

Rough figures; check current pricing. Transcription is the main cost, on the order of a few tenths of a dollar per hour of audio sent while *Listen* is on. Silence is not sent, and the microphone is a second stream (only your own speech counts), so the total depends on how much is actually said. Translation and hints send only a few thousand tokens per request, which amounts to cents per call.

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

## Interview rehearsal (a second machine)

To test the whole setup end to end, run the same binary on another machine as a simulated interviewer and talk to it over a real Zoom or Google Meet call. The network between the machines does not matter: the call goes through Zoom/Meet.

```
machine B (interviewer)                               machine A (you)
LLM text ─► TTS ─► sink "interviewer_out"                      ┌► assistant: subtitles + hints
                   └► "Interviewer_mic" ─► Zoom/Meet ─────────┘
your voice ◄─ Zoom/Meet output ◄─ default sink .monitor ─► realtime transcription ─► LLM
```

On machine B (Linux with PipeWire/PulseAudio, a graphical session, the same `config.toml`, `docs/<topic>`, `OPENAI_API_KEY` and `HF_TOKEN`):

```bash
./assistant --interview --topic demo --persona persona.md --questions 5
```

1. The program creates a virtual microphone **Interviewer_mic** (a PulseAudio null sink plus a remapped source) and waits.
2. In Zoom or Meet on machine B, choose **Interviewer_mic** as the microphone and keep the speakers on the default output. Join the call, then press Enter.
3. The interviewer greets you and asks question 1. When you stop talking for about 3 s it reacts to your answer in a sentence and asks the next one. After the last answer it says goodbye. The dialogue is also printed in the terminal.

`persona.md` is free text, for example: *Eres Pedro, tech lead en Acme. Entrevistas a un backend developer para el proyecto X. Eres directo pero amable.* Without `--persona`, the `persona` from `[interview]` in `config.toml` is used. The questions are based on the PDFs in the chosen `docs/` folder.

Notes:
- Turn off "automatic microphone volume" and noise suppression in Zoom/Meet if the synthetic voice is cut.
- The interviewer waits up to 60 s for an answer, asks "¿Me escuchas?" once, and moves on if there is still silence.
- Remove the virtual devices afterwards: `pactl unload-module module-remap-source && pactl unload-module module-null-sink`.
- Cost of a 10-minute run is roughly 10-20 cents in total: speech synthesis, transcription of your answers, and a few LLM requests. Check current prices.

## Troubleshooting

- **No subtitles, `test.wav` is silent.** The app records the monitor of the *default* output device. Make sure Zoom plays through the default output (check with `pactl get-default-sink`) or change the default in your sound settings.
- **Your own voice is not transcribed.** Check `capture_mic` and `pactl get-default-source`; if the level is low, lower `mic_silence_threshold`. A missing microphone shows `Microphone lost, retrying…` while the speaker stream keeps working.
- **The other person's words appear as `Me:`.** Their voice leaks from the speakers into your microphone. Use headphones.
- **Subtitles cut off quiet speech, or the stream never goes quiet.** Tune `silence_threshold` / `mic_silence_threshold`.
- **`LLM timed out`.** The provider gave no first token within 15 s (or went quiet for 20 s). Press *Hint* again or check the provider.
- **`API key missing …` / `OpenAI API key is not set`.** Put the key into `config.toml` (`[transcribe] api_key`) and restart, or export `OPENAI_API_KEY` in the shell that launches the app.
- **`LLM token is not set`** (hint error, or `[translation error: …]` under subtitles). Set `[llm] api_key` in `config.toml` (or export `HF_TOKEN`, or the variable named in `[llm] api_key_env`) and restart.
- **`Transcription: HTTP error: 401`.** The OpenAI key is invalid or has no access to the Realtime API. The app stops instead of retrying.
- **Translation shows `[translation error: …]`.** Check `HF_TOKEN`, your credits, and the model name in `config.toml`.
- **`No text found in <folder>`.** The PDFs are scans without a text layer.
- **Hints miss the right passage.** Search is keyword-based (BM25 with Spanish stemming), so it works best when the question and the document use the same words. Splitting very long documents into topical folders helps.
- **Status says `Reconnecting…`.** The connection dropped and the app is retrying; subtitles resume by themselves. The red line shows the reason.
