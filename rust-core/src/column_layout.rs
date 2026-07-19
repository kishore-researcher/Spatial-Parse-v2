use crate::markdown_render::group_into_lines;
use crate::types::TextElement;

/// Width of each histogram bucket used to sweep the x-axis, in points.
/// Fine enough to localize a real column gutter's edges reasonably
/// precisely without being so fine that noise dominates.
const BUCKET_WIDTH_PT: f64 = 3.0;
/// A bucket counts as part of a "gutter" (column separator) only if the
/// fraction of lines with any content covering it is at or below this
/// value. Real inter-word/inter-sentence gaps vary in *position* from line
/// to line, so they essentially never leave the same x-position empty
/// across most of a page's lines -- a genuine column gutter does, which is
/// the statistical signal this whole approach rests on. Set loosely enough
/// to tolerate a handful of full-width lines (e.g. a banner headline above
/// 2-column body text) without losing the gutter signal.
const GUTTER_MAX_LINE_COVERAGE: f64 = 0.20;
/// Minimum width (points) for a run of "gutter" buckets to count as a real
/// column separator rather than an unusually wide inter-word space. Newspaper-
/// style gutters are typically 0.2-0.4in (14-29pt); word gaps are far
/// smaller. 20pt sits comfortably between the two.
const MIN_GUTTER_WIDTH_PT: f64 = 20.0;
/// Each side of a candidate gutter must have at least this many lines with
/// real content, so a narrow marginal caption or pull-quote isn't
/// misread as a second column.
const MIN_LINES_PER_COLUMN: usize = 3;
/// Each candidate column's lines must average at least this many words per
/// line to be accepted as genuine prose. This is what distinguishes a real
/// multi-column text layout from a borderless data table: both are
/// "vertically-aligned content separated by consistent gaps" to a naive
/// X-axis density sweep, but a table's "columns" are short discrete values
/// (numbers, single words like "Q1 2026") while real column text is
/// multi-word wrapped sentences. Without this check, a 4-column financial
/// table gets misread as 4 reading columns and each column of *numbers*
/// gets rendered top-to-bottom as its own "paragraph" -- silently
/// destroying the table's row structure. This was caught by testing
/// against a real borderless table fixture, not anticipated up front.
const MIN_AVG_WORDS_PER_LINE: f64 = 3.0;

/// Detects column boundaries via 1D x-axis density analysis (a sweep over
/// fixed-width buckets counting, per bucket, what fraction of the page's
/// lines have content there) and partitions elements into left-to-right
/// column groups. Falls back to a single column (the original, unsplit
/// element order) whenever the signal isn't clear -- this function is
/// deliberately conservative because a false-positive column split would
/// scramble an ordinary single-column document's reading order, which is a
/// worse failure mode than occasionally missing a real multi-column layout.
///
/// Known limitation: this assumes the *whole page* follows one consistent
/// column structure. A page mixing a full-width banner/headline with a
/// 2-column body below it is not specially handled -- enough full-width
/// lines mixed into an otherwise 2-column page can wash out the gutter
/// signal (each such line "covers" the gutter bucket), which degrades
/// gracefully to detecting 1 column rather than crashing or scrambling
/// output, but doesn't recover the true 2-column boundary in that case.
pub fn partition_into_columns<'a>(elements: &[&'a TextElement]) -> Vec<Vec<&'a TextElement>> {
    if elements.is_empty() {
        return vec![];
    }

    let lines = group_into_lines(elements);
    if lines.len() < MIN_LINES_PER_COLUMN * 2 {
        return vec![elements.to_vec()]; // not enough lines to reliably detect 2 columns
    }

    let min_x = elements.iter().map(|e| e.x_min).fold(f64::INFINITY, f64::min);
    let max_x = elements.iter().map(|e| e.x_max).fold(f64::NEG_INFINITY, f64::max);
    let span = max_x - min_x;
    if span <= 0.0 {
        return vec![elements.to_vec()];
    }

    let bucket_count = ((span / BUCKET_WIDTH_PT).ceil() as usize).max(1);
    let mut coverage = vec![0usize; bucket_count];
    for line in &lines {
        // Mark every bucket touched by any element in this line as
        // "covered" for this line (count each line at most once per bucket
        // regardless of how many elements in it touch that bucket).
        let mut touched = vec![false; bucket_count];
        for el in &line.elements {
            let start = (((el.x_min - min_x) / BUCKET_WIDTH_PT).floor() as isize).max(0) as usize;
            let end = (((el.x_max - min_x) / BUCKET_WIDTH_PT).ceil() as isize).max(0) as usize;
            for b in start..end.min(bucket_count) {
                touched[b] = true;
            }
        }
        for (b, was_touched) in touched.into_iter().enumerate() {
            if was_touched {
                coverage[b] += 1;
            }
        }
    }

    let total_lines = lines.len() as f64;
    let is_gutter: Vec<bool> = coverage.iter().map(|&c| (c as f64 / total_lines) <= GUTTER_MAX_LINE_COVERAGE).collect();

    // Find maximal runs of gutter buckets wide enough to count as a real
    // column separator, ignoring any run touching the very first or last
    // bucket (that would be a page margin, not an internal gutter).
    let mut cuts: Vec<(f64, f64)> = Vec::new(); // (x_start, x_end) of each gutter run
    let mut run_start: Option<usize> = None;
    for (i, &g) in is_gutter.iter().enumerate() {
        if g {
            if run_start.is_none() {
                run_start = Some(i);
            }
        } else if let Some(start) = run_start.take() {
            record_cut_if_wide_enough(start, i, min_x, &mut cuts);
        }
    }
    if let Some(start) = run_start {
        record_cut_if_wide_enough(start, bucket_count, min_x, &mut cuts);
    }
    // Drop cuts touching the outer edges -- those are margins, not gutters.
    cuts.retain(|&(x_start, x_end)| x_start > min_x + 1.0 && x_end < max_x - 1.0);

    if cuts.is_empty() {
        return vec![elements.to_vec()];
    }

    // Partition elements by which side of each cut their x-center falls on.
    let boundaries: Vec<f64> = cuts.iter().map(|&(s, e)| (s + e) / 2.0).collect();
    let mut columns: Vec<Vec<&TextElement>> = vec![Vec::new(); boundaries.len() + 1];
    for &el in elements {
        let center = (el.x_min + el.x_max) / 2.0;
        let col_idx = boundaries.iter().filter(|&&b| center > b).count();
        columns[col_idx].push(el);
    }

    // Validate: each column must have a reasonable minimum of distinct
    // lines, or this was a spurious split (e.g. a stray footnote off to one
    // side) -- fall back to single-column rather than emit a near-empty
    // "column" that would badly fragment reading order.
    for col in &columns {
        let col_lines = group_into_lines(col);
        if col_lines.len() < MIN_LINES_PER_COLUMN {
            return vec![elements.to_vec()];
        }
        // Reject table-like splits: see MIN_AVG_WORDS_PER_LINE doc comment.
        let total_words: usize = col_lines
            .iter()
            .map(|line| line.elements.iter().map(|e| e.text.split_whitespace().count()).sum::<usize>())
            .sum();
        let avg_words_per_line = total_words as f64 / col_lines.len() as f64;
        if avg_words_per_line < MIN_AVG_WORDS_PER_LINE {
            return vec![elements.to_vec()];
        }
    }

    columns
}

fn record_cut_if_wide_enough(bucket_start: usize, bucket_end: usize, min_x: f64, cuts: &mut Vec<(f64, f64)>) {
    let x_start = min_x + bucket_start as f64 * BUCKET_WIDTH_PT;
    let x_end = min_x + bucket_end as f64 * BUCKET_WIDTH_PT;
    if x_end - x_start >= MIN_GUTTER_WIDTH_PT {
        cuts.push((x_start, x_end));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(text: &str, x_min: f64, y_min: f64, width: f64) -> TextElement {
        TextElement { text: text.to_string(), x_min, y_min, x_max: x_min + width, y_max: y_min + 8.0, font_size: 10.0, page_num: 1 }
    }

    /// Builds a synthetic single-column page: many lines, each spanning
    /// roughly the same wide x-range with normal small word gaps (never
    /// leaving the same x position empty across many lines).
    fn single_column_page(n_lines: usize) -> Vec<TextElement> {
        let mut out = Vec::new();
        for i in 0..n_lines {
            let y = 700.0 - (i as f64) * 12.0;
            // Vary the "word gap" position per line so no single x bucket
            // is empty across most lines -- exactly what real prose does.
            let gap_x = 50.0 + ((i * 37) % 400) as f64;
            out.push(el("word", 50.0, y, gap_x - 50.0));
            out.push(el("word", gap_x + 4.0, y, 450.0 - (gap_x - 50.0)));
        }
        out
    }

    /// Builds a synthetic clean 2-column page: left column text in
    /// [50,290], a genuine ~40pt gutter [290,330], right column in [330,570].
    /// Uses realistic multi-word prose per line (not single-token
    /// placeholders) since that's now part of what distinguishes a real
    /// text column from a borderless table's columns -- see
    /// MIN_AVG_WORDS_PER_LINE.
    fn two_column_page(n_lines_each: usize) -> Vec<TextElement> {
        let mut out = Vec::new();
        for i in 0..n_lines_each {
            let y = 700.0 - (i as f64) * 12.0;
            out.push(el("this is left column prose text", 50.0, y, 240.0));
            out.push(el("this is right column prose text", 330.0, y, 240.0));
        }
        out
    }

    #[test]
    fn single_column_page_is_not_split() {
        let elements = single_column_page(20);
        let refs: Vec<&TextElement> = elements.iter().collect();
        let columns = partition_into_columns(&refs);
        assert_eq!(columns.len(), 1, "a genuine single-column page must not be split");
    }

    #[test]
    fn two_column_page_is_correctly_split() {
        let elements = two_column_page(10);
        let refs: Vec<&TextElement> = elements.iter().collect();
        let columns = partition_into_columns(&refs);
        assert_eq!(columns.len(), 2, "a genuine 2-column page should be detected as such");
        assert!(columns[0].iter().all(|e| e.text.contains("left")));
        assert!(columns[1].iter().all(|e| e.text.contains("right")));
    }

    #[test]
    fn narrow_stray_caption_does_not_trigger_false_split() {
        // A single-column page plus one lone narrow element off to the
        // right (e.g. a page-number-like stray mark) must not be read as a
        // second "column" of only 1 line.
        let mut elements = single_column_page(20);
        elements.push(el("(fig 1)", 560.0, 400.0, 30.0));
        let refs: Vec<&TextElement> = elements.iter().collect();
        let columns = partition_into_columns(&refs);
        assert_eq!(columns.len(), 1, "a single stray element must not be misread as a second column");
    }

    #[test]
    fn borderless_table_is_not_misread_as_reading_columns() {
        // A 4-column financial table with no ruled borders -- structurally
        // it looks exactly like "vertically-aligned content separated by
        // consistent gaps" to a naive X-axis sweep, same as a real
        // multi-column layout. The distinguishing signal is content: table
        // cells are short discrete values, not multi-word prose. This
        // reproduces a real bug found via full end-to-end testing, where
        // such a table got split into 4 "reading columns" and each
        // column's *numbers* got rendered top-to-bottom as if they were a
        // wrapped paragraph, destroying the table's row structure.
        let mut elements = Vec::new();
        let headers = ["Quarter", "Product X", "Product Y", "Total"];
        let col_x = [54.0, 162.0, 270.0, 378.0];
        let rows = [
            ["Quarter", "Product X", "Product Y", "Total"],
            ["Q1 2026", "12.4", "3.1", "15.5"],
            ["Q2 2026", "14.0", "3.4", "17.4"],
            ["Q3 2026", "15.2", "3.9", "19.1"],
            ["Q4 2026", "16.8", "4.2", "21.0"],
        ];
        let _ = headers;
        for (r, row) in rows.iter().enumerate() {
            let y = 700.0 - (r as f64) * 16.0;
            for (cell, &x) in row.iter().zip(col_x.iter()) {
                elements.push(el(cell, x, y, 40.0));
            }
        }
        let refs: Vec<&TextElement> = elements.iter().collect();
        let columns = partition_into_columns(&refs);
        assert_eq!(columns.len(), 1, "a borderless data table must never be split into per-column reading order");
    }

    #[test]
    fn too_few_lines_skips_detection_entirely() {
        let elements = two_column_page(1); // only 2 lines total, below MIN_LINES_PER_COLUMN*2
        let refs: Vec<&TextElement> = elements.iter().collect();
        let columns = partition_into_columns(&refs);
        assert_eq!(columns.len(), 1, "too few lines to reliably detect columns should fall back to 1");
    }

    #[test]
    fn reproduces_real_world_page27_coordinates() {
        // Exact coordinates captured from the actual PDF extraction for the
        // two-column test fixture page, used to debug why detection failed
        // in the full pipeline despite a healthy ~57pt real gutter.
        let raw: Vec<(f64, f64, f64, &str)> = vec![
            (54.0, 193.5, 709.5, "Risk Factors Overview"),
            (54.0, 252.0, 688.8, "Market risk arises from curren"),
            (54.0, 267.8, 675.8, "fluctuations across the region"),
            (54.0, 257.8, 662.8, "operates its manufacturing and"),
            (54.0, 277.3, 649.8, "Management monitors exposure u"),
            (54.0, 247.8, 636.8, "placed on a quarterly basis to"),
            (54.0, 219.8, 623.8, "earnings volatility from perio"),
            (54.0, 263.1, 610.8, "These hedges are reviewed by t"),
            (334.8, 539.1, 688.8, "Operational risk includes disr"),
            (334.8, 557.6, 675.8, "chain from single-source compo"),
            (334.8, 541.3, 662.8, "in regions subject to periodic"),
            (334.8, 561.8, 649.8, "The company maintains dual-sou"),
            (334.8, 545.5, 636.8, "its highest-volume components "),
            (334.8, 534.9, 623.8, "Contingency logistics plans ar"),
            (334.8, 526.5, 610.8, "year with each regional operat"),
        ];
        let elements: Vec<TextElement> = raw
            .iter()
            .map(|&(x_min, x_max, y_min, text)| TextElement {
                text: text.to_string(),
                x_min,
                y_min,
                x_max,
                y_max: y_min + 9.0,
                font_size: 9.5,
                page_num: 27,
            })
            .collect();
        let refs: Vec<&TextElement> = elements.iter().collect();
        let columns = partition_into_columns(&refs);
        eprintln!("DEBUG: detected {} columns", columns.len());
        for (i, col) in columns.iter().enumerate() {
            eprintln!("  column {i}: {} elements", col.len());
        }
        assert_eq!(columns.len(), 2, "real-world page-27 coordinates with a ~57pt gutter must be split into 2 columns");
    }
}
