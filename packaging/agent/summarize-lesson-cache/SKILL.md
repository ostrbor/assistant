---
name: summarize-lesson-cache
description: Condense the extracted-text cache of course PDFs used by the assistant app (~/assistant/docs/<topic>/.cache/*.pdf.txt) into a short summary of terms, definitions and main ideas, and restore the math/logic symbols that PDF extraction lost. Use when asked to summarize, condense or clean the lesson caches (clases) of the assistant app.
argument-hint: <topic folder, e.g. algebra> [pdf name or number, e.g. clase-04]
---

# Summarize lesson caches for the assistant app

## What you are editing
The assistant app extracts the text of every PDF in `~/assistant/docs/<topic>/` once and stores it in
`~/assistant/docs/<topic>/.cache/<pdf name>.pdf.txt`. During a call it searches these texts and
builds short answer hints in Spanish from the best matching excerpts, citing `file p.N`.
The user wants those texts reduced to what matters: **terms, definitions, rules, ideas**.
Noise, exercises, repeated material and details that add nothing are removed, and symbols lost in
extraction are put back.

File format (the app depends on it):

```
v1 1023359 1792092973096341000      <- line 1: header, identifies the PDF. NEVER change it.
<text of page 1>\f<text of page 2>\f...   <- pages separated by a form feed (\f, 0x0C)
```

The PDF is `~/assistant/docs/<topic>/<pdf name>.pdf` (the cache name is the PDF name plus `.txt`).
Page N of the cache is page N of the PDF.

## Hard rules (breaking them breaks the app)
1. Keep line 1 (the header) byte for byte, or the app discards the cache and re-extracts the PDF.
2. Keep **exactly the same number of pages**: same number of `\f` separators. Page numbers in the
   hint sources come from them. Never merge or drop pages.
3. Never leave a page empty (the app flags the file as having "pages with no text"). A page with
   nothing worth keeping gets exactly **one line** in this stub format (the app recognizes it and
   leaves the page out of the search, so it never competes with real content):
   - `<short description> (sin teoría).`, e.g. `Ejercicios de práctica (sin teoría).`,
     `Portada de la clase 04 (sin contenido teórico).`, `Soluciones de ejercicios (sin teoría nueva).`
     The line must contain `(sin teoría` or `(sin contenido teórico` (any ending after that is fine),
     and nothing else on the page. Never put this marker on a page that has real content.
   - `Ver p.N.` for a page whose content moved to page N.
4. Stay in the language of the source (Spanish). Do not translate. Keep the course's own
   terminology and spelling exactly (accents, ñ, ¿ ¡), because search matches these words.
5. UTF-8 only. Use real Unicode symbols, never LaTeX (`∧`, not `\wedge`).
6. Edit only the `.cache/*.pdf.txt` files you were asked about. Never modify the PDFs, the app,
   or its config (`config.toml` holds API keys: do not read or print it).
7. Make the backup before the first write (step 1 below). Never overwrite an existing backup.

## Workflow, one file at a time
Let `C` = the cache file, `P` = its PDF. If the user gave only a topic, list the files and
process them one by one in order, reporting after each. If they gave a name or number
(`clase-04`), process only that file.

1. **Backup**: `python3 -I ~/assistant/.claude/skills/summarize-lesson-cache/scripts/cache_tool.py backup "C"`
   (copies the original to `<topic>/.cache-orig/`; it never overwrites an existing copy).
   Always read the **original** from the backup if you run the skill again, so repeated runs do
   not summarize a summary.
2. **Find damaged places**: `python3 -I ~/assistant/.claude/skills/summarize-lesson-cache/scripts/cache_tool.py gaps "C"`.
   It lists pages where symbols are missing. It cannot see every case, so use it as a guide.
3. **Read** the cache text, page by page.
4. **For every page with gaps, look at the page itself** and fill the symbols from the image:
   - Claude Code: Read the PDF with `pages: "N"` (one page at a time).
   - Any other agent: `pdftoppm -f N -l N -png -r 110 "P" /tmp/page` and open the PNG.
   The image is the truth. Do not rely on the text for symbols.
   If you cannot view images, see "If a symbol cannot be determined" below.
5. **Rewrite each page** following "What to keep", "What to drop", "Repeats" and "Style".
   Remove exercises and repeated material, but keep every page (a short note instead of an empty page).
6. **Write** the new file to `C` (header line first, then the pages joined by `\f`). Write it with a
   script or a tool that preserves the `\f` characters; check that your tool did not turn them
   into spaces or newlines.
7. **Verify**: `python3 -I ~/assistant/.claude/skills/summarize-lesson-cache/scripts/cache_tool.py verify "<topic>/.cache-orig/<name>" "C"`.
   It must print `OK` (same header, same page count, no empty pages, no U+FFFD). If it prints
   `FAILED`, fix the file and verify again. Words 25-40% of the original is a good target.

## What to keep
- Definitions, with the defined term exactly as in the course ("Una fórmula bien formada (f.b.f.) es ...").
- Rules and their numbering ("Regla 1", "Regla 2"...), theorems, properties, laws, equivalences.
- Notation and symbols, with their meaning (conjunción, disyunción, condicional, bicondicional, negación...).
- Truth tables in compact form, and the key worked examples that illustrate a rule (one or two per rule,
  keep the final result). A worked example is part of the explanation; an exercise for the student is not.
- Names and acronyms that appear in the course (f.b.f., SE, tautología, contradicción...).
- Anything the teacher marks as important.

## What to drop
- Cover pages, tables of contents, page footers/headers, logos ("ISSD", "Inteligencia Artificial").
- **Exercises and tasks for the student**: "Ejercicio", "Actividad", "Practicá", "Resolvé", "Para pensar",
  lists of formulas to classify or symbolize, self-assessment questions, homework, and their answer keys.
  Drop them entirely. If a page has only exercises, write one line (`Ejercicios de práctica (sin teoría).`).
  Keep a task only when it states a rule or definition that appears nowhere else.
- **Repeats** (see the next section).
- Chit-chat, motivation, long transitions, "Pero ¿qué estamos queriendo formalizar?"-style filler.
- Garbled leftovers from extraction (stray letters, broken numbering, dot leaders `.........`).

## Repeats
The same content often appears more than once. Keep it once, in the best place:
- **Inside a file**: a definition restated later ("Recordemos que..."), a summary box that repeats the
  page before, the same example solved twice, a table copied on two pages. Keep the clearest, most
  complete version on the earliest page it belongs to; on the other pages leave a single line or a
  short note (`Ver p.6.`), never an empty page.
- **Across files** (lessons recap earlier lessons): before rewriting a class, skim the earlier classes
  of the same folder (their `.cache/` text, already summarized or not). A definition, rule or table
  that an earlier class already contains is not repeated; keep only what is new in this class, with
  a short pointer when the context needs it (`Retoma la definición de f.b.f. de la clase 04.`).
  Never drop something that the earlier class does not actually contain, and never remove a term
  that this class defines differently or extends: keep the new part.
- Near-duplicates (the same rule worded twice) collapse into the clearer wording; keep the course's
  own term for it.
- When unsure whether two passages say the same thing, keep both.

## Style
- Compact, factual, one idea per line. Plain text, light formatting (no tables, no heavy Markdown),
  because the text is cut into ~200-word chunks for search: each page's text should make sense
  on its own and include the key terms in full words (write "conjunción" even when you also use ∧).
- Start a page with its topic when useful (`Sintaxis del lenguaje formal.`).
- Formulas in plain Unicode on their own line: `K = {¬} ∪ {∧, ∨, →, ↔}`.

## Symbols lost in extraction
The extractor keeps symbols that are real text in the PDF font: `¬ ⇒ ⇔ → ≤ ≠ ≥ ± ° ² ³`, subscripts
and superscripts. It **loses** symbols that the PDF draws as graphics: typically `∧ ∨ ∪ ∩ ∈ ∉ ⊂ ⊆ ⊄
∀ ∃ ∅` and sometimes arrows. They disappear without a trace, so you find only holes:

| What you see in the cache | What happened |
|---|---|
| `K = {¬}     {   ,   ,   ,   }` | the set of connectives lost its symbols: `K = {¬} ∪ {∧, ∨, →, ↔}` |
| `( p   q )`, `(¬p   ¬q)` (wide hole between operands) | a connector `∧ ∨ → ↔` is missing |
| `El símbolo “ ”, utilizado para denotar la intersección` | the symbol in quotes is missing (here `∩`) |
| `donde ηη    {   ,  ,   ,  }` | `η ∈ {∧, ∨, →, ↔}`: both `∈` and the set contents are missing |
| `Conjunción “ y ”`, `Disyunción “ o ”` in a table | the symbol column of the table is missing (`∧`, `∨`) |

Symbol meanings in this course (logic): `∧` conjunción "y", `∨` disyunción "o", `→` condicional
"entonces", `↔` bicondicional "si y solo si", `¬` negación; sets: `∪` unión, `∩` intersección,
`∈`/`∉` pertenece/no pertenece, `⊂`/`⊆` inclusión, `∅` conjunto vacío, `∀`/`∃` cuantificadores.

### How to restore
1. Open the page image (step 4) and read the formula or table there.
2. Write the formula back with the correct symbols, keeping the variables and parentheses from the text.
3. Cross-check with the surrounding sentence ("(p ∧ q) simboliza 'hoy llueve y no saldré a pasear'").
4. In summaries it is fine to rewrite a damaged formula in its clean form.

### If a symbol cannot be determined
Never guess a symbol. In order of preference:
1. Replace the formula with its description in words: `la conjunción de p y q`.
2. Leave out that example if it adds nothing.
3. Keep the place with an explicit marker `[símbolo no recuperado]` and mention it in the final report.
A wrong symbol (∧ instead of ∨) is worse than a missing one: the hints would teach the user the
wrong rule.

## Final report to the user
Short: for each file, pages, words before -> after, how many damaged places you restored from
the page image, how many remain unresolved (with page numbers), the backup folder, and these two notes:
- To undo a file: `cp ~/assistant/docs/<topic>/.cache-orig/<name> ~/assistant/docs/<topic>/.cache/<name>`.
- The app reads the caches when a folder is selected: re-select the folder in the app (or restart it)
  to use the new texts. If a PDF is replaced or changed, its cache is regenerated and the summary
  is lost: re-run this skill (from the backup copy).
