# resources/tokenizer.json

A cl100k_base-equivalent BPE tokenizer in the `tokenizers` crate's native
JSON schema (vocab + string-form merges + byte-level pre-tokenizer using
cl100k_base's actual split regex). Embedded into the Rust binary at compile
time via `include_str!` in `src/chunker.rs`, so token counting is exact and
fully offline at runtime — no network calls, no file-path configuration.

## Provenance

- **Source data**: `data/cl100k_base.tiktoken` from
  [niieani/gpt-tokenizer](https://github.com/niieani/gpt-tokenizer)
  (MIT licensed), fetched via `raw.githubusercontent.com`. This is a mirror
  of OpenAI's own published rank table for the `cl100k_base` encoding (the
  encoding GPT-3.5/GPT-4-era models and many embedding models use) — a
  functional numeric data table (byte-sequence → merge rank), not creative
  content.
- **Why not fetch directly from HuggingFace or OpenAI**: neither
  `huggingface.co` nor OpenAI's blob storage host is reachable from this
  build sandbox's network allowlist. GitHub-hosted mirrors of the same
  public data are reachable, so that's what was used.
- **Conversion**: `tiktoken`'s rank file only records `(byte sequence →
  rank)`, not the explicit merge pairs the `tokenizers` crate's BPE model
  needs. Merge pairs are reconstructed with the standard technique described
  in [openai/tiktoken#60](https://github.com/openai/tiktoken/issues/60#issuecomment-1499977960)
  and used by every community HF conversion of tiktoken encodings: re-run
  greedy BPE merge search on each token's raw bytes, restricted to only
  merges with a strictly lower rank than the token itself. For a token that
  really did come from one BPE merge step, this always resolves to exactly
  2 parts — verified here by an assertion in the converter for all 100,256
  tokens.
- **Verification**: after conversion, encoding `"hello world!"` with the
  embedded tokenizer produces `[15339, 1917, 0]`, matching the publicly
  documented real cl100k_base output for that exact string (see e.g. the
  `DWDMaiMai/tiktoken_cl100k_base` model card). This is the same check
  anyone converting this data would run to confirm correctness.
- **Special tokens**: `<|endoftext|>` (100257), `<|fim_prefix|>` (100258),
  `<|fim_middle|>` (100259), `<|fim_suffix|>` (100260), `<|endofprompt|>`
  (100276) — cl100k_base's own published constants, included for
  completeness; irrelevant to plain-text token counting for RAG chunking.

## Regenerating this file

`convert_tiktoken_to_hf.py` (kept alongside this README, one directory up
from `rust-core/`) does the conversion. Re-run it if you want to swap in a
different encoding (e.g. `o200k_base` for newer OpenAI models) — just point
`SRC` at the corresponding `.tiktoken` rank file and update
`CL100K_SPLIT_PATTERN` to that encoding's actual split regex.

# standard_font_metrics.rs

Flattened `[width_per_char_code; 256]` tables for the 14 PDF "standard"
fonts (Helvetica/Times/Courier families), across StandardEncoding and
WinAnsiEncoding — used by `font_metrics.rs` whenever a font resource has no
`/Widths` array of its own (Track A Priority 1).

## Provenance

- **Source**: `src/core/metrics.js` and `src/core/encodings.js` from
  [mozilla/pdf.js](https://github.com/mozilla/pdf.js) (Apache License 2.0),
  fetched via `raw.githubusercontent.com`. metrics.js's tables are
  themselves a derivative of Adobe's public "Font Metrics for the PDF Core
  14 Fonts" package, redistributable per Adobe's own accompanying notice —
  the same terms every PDF library (poppler, PDFBox, pdfminer.six, jsPDF,
  reportlab itself, ...) relies on to bundle the equivalent tables.
- **Why not embed real Widths for these fonts instead**: there's nothing to
  embed — that's the point of a "standard" font. A PDF referencing
  `/BaseFont /Helvetica` with no `/Widths` array is relying entirely on
  every conforming reader already knowing these metrics. Our own
  reportlab-generated test fixture does exactly this.
- **Verification**: extracted widths checked against well-known published
  AFM values (Helvetica: space=278, 'A'=667, 'a'=556, 'W'=944 — all exact).
- **Regenerating**: `tools/generate_standard_font_metrics.py`, after fetching
  the two source files as documented in that script's docstring.

