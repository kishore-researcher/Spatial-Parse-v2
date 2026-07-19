mod chunker;
mod column_layout;
mod fidelity;
mod font_metrics;
mod markdown_render;
mod pdf_extract;
mod spatial_filter;
mod standard_font_metrics;
mod table_grid;
mod types;

use serde_json::json;
use std::io::{self, Read, Write};
use std::time::Instant;
use types::IngestionQuality;

/// Reads the whole PDF into memory once (lopdf requires random access to
/// parse the xref table), but everything *downstream* of that -- filtering,
/// scoring, rendering, chunking -- processes one page group at a time and
/// only ever keeps small, bounded structures (header/footer candidate
/// strings, per-page element lists) in memory simultaneously. See README
/// for the full discussion of why the raw-bytes read is an unavoidable
/// exception to "zero global allocations" for a format like PDF, and how
/// pass1/pass2 are scoped to stay lightweight regardless.
fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let document_id = args.get(2).cloned().unwrap_or_else(|| "document".to_string());

    let bytes = if let Some(path) = args.get(1) {
        std::fs::read(path)?
    } else {
        let mut buf = Vec::new();
        io::stdin().read_to_end(&mut buf)?;
        buf
    };

    let start = Instant::now();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let stderr = io::stderr();
    let mut errout = stderr.lock();

    let pages = match pdf_extract::extract_document(&bytes) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("FATAL: failed to parse PDF: {e}");
            std::process::exit(1);
        }
    };
    let total_pages = pages.len();

    // ---- Pain Point 1, Pass 1: lightweight frequency profiling ----
    let candidates = spatial_filter::pass1_profile_candidates(&pages);
    let clusters = spatial_filter::pass2_build_redact_clusters(&candidates, total_pages);

    let redact_summary: Vec<_> = clusters.iter().filter(|c| c.redact).collect();
    writeln!(
        errout,
        "[pass1/pass2] {} header/footer candidate strings profiled -> {} recurring clusters marked REDACT (>= 30% page frequency)",
        candidates.len(),
        redact_summary.len()
    )?;

    // Body font-size baseline computed once across kept (non-redacted)
    // elements, so heading detection is consistent across the whole doc.
    let mut all_kept: Vec<&types::TextElement> = Vec::new();
    let mut kept_per_page: Vec<Vec<&types::TextElement>> = Vec::new();
    for page in &pages {
        let kept: Vec<&types::TextElement> = page
            .elements
            .iter()
            .filter(|e| !spatial_filter::should_redact(e, page.page_height, &clusters))
            .collect();
        all_kept.extend(kept.iter().copied());
        kept_per_page.push(kept);
    }
    let body_font_size = markdown_render::estimate_body_font_size(&all_kept);
    let larger_sizes = markdown_render::rank_larger_sizes(&all_kept, body_font_size);

    // ---- Pain Point 2: render + score fidelity, one page group at a time ----
    let mut markdown_by_page: Vec<(usize, String)> = Vec::new();
    let mut fidelity_reports = Vec::new();
    let mut quarantined_pages = 0usize;
    let mut warning_pages = 0usize;

    for (page, kept) in pages.iter().zip(kept_per_page.iter()) {
        let md = markdown_render::render_page_markdown(kept, body_font_size, &larger_sizes);
        let report = fidelity::score_page(page.page_num, kept, &md, &page.vector_lines);

        writeln!(out, "{}", json!({ "type": "page_fidelity", "report": report }))?;

        match report.quality {
            IngestionQuality::Quarantined => {
                quarantined_pages += 1;
                writeln!(
                    errout,
                    "[ALERT] page {} quarantined (S_fidelity={:.3} < 0.70) — bypassed vectorization, needs human review",
                    page.page_num, report.s_fidelity
                )?;
            }
            IngestionQuality::Warning => {
                warning_pages += 1;
                writeln!(
                    errout,
                    "[warn] page {} flagged warning_layout_anomaly (S_fidelity={:.3})",
                    page.page_num, report.s_fidelity
                )?;
                markdown_by_page.push((page.page_num, md));
            }
            IngestionQuality::Pass => {
                markdown_by_page.push((page.page_num, md));
            }
        }
        fidelity_reports.push(report);
    }

    // ---- Pain Point 3: AST-based semantic chunking over surviving pages ----
    let chunks = chunker::chunk_document(&markdown_by_page, &document_id);
    for chunk in &chunks {
        writeln!(out, "{}", json!({ "type": "chunk", "payload": chunk }))?;
    }

    let elapsed = start.elapsed();
    let avg_fidelity = if !fidelity_reports.is_empty() {
        fidelity_reports.iter().map(|r| r.s_fidelity).sum::<f64>() / fidelity_reports.len() as f64
    } else {
        0.0
    };
    writeln!(
        out,
        "{}",
        json!({
            "type": "summary",
            "document_id": document_id,
            "total_pages": total_pages,
            "pages_passed": total_pages - quarantined_pages - warning_pages,
            "pages_warned": warning_pages,
            "pages_quarantined": quarantined_pages,
            "redact_clusters": redact_summary.len(),
            "chunks_emitted": chunks.len(),
            "avg_fidelity": avg_fidelity,
            "elapsed_ms": elapsed.as_millis(),
        })
    )?;

    Ok(())
}
