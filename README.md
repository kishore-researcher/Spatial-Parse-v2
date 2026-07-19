# RAG Ingestion Pipeline — Deterministic PDF → Markdown Engine

A zero-AI, deterministic preprocessing pipeline for enterprise PDF-to-Markdown
ingestion, built as a Go orchestration layer around a Rust processing core.

**Status: working v1 foundation, not a finished production system.** Every
algorithm described below is real, compiles, and is covered by passing unit
tests plus an end-to-end run against a generated 25-page test PDF (25/25
pages processed, 4 header/footer clusters correctly redacted, 1 garbled page
correctly quarantined, 6 context-aware chunks correctly emitted). It is a
solid foundation to build on, not a drop-in replacement for a team spending
several weeks hardening this against the full variety of real-world PDFs.

## Quick start

```bash
# 1. Build the Rust core
cd rust-core
cargo build --release
cargo test --release   # 15 unit tests

# 2. Run it directly against a PDF (writes NDJSON to stdout, logs to stderr)
./target/release/rag_ingestion_core /path/to/document.pdf my_document_id

# 3. Or run it behind the Go HTTP orchestrator
cd ../go-orchestrator
go build -o orchestrator .
RUST_CORE_PATH=../rust-core/target/release/rag_ingestion_core ./orchestrator
# in another shell:
curl -X POST http://localhost:8080/ingest \
  -F "file=@/path/to/document.pdf" \
  -F "document_id=my_document_id"
```

The HTTP response is streamed newline-delimited JSON: `page_fidelity` records
as each page is scored, `log` lines for redaction stats and quarantine
alerts, `chunk` records as they're emitted, and one final `summary` record.

## Architecture

```
Incoming PDF (multipart upload)
        │
        ▼
Go Orchestrator (net/http, stdlib only)
  - multipart.Reader streams the upload straight to a temp file
    (never buffers the whole file in the Go process's own heap)
  - spawns the Rust core as a subprocess, piping stdout/stderr back
    to the HTTP client as NDJSON, flushed line-by-line
        │
        ▼ (file path via argv, results via stdout/stderr pipes)
Rust Processing Core
  - pdf_extract.rs         -- content-stream interpreter -> positioned text elements,
                              real per-glyph widths (font_metrics.rs), vector line/rect tracking
  - font_metrics.rs        -- Track A Priority 1: /Widths, /W (CID), standard-14 fallback
  - standard_font_metrics.rs -- vendored core-14 font width tables (generated, see below)
  - table_grid.rs          -- Track A Priority 2: ruled-table lattice detection from vector lines
  - column_layout.rs       -- Track A Priority 3: X-axis density clustering for column reading order
  - spatial_filter.rs      -- Pain Point 1: header/footer frequency filter (R-tree + NLD)
  - fidelity.rs            -- Pain Point 2: structural fidelity scoring + quality gates
  - markdown_render.rs     -- turns kept elements into Markdown (column-aware; heading/table heuristics)
  - chunker.rs             -- Pain Point 3: AST-based semantic chunking, real offline BPE tokenizer
  - main.rs                -- wires it all together, emits NDJSON
```

### Why subprocess pipes instead of FFI/WASM

The spec's diagram shows "Zero-Copy Memory Stream Transfer" via FFI/WASM
between Go and Rust. We used a subprocess + stdin/stdout/stderr pipes
instead, deliberately:

- No cgo toolchain coupling — cgo requires matching ABI/allocator
  assumptions between Go and Rust and complicates cross-compilation.
- Process isolation — a panic or OOM in the Rust core (a 500-page
  worst-case document is exactly the scenario where this matters) can't take
  the Go server down with it.
- It's a straightforward drop-in to swap later for a `cdylib` + cgo binding,
  or for gRPC to a separate Rust worker pool, without changing the
  HTTP-facing contract.

This is the single biggest deviation from the literal spec diagram, so it's
worth being upfront about it rather than silently substituting it.

## Design corrections made to the literal spec

A few places in the spec, taken completely literally, would have produced a
system that either couldn't work or would silently misbehave. Each is
implemented as the corrected version, not the literal one, and documented
in the corresponding source file:

1. **Pass 1/Pass 2 is not fully "single-pass streaming."** A global
   frequency-ratio redaction algorithm mathematically requires having seen
   every page before you know what to redact on page 1 — you cannot know a
   ratio over N pages having only seen 1. We resolve this by keeping Pass 1
   deliberately lightweight: it extracts only header/footer-zone candidate
   strings (a few hundred short strings for a 500-page document), never full
   page bodies, so the "no global DOM in RAM" spirit holds even though two
   passes over the byte stream are unavoidable for this specific algorithm.
   See `spatial_filter.rs`.

2. **Fidelity scoring must not penalize intentional redaction.** The spec's
   literal `C_raw` ("alphanumeric characters from native PDF binary reading")
   would count header/footer text that Pain Point 1 *correctly* stripped as
   "lost content," flagging good pages as broken. `C_raw` is computed only
   over the non-redacted (kept) elements, so the score measures genuine
   extraction/rendering loss, not deliberate cleanup. See `fidelity.rs`.

3. **A header appearing mid-chunk must force a chunk boundary — but only
   if its text actually changes.** The literal pseudocode only flushes a
   chunk on token overflow, which lets two unrelated sections silently merge
   into one chunk carrying only the *first* section's `parent_hierarchy`
   (verified by a failing test before the fix). The naive fix — flush on
   every header line — instead badly over-fragments documents where the same
   sub-heading is printed on every page of a long section (e.g. a running
   "Appendix Notes" header), turning one section into dozens of near-empty
   chunks. The implemented behavior only starts a new section (and flushes)
   when the header text actually differs from what's already at that
   context depth. See `chunker.rs`.

## Offline tokenizer (Priority 3, implemented)

`chunker::get_exact_token_count` now uses the real `tokenizers` crate with a
cl100k_base-equivalent BPE vocabulary embedded via `include_str!` — no
runtime network calls, no file-path configuration. See
`rust-core/resources/README.md` for exactly where the vocabulary data came
from and how it was verified.

**One correction to the sprint spec's snippet:** the proposed
implementation called `Tokenizer::from_str(TOKENIZER_DATA).unwrap()` *inside*
`get_exact_token_count` itself, meaning every single call would re-parse the
full ~100k-vocab/~100k-merge JSON document from scratch. That's fine once;
it's a serious performance bug called once per AST node across a whole
document. The tokenizer is now parsed once into a `std::sync::OnceLock` and
reused for the process's lifetime — one ~50ms parse instead of paying that
cost per paragraph/table/list node.

**Getting `tokenizers` to compile at all was the bulk of the work here.**
This sandbox's `rustc` is pinned at 1.75.0 (Ubuntu 24.04's packaged
version, and the only one reachable through this environment's network
allowlist — no `rustup`/`static.rust-lang.org` access to install a newer
one). `tokenizers` 0.19's dependency tree has a hard MSRV floor around
rustc 1.80 with no way to pin around it (`rayon` ≥1.10 unconditionally
requires `rayon-core` ≥1.13, which itself needs 1.80+). Downgrading to
`tokenizers = "0.15"` with `default-features = false, features = ["onig"]`
avoids that specific wall, but still required pinning several transitive
dependencies to older, 1.75-compatible versions in `Cargo.lock`:
`monostate` 0.1.12, `rayon` 1.8.1, `rayon-core` 1.12.1,
`unicode-segmentation` 1.11.0, `onig` 6.4.0 (which also downgrades
`bitflags` to 1.3.2). **If you build this on a normal, non-sandboxed
machine with a current stable `rustc` (1.80+), you almost certainly don't
need any of these pins** — feel free to `cargo update` and go back to plain
`tokenizers = "0.19"` with default features once you're not fighting this
sandbox's constraints.

## Track A: The High-Fidelity Layout Layer (implemented)

### Priority 1 — real font widths

`font_metrics.rs` parses a page's actual font resources and resolves, per
font, one of:
1. An explicit `/Widths` array (embedded/subset fonts — the common case for
   anything produced by Word, LaTeX, or a browser's print-to-PDF).
2. A **standard-14 core font fallback** when there's no `/Widths` array at
   all — which turned out to be essential, not optional: our own
   reportlab-generated test fixture references `Helvetica`/`Helvetica-Bold`
   directly with zero embedded width data, exactly like a large fraction of
   real-world "quick report" PDFs. Without this fallback, "parse /Widths"
   would have done nothing for our own test document. The metrics table is
   generated (`tools/generate_standard_font_metrics.py`) from PDF.js's
   published core-font tables (Apache-2.0) and verified against known AFM
   values (Helvetica space=278, 'A'=667, 'a'=556 — all confirmed exact).
3. A `/W` array for composite/CID fonts (both run-length forms from the PDF
   spec), assuming 2-byte Identity-H/V codes — the common case, though not
   the only possible CMap structure (see limitations below).
4. A last-resort flat estimate if none of the above apply.

### Priority 2 — vector graphics operator tracking

`pdf_extract.rs` now tracks `m`/`l`/`re`/`c`/`v`/`y`/`h` path construction
and `S`/`s`/`f`/`F`/`f*`/`B`/`B*`/`b`/`b*`/`n` painting operators, producing
line segments in device space. `table_grid.rs` clusters these into
horizontal/vertical rule positions and detects a genuine lattice (rules
that mutually intersect at least twice, not just any two stray lines
anywhere on the page).

**Correction to the ticket's framing:** ruled-line detection gives
high-confidence, exact grid dimensions *for tables that actually have drawn
borders* — it does not make table detection "100% accurate" in general,
because a very large share of real-world tables (including our own page-2
fixture) have **no drawn borders at all** and rely purely on column
alignment. `fidelity::detect_pdf_grid` therefore tries the ruled-line
detector first and only falls back to the spatial-alignment heuristic when
no ruled grid is found — the two are complements, not one superseding the
other. Verified end-to-end against a real drawn-border table (page 26 of
the test fixture): `t_matrix: 1.0`, ruled grid detected via actual `re`/`l`/`S`
operators, not the heuristic.

### Priority 3 — X-axis column clustering

`column_layout.rs` sweeps a page's elements in fixed-width x-buckets,
looking for a run of buckets that stays empty across most lines (a real
column gutter) rather than the position-varying gaps normal word/sentence
spacing produces. Deliberately conservative — a false-positive column split
is a worse failure mode than occasionally missing a real one, since it
would scramble an ordinary single-column document's reading order.

**A real bug found and fixed during end-to-end testing, not just unit
tests:** the first working version correctly split genuine 2-column prose
into two columns, but also misread our page-2 **borderless 4-column data
table** ("Quarter | Product X | Product Y | Total") as four separate
reading columns — rendering each column of *numbers* top-to-bottom as if it
were a wrapped paragraph, destroying the table's row structure entirely. A
data table's columns and a magazine layout's columns are structurally
identical to a naive X-axis sweep (vertically-aligned content, consistent
gaps) — the fix adds a content-based check: genuine text columns average
several words per line (prose), while table cells average close to one
short value per line. Both the original bug and the fix are captured as
permanent regression tests (`borderless_table_is_not_misread_as_reading_columns`,
`reproduces_real_world_page27_coordinates`). This is exactly the kind of
interaction bug that only surfaces when priorities are tested *together*
against realistic documents rather than in isolation.

**Known limitation:** column detection assumes the whole page follows one
consistent structure. A page mixing a full-width banner/headline with a
2-column body below it isn't specially handled — enough full-width lines
mixed into an otherwise 2-column page can wash out the gutter signal,
degrading gracefully to 1 detected column rather than crashing, but not
recovering the true boundary. Also: a genuine narrow list of short items
(e.g. a numbered list of single words) could, in principle, be rejected by
the words-per-line check the same way the table fix rejects it — a
real, if less common, trade-off of the same fix that solved the table
false-positive.

## Known simplifications (the honest list)

- **Font widths are now real** (Track A Priority 1) for simple fonts with
  `/Widths` and for standard-14 fonts via the vendored fallback table. Not
  fully general: Type0/CID fonts are only handled for the common 2-byte
  Identity-H/V case (a genuine variable-width CMap would need the CMap
  itself parsed for codespace boundaries), and Type3 fonts (rare, define
  glyphs as tiny content-stream programs rather than standard outlines)
  aren't handled at all and fall back to the flat estimate.
- **No Form XObject recursion.** The `Do` operator is a no-op; text inside
  reusable form XObjects (common in some templated PDFs) won't be extracted.
  This also means vector lines drawn *inside* a Form XObject (e.g. a
  reusable ruled-table template) won't feed into ruled-grid detection either.
- **Ruled-table detection (Track A Priority 2) and the spatial-alignment
  heuristic are complements, not one replacing the other** — see the Track A
  section above for why "100% accurate" only holds for tables that actually
  have drawn borders, and for the real borderless-table-vs-columns bug this
  surfaced and how it was fixed.
- **Column detection (Track A Priority 3)** doesn't handle mixed
  full-width-banner + multi-column-body layouts on the same page, and its
  words-per-line table-vs-prose signal could, in principle, misjudge a
  genuine narrow list of short items — see the Track A section above.
- **Vector line detection approximates Bezier curves as straight lines to
  their endpoint** (curves are essentially never used for table rules, so
  this only affects shapes that wouldn't be classified as rules anyway),
  and doesn't apply PNG-predictor un-filtering (predictors are almost
  exclusively used for image/xref streams, not content streams).
- **Token counts are exact**, via a real, offline, embedded BPE tokenizer
  (see "Offline tokenizer" section below) — this was previously a
  `words × 1.3` heuristic; it no longer is.
- **LZWDecode content streams pass through undecoded.** Rare in modern PDF
  producers; ASCII85Decode + FlateDecode (what reportlab, and several other
  real-world producers, use) and plain FlateDecode are both fully supported
  via our own filter-chain decoder (`pdf_extract::decode_stream`) — this was
  necessary because lopdf 0.32's built-in decoder only understands a single
  Flate or LZW filter and silently falls back to raw encoded bytes on
  anything else, which was the root cause of zero text extraction the first
  time this was run against a real PDF.
- **Whole-file read for parsing.** PDF's xref table structure requires
  random/backward access, so the raw bytes are read into memory once before
  any processing starts — this is a format-level constraint, not a
  violation of the streaming architecture's intent, which applies to
  everything *after* that initial read (filtering, scoring, rendering,
  chunking all process one page group at a time).

## What to build next, roughly in priority order

1. Form XObject recursion for templated documents (text and vector lines
   inside reusable form objects currently aren't extracted at all).
2. CMap parsing for genuinely variable-width Type0/CID fonts, beyond the
   current 2-byte Identity-H/V assumption.
3. Mixed-layout support in column detection (full-width banner + multi-column
   body on the same page) — currently degrades to 1 column rather than
   detecting the boundary.
4. If the subprocess-per-request model becomes a throughput bottleneck under
   load, move to a persistent Rust worker pool (Unix socket or gRPC) instead
   of spawning a process per upload — the HTTP contract in `main.go` was
   kept intentionally decoupled from the Rust invocation mechanism to make
   this swap contained to `streamRustCore`.
