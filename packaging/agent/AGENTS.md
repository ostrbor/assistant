# Assistant working folder

This folder belongs to the assistant app (live Spanish subtitles and answer hints for calls).
Your course PDFs are in `docs/<topic>/`; the app keeps their extracted text in `docs/<topic>/.cache/`.

## Skill: summarize-lesson-cache

When asked to summarize, condense or clean the lessons (clases) or their caches, read
`.claude/skills/summarize-lesson-cache/SKILL.md` and follow it exactly. It keeps the terms,
definitions and ideas, drops the noise, restores math/logic symbols that PDF extraction lost,
and keeps the cache files valid for the app (same header line, same pages).
Do not read or print `config.toml`: it holds API keys.
