use serde::{Deserialize, Serialize};

/// A single positioned text run extracted directly from the PDF content
/// stream, before any markdown rendering or filtering happens.
///
/// `bbox` follows PDF user-space convention: origin bottom-left, y increases
/// upward. We keep raw PDF coordinates here and only normalize (Y_norm) at
/// the point of use, since normalization requires the page height which is
/// looked up separately per page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextElement {
    pub text: String,
    pub x_min: f64,
    pub y_min: f64,
    pub x_max: f64,
    pub y_max: f64,
    pub font_size: f64,
    pub page_num: usize,
}

impl TextElement {
    pub fn y_norm(&self, page_height: f64) -> f64 {
        if page_height <= 0.0 {
            return 0.0;
        }
        // Flip so 0.0 = top of page, 1.0 = bottom of page. This matches the
        // spec's framing ("Y_norm < 0.07 = Header", "Y_norm > 0.93 = Footer")
        // which only makes sense if 0 is the top.
        1.0 - (self.y_min / page_height)
    }
}

/// A straight line segment in device space, produced by a stroked or
/// filled-and-thin path (see pdf_extract's vector operator tracking). Used
/// by table_grid.rs to detect ruled table structure -- actual drawn
/// horizontal/vertical rules -- as a complement to (not a replacement for)
/// the spatial-alignment heuristic in fidelity.rs, since plenty of
/// real-world tables have no drawn rules at all.
#[derive(Debug, Clone, Serialize)]
pub struct LineSegment {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

/// Everything extracted from one page before filtering/rendering.
#[derive(Debug, Clone)]
pub struct PageContent {
    pub page_num: usize,
    pub page_height: f64,
    pub page_width: f64,
    pub elements: Vec<TextElement>,
    pub vector_lines: Vec<LineSegment>,
}

/// Lightweight record kept from Pass 1 (frequency profiling). We deliberately
/// do NOT keep full page content here -- only header/footer-zone candidates
/// -- to satisfy the "no global DOM in RAM" constraint even though the
/// filtering algorithm itself is inherently two-pass. See README for the
/// discussion of why a purely single-pass version of this specific algorithm
/// is not possible.
#[derive(Debug, Clone)]
pub struct HeaderFooterCandidate {
    pub page_num: usize,
    pub y_zone: String, // Y_norm rounded to 2 decimals, as per spec
    pub text: String,
}

/// A cluster of near-duplicate header/footer strings that occupy the same
/// vertical zone across the document (e.g. "Page 1 of 500", "Page 2 of 500").
#[derive(Debug, Clone, Serialize)]
pub struct RedactCluster {
    pub y_zone: String,
    pub representative_text: String,
    pub pages_seen: usize,
    pub frequency: f64,
    pub redact: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum IngestionQuality {
    #[serde(rename = "pass")]
    Pass,
    #[serde(rename = "warning_layout_anomaly")]
    Warning,
    #[serde(rename = "quarantined")]
    Quarantined,
}

#[derive(Debug, Clone, Serialize)]
pub struct PageFidelityReport {
    pub page_num: usize,
    pub c_retention: f64,
    pub t_matrix: f64,
    pub s_fidelity: f64,
    pub quality: IngestionQuality,
    pub table_detected_in_pdf: bool,
    pub table_detected_in_md: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChunkPayload {
    pub chunk_id: usize,
    pub text: String,
    pub parent_hierarchy: Vec<String>,
    pub root_document: String,
    pub token_count: usize,
    pub source_pages: Vec<usize>,
}
