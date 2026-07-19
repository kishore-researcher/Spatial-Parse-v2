use crate::column_layout::partition_into_columns;
use crate::types::TextElement;
use std::collections::HashMap;

const ROW_TOLERANCE: f64 = 3.0;
const HEADING_SIZE_RATIO: f64 = 1.15;
const PARAGRAPH_GAP_MULTIPLIER: f64 = 1.6;

pub(crate) struct Line<'a> {
    pub(crate) elements: Vec<&'a TextElement>,
    pub(crate) y: f64,
    pub(crate) avg_font_size: f64,
}

pub(crate) fn group_into_lines<'a>(elements: &[&'a TextElement]) -> Vec<Line<'a>> {
    let mut sorted: Vec<&&TextElement> = elements.iter().collect();
    sorted.sort_by(|a, b| b.y_min.partial_cmp(&a.y_min).unwrap());

    let mut lines: Vec<Line> = Vec::new();
    for el in sorted {
        if let Some(last) = lines.last_mut() {
            if (last.y - el.y_min).abs() <= ROW_TOLERANCE {
                last.elements.push(*el);
                continue;
            }
        }
        lines.push(Line { elements: vec![*el], y: el.y_min, avg_font_size: el.font_size });
    }
    for line in lines.iter_mut() {
        line.elements.sort_by(|a, b| a.x_min.partial_cmp(&b.x_min).unwrap());
        let sum: f64 = line.elements.iter().map(|e| e.font_size).sum();
        line.avg_font_size = sum / line.elements.len() as f64;
    }
    lines
}

/// Finds the most common font size across the document, used as the "body
/// text" baseline that heading sizes are judged relative to.
pub fn estimate_body_font_size(all_elements: &[&TextElement]) -> f64 {
    let mut counts: HashMap<i64, usize> = HashMap::new();
    for e in all_elements {
        let bucket = (e.font_size * 2.0).round() as i64; // 0.5pt buckets
        *counts.entry(bucket).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .map(|(bucket, _)| bucket as f64 / 2.0)
        .unwrap_or(11.0)
}

fn heading_depth(font_size: f64, body_size: f64, distinct_larger_sizes: &[f64]) -> Option<usize> {
    if font_size <= body_size * HEADING_SIZE_RATIO {
        return None;
    }
    // distinct_larger_sizes is sorted descending; rank 0 = largest => H1.
    let rank = distinct_larger_sizes
        .iter()
        .position(|s| (*s - font_size).abs() < 0.6)
        .unwrap_or(distinct_larger_sizes.len().saturating_sub(1));
    Some((rank + 1).min(4))
}

/// Renders one page's kept (post-redaction) elements into a Markdown
/// string. Detects multi-column layouts first (Priority 3: X-axis spatial
/// clustering) and renders each detected column fully, left to right,
/// before moving to the next -- this is what prevents "word salad" from
/// blending unrelated lines across a column gutter, which naive
/// top-to-bottom-across-the-whole-page ordering would produce on any
/// genuinely multi-column page.
pub fn render_page_markdown(elements: &[&TextElement], body_font_size: f64, larger_sizes: &[f64]) -> String {
    let columns = partition_into_columns(elements);
    columns
        .iter()
        .map(|col| render_column_markdown(col, body_font_size, larger_sizes))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Renders a single column's (or, for single-column pages, the whole
/// page's) elements into Markdown -- the heading/table/paragraph detection
/// logic that existed before column-awareness was added, now scoped to
/// operate within one column's elements rather than the whole page.
fn render_column_markdown(elements: &[&TextElement], body_font_size: f64, larger_sizes: &[f64]) -> String {
    let lines = group_into_lines(elements);
    let mut out = String::new();
    let mut i = 0;
    let mut prev_y: Option<f64> = None;

    while i < lines.len() {
        let line = &lines[i];

        // Try to detect a table block starting here: consecutive lines with
        // >= 2 cells and a matching column count.
        if line.elements.len() >= 2 {
            let col_count = line.elements.len();
            let mut block_end = i;
            while block_end < lines.len()
                && lines[block_end].elements.len() == col_count
            {
                block_end += 1;
            }
            let row_count = block_end - i;
            if row_count >= 2 {
                out.push('\n');
                for (r, row_idx) in (i..block_end).enumerate() {
                    let cells: Vec<String> =
                        lines[row_idx].elements.iter().map(|e| e.text.trim().to_string()).collect();
                    out.push_str("| ");
                    out.push_str(&cells.join(" | "));
                    out.push_str(" |\n");
                    if r == 0 {
                        out.push_str("|");
                        out.push_str(&" --- |".repeat(col_count));
                        out.push('\n');
                    }
                }
                out.push('\n');
                i = block_end;
                prev_y = Some(lines[block_end.min(lines.len()) - 1].y);
                continue;
            }
        }

        let text: String = line.elements.iter().map(|e| e.text.trim()).collect::<Vec<_>>().join(" ");
        let text = text.trim();
        if !text.is_empty() {
            if let Some(depth) = heading_depth(line.avg_font_size, body_font_size, larger_sizes) {
                out.push('\n');
                out.push_str(&"#".repeat(depth));
                out.push(' ');
                out.push_str(text);
                out.push('\n');
            } else {
                // New paragraph if there's a large vertical jump since the
                // previous line (heuristic for blank-line separation).
                if let Some(py) = prev_y {
                    let gap = py - line.y;
                    if gap > body_font_size * PARAGRAPH_GAP_MULTIPLIER {
                        out.push('\n');
                    } else if !out.ends_with('\n') && !out.is_empty() {
                        out.push(' ');
                    }
                }
                out.push_str(text);
            }
        }
        prev_y = Some(line.y);
        i += 1;
    }
    out.trim().to_string()
}

/// Ranks font sizes strictly larger than the body baseline, descending,
/// so callers can map a heading's size to a stable H1/H2/H3 depth.
pub fn rank_larger_sizes(all_elements: &[&TextElement], body_size: f64) -> Vec<f64> {
    let mut sizes: Vec<f64> = all_elements
        .iter()
        .map(|e| e.font_size)
        .filter(|s| *s > body_size * HEADING_SIZE_RATIO)
        .collect();
    sizes.sort_by(|a, b| b.partial_cmp(a).unwrap());
    sizes.dedup_by(|a, b| (*a - *b).abs() < 0.6);
    sizes
}
