use crate::spatial_filter::build_page_rtree;
use crate::table_grid;
use crate::types::{IngestionQuality, LineSegment, PageFidelityReport, TextElement};
use rstar::AABB;
use std::collections::HashSet;

pub const W1_CHAR_RETENTION: f64 = 0.40;
pub const W2_TABLE_MATRIX: f64 = 0.60;
pub const PASS_THRESHOLD: f64 = 0.90;
pub const WARNING_THRESHOLD: f64 = 0.70;

fn alnum_count(s: &str) -> usize {
    s.chars().filter(|c| c.is_alphanumeric()).count()
}

/// IMPORTANT deviation from the spec's literal formula, called out
/// explicitly (see README "Design corrections"):
///
/// The spec defines C_raw as "alphanumeric characters from native PDF binary
/// reading" -- i.e. everything on the page, including header/footer noise.
/// If we compute retention against *that* baseline, every page will show
/// "content loss" purely because Pain Point 1 deliberately stripped repeated
/// headers/footers, which would make S_fidelity flag good pages as broken.
///
/// So here, C_raw is computed only from elements NOT flagged for redaction
/// (i.e. the legitimate body content baseline), and C_md is the character
/// count of the actual rendered markdown body. That way the score measures
/// genuine extraction/rendering loss (garbled decoding, dropped runs,
/// broken tables), not intentional, correct redaction.
pub fn char_retention(kept_elements: &[&TextElement], markdown_text: &str) -> f64 {
    let c_raw: usize = kept_elements.iter().map(|e| alnum_count(&e.text)).sum();
    let c_md = alnum_count(markdown_text);
    if c_raw == 0 {
        return 1.0; // nothing expected, nothing lost
    }
    let diff = (c_raw as i64 - c_md as i64).unsigned_abs() as f64;
    (1.0 - diff / c_raw as f64).clamp(0.0, 1.0)
}

/// Simple grid: (rows, cols) detected via heuristic alignment clustering.
pub type Grid = (usize, usize);

/// Heuristic table-grid detector over the page's spatial layout: the
/// fallback for the (very common) case of a *borderless* table, which has
/// no drawn rules for `detect_ruled_grid` to find at all -- alignment is
/// the only signal available. This is the "Spatial Memory Isolation" piece
/// of the architecture in practice: rather than clustering rows by sorting
/// a flat string list, we build an R-tree over the page's element bounding
/// boxes and answer "what else shares this row band?" as a genuine 2D
/// range query (an envelope intersection against a thin horizontal strip
/// spanning the page). This scales better than an all-pairs comparison on
/// dense pages and mirrors how a real spatial-index-backed layout engine
/// would do row/column clustering. Deliberately conservative (requires >= 2
/// rows and >= 2 columns) since false positives here would wrongly
/// penalize ordinary paragraphs.
pub fn detect_spatial_grid(kept_elements: &[&TextElement]) -> Option<Grid> {
    if kept_elements.len() < 4 {
        return None;
    }

    let tree = build_page_rtree(kept_elements);
    let row_tolerance = 3.0;
    let min_x = kept_elements.iter().map(|e| e.x_min).fold(f64::INFINITY, f64::min) - 1.0;
    let max_x = kept_elements.iter().map(|e| e.x_max).fold(f64::NEG_INFINITY, f64::max) + 1.0;

    let mut visited: HashSet<usize> = HashSet::new();
    let mut rows: Vec<usize> = Vec::new(); // cell counts per detected row
    let mut min_cols = usize::MAX;

    for (i, el) in kept_elements.iter().enumerate() {
        if visited.contains(&i) {
            continue;
        }
        let band = AABB::from_corners(
            [min_x, el.y_min - row_tolerance],
            [max_x, el.y_min + row_tolerance],
        );
        let row_members: Vec<usize> = tree
            .locate_in_envelope_intersecting(&band)
            .map(|se| se.index)
            .collect();
        for idx in &row_members {
            visited.insert(*idx);
        }
        if row_members.len() >= 2 {
            rows.push(row_members.len());
            min_cols = min_cols.min(row_members.len());
        }
    }

    if rows.len() < 2 || min_cols < 2 || min_cols == usize::MAX {
        return None;
    }
    Some((rows.len(), min_cols))
}

/// Combined PDF-side table detector: tries actual drawn rule lines first
/// (high-confidence, exact grid dimensions when a table really has ruled
/// borders), and only falls back to the spatial-alignment heuristic when no
/// ruled grid is found -- which is the common case for borderless tables
/// that rely purely on whitespace/column alignment. Neither detector
/// supersedes the other; they cover different, non-overlapping cases. A
/// claim of "100% accurate for all tables" would only be true for the
/// ruled-line-only subset -- see README.
///
/// The spatial-heuristic fallback is run *per detected column* (reusing
/// Priority 3's column partitioning) rather than on the flat page element
/// list: on a genuine multi-column page, lines from two different columns
/// can align at the same y position (this is the normal case for justified
/// multi-column body text with uniform line height), which would otherwise
/// look exactly like a 2-column table row to the alignment heuristic. Column
/// partitioning is exactly the information needed to tell those apart.
pub fn detect_pdf_grid(kept_elements: &[&TextElement], vector_lines: &[LineSegment]) -> Option<Grid> {
    if let Some(grid) = table_grid::detect_ruled_grid(vector_lines) {
        return Some(grid);
    }
    let columns = crate::column_layout::partition_into_columns(kept_elements);
    columns.iter().find_map(|col| detect_spatial_grid(col))
}

/// Counts rows/cols in a rendered markdown pipe-table, if any is present.
pub fn detect_md_grid(markdown_text: &str) -> Option<Grid> {
    let table_lines: Vec<&str> = markdown_text
        .lines()
        .filter(|l| l.trim_start().starts_with('|'))
        .filter(|l| !l.chars().all(|c| c == '|' || c == '-' || c == ':' || c.is_whitespace()))
        .collect();
    if table_lines.len() < 2 {
        return None;
    }
    let cols = table_lines[0].split('|').filter(|s| !s.trim().is_empty()).count();
    if cols < 2 {
        return None;
    }
    Some((table_lines.len(), cols))
}

pub fn table_matrix_score(pdf_grid: Option<Grid>, md_grid: Option<Grid>) -> (f64, bool, bool) {
    match (pdf_grid, md_grid) {
        (None, _) => (1.0, false, md_grid.is_some()), // nothing structural to preserve
        (Some(p), Some(m)) => (if p == m { 1.0 } else { 0.0 }, true, true),
        (Some(_), None) => (0.0, true, false), // detected a grid but lost it in md
    }
}

pub fn score_page(
    page_num: usize,
    kept_elements: &[&TextElement],
    markdown_text: &str,
    vector_lines: &[LineSegment],
) -> PageFidelityReport {
    let c_retention = char_retention(kept_elements, markdown_text);
    let pdf_grid = detect_pdf_grid(kept_elements, vector_lines);
    let md_grid = detect_md_grid(markdown_text);
    let (t_matrix, table_in_pdf, table_in_md) = table_matrix_score(pdf_grid, md_grid);

    let s_fidelity = W1_CHAR_RETENTION * c_retention + W2_TABLE_MATRIX * t_matrix;

    let quality = if s_fidelity >= PASS_THRESHOLD {
        IngestionQuality::Pass
    } else if s_fidelity >= WARNING_THRESHOLD {
        IngestionQuality::Warning
    } else {
        IngestionQuality::Quarantined
    };

    PageFidelityReport {
        page_num,
        c_retention,
        t_matrix,
        s_fidelity,
        quality,
        table_detected_in_pdf: table_in_pdf,
        table_detected_in_md: table_in_md,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(text: &str, x_min: f64, y_min: f64) -> TextElement {
        TextElement { text: text.to_string(), x_min, y_min, x_max: x_min + 20.0, y_max: y_min + 8.0, font_size: 10.0, page_num: 1 }
    }

    #[test]
    fn full_retention_scores_perfectly() {
        let elements = vec![el("Hello world", 0.0, 100.0)];
        let refs: Vec<&TextElement> = elements.iter().collect();
        let score = char_retention(&refs, "Hello world");
        assert_eq!(score, 1.0);
    }

    #[test]
    fn severe_character_loss_is_penalized() {
        let elements = vec![el("A fairly long sentence with real content here", 0.0, 100.0)];
        let refs: Vec<&TextElement> = elements.iter().collect();
        // Markdown lost almost everything -- simulates a badly garbled page.
        let score = char_retention(&refs, "A");
        assert!(score < 0.3, "expected heavy penalty for major content loss, got {score}");
    }

    #[test]
    fn clean_table_grid_matches_and_passes_gate() {
        // 3 rows x 3 cols, cleanly aligned -- both the PDF spatial pass and
        // the rendered markdown should agree, giving a perfect T_matrix.
        let elements = vec![
            el("Quarter", 0.0, 100.0), el("Revenue", 30.0, 100.0), el("Cost", 60.0, 100.0),
            el("Q1", 0.0, 88.0), el("10", 30.0, 88.0), el("5", 60.0, 88.0),
            el("Q2", 0.0, 76.0), el("12", 30.0, 76.0), el("6", 60.0, 76.0),
        ];
        let refs: Vec<&TextElement> = elements.iter().collect();
        let md = "| Quarter | Revenue | Cost |\n| --- | --- | --- |\n| Q1 | 10 | 5 |\n| Q2 | 12 | 6 |\n";
        let report = score_page(1, &refs, md, &[]);
        assert!(report.table_detected_in_pdf);
        assert!(report.table_detected_in_md);
        assert_eq!(report.t_matrix, 1.0);
        assert_eq!(report.quality, IngestionQuality::Pass);
    }

    #[test]
    fn detected_table_lost_in_markdown_triggers_quarantine_range() {
        let elements = vec![
            el("Quarter", 0.0, 100.0), el("Revenue", 30.0, 100.0), el("Cost", 60.0, 100.0),
            el("Q1", 0.0, 88.0), el("10", 30.0, 88.0), el("5", 60.0, 88.0),
        ];
        let refs: Vec<&TextElement> = elements.iter().collect();
        // Markdown rendering dropped the table structure entirely.
        let md = "Quarter Revenue Cost Q1 10 5";
        let report = score_page(1, &refs, md, &[]);
        assert!(report.table_detected_in_pdf);
        assert!(!report.table_detected_in_md);
        assert_eq!(report.t_matrix, 0.0);
        // 0.4 * 1.0 (full char retention) + 0.6 * 0.0 = 0.40 -> quarantined.
        assert_eq!(report.quality, IngestionQuality::Quarantined);
    }

    #[test]
    fn plain_paragraph_never_falsely_flagged_as_table() {
        let elements = vec![el("This is a single line of ordinary prose.", 0.0, 100.0)];
        let refs: Vec<&TextElement> = elements.iter().collect();
        assert!(detect_pdf_grid(&refs, &[]).is_none());
    }

    #[test]
    fn ruled_grid_takes_precedence_over_spatial_heuristic_and_is_exact() {
        // A page with actual drawn rule lines for a 2x2 table, plus text
        // elements that (deliberately) would NOT trigger the spatial
        // heuristic on their own (only 2 elements, below its threshold of 4).
        let elements = vec![el("A", 5.0, 55.0), el("B", 55.0, 55.0)];
        let refs: Vec<&TextElement> = elements.iter().collect();
        let lines = vec![
            LineSegment { x0: 0.0, y0: 100.0, x1: 100.0, y1: 100.0 },
            LineSegment { x0: 0.0, y0: 50.0, x1: 100.0, y1: 50.0 },
            LineSegment { x0: 0.0, y0: 0.0, x1: 0.0, y1: 100.0 },
            LineSegment { x0: 100.0, y0: 0.0, x1: 100.0, y1: 100.0 },
        ];
        let grid = detect_pdf_grid(&refs, &lines);
        assert_eq!(grid, Some((1, 1)), "ruled-line detector should fire even though the spatial heuristic alone would not");
    }
}
