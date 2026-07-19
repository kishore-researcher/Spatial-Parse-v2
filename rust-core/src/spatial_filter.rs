use crate::types::{HeaderFooterCandidate, PageContent, RedactCluster, TextElement};
use rstar::{RTree, RTreeObject, AABB};
use std::collections::HashMap;

pub const HEADER_ZONE_MAX: f64 = 0.07;
pub const FOOTER_ZONE_MIN: f64 = 0.93;
pub const FREQ_REDACT_THRESHOLD: f64 = 0.30;
/// Two strings in the same Y-zone are treated as the "same" recurring
/// template (e.g. "Page 1 of 500" vs "Page 2 of 500") if their Normalized
/// Levenshtein Distance is at or above this similarity threshold.
pub const NLD_SIMILARITY_THRESHOLD: f64 = 0.65;

/// Wraps a TextElement's bounding box so it can be indexed in an R-tree.
/// We build one R-tree per page: this is the "Spatial Memory Isolation"
/// piece of the spec -- page layout is modeled geometrically rather than as
/// a flat string blob, which is what lets Pain Point 2's table-grid
/// detection do proximity/alignment queries instead of regex guessing.
pub struct SpatialElement {
    pub index: usize,
    pub bbox: [f64; 4],
}

impl RTreeObject for SpatialElement {
    type Envelope = AABB<[f64; 2]>;
    fn envelope(&self) -> Self::Envelope {
        AABB::from_corners([self.bbox[0], self.bbox[1]], [self.bbox[2], self.bbox[3]])
    }
}

pub fn build_page_rtree(elements: &[&TextElement]) -> RTree<SpatialElement> {
    let items: Vec<SpatialElement> = elements
        .iter()
        .enumerate()
        .map(|(i, e)| SpatialElement { index: i, bbox: [e.x_min, e.y_min, e.x_max, e.y_max] })
        .collect();
    RTree::bulk_load(items)
}

fn normalized_levenshtein(a: &str, b: &str) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let max_len = a.chars().count().max(b.chars().count()) as f64;
    if max_len == 0.0 {
        return 1.0;
    }
    let dist = strsim::levenshtein(a, b) as f64;
    1.0 - (dist / max_len)
}

/// PASS 1: Frequency Profiling Iterator.
///
/// Deliberately lightweight: we only pull out elements sitting in the
/// header/footer zones (Y_norm < 0.07 or > 0.93), not full page bodies. For
/// a 500-page document this is typically a few hundred short strings, not
/// the whole document -- keeping this "global" structure well within the
/// spirit of the Zero-Global-Allocations constraint even though the overall
/// algorithm needs two passes over the stream (see README for why a
/// single-pass version of *this specific* algorithm is not mathematically
/// possible: you cannot know a global frequency ratio before you've seen
/// every page).
pub fn pass1_profile_candidates(pages: &[PageContent]) -> Vec<HeaderFooterCandidate> {
    let mut candidates = Vec::new();
    for page in pages {
        for el in &page.elements {
            let y_norm = el.y_norm(page.page_height);
            if y_norm < HEADER_ZONE_MAX || y_norm > FOOTER_ZONE_MIN {
                candidates.push(HeaderFooterCandidate {
                    page_num: page.page_num,
                    y_zone: format!("{:.2}", y_norm),
                    text: el.text.trim().to_string(),
                });
            }
        }
    }
    candidates
}

/// PASS 2: Programmatic Redaction (cluster building + frequency scoring).
///
/// Within each Y-zone key, near-duplicate strings (by NLD) are merged into
/// one cluster so that dynamic strings like page numbers are recognized as
/// a single recurring template rather than N unique one-off strings. A
/// cluster is marked REDACT if it appears on >= 30% of all pages.
pub fn pass2_build_redact_clusters(
    candidates: &[HeaderFooterCandidate],
    total_pages: usize,
) -> Vec<RedactCluster> {
    // Group by Y-zone key first (spec: "Key: Cluster Zone (Y_norm rounded to
    // 2 decimals)").
    let mut zones: HashMap<String, Vec<&HeaderFooterCandidate>> = HashMap::new();
    for c in candidates {
        zones.entry(c.y_zone.clone()).or_default().push(c);
    }

    let mut clusters = Vec::new();
    for (zone, items) in zones {
        // Greedy NLD clustering within the zone: each cluster tracks a
        // representative string and the set of distinct pages it occurred on.
        struct Sub {
            representative: String,
            pages: std::collections::HashSet<usize>,
        }
        let mut subs: Vec<Sub> = Vec::new();

        for item in items {
            let mut matched = false;
            for sub in subs.iter_mut() {
                if normalized_levenshtein(&sub.representative, &item.text) >= NLD_SIMILARITY_THRESHOLD {
                    sub.pages.insert(item.page_num);
                    matched = true;
                    break;
                }
            }
            if !matched {
                let mut pages = std::collections::HashSet::new();
                pages.insert(item.page_num);
                subs.push(Sub { representative: item.text.clone(), pages });
            }
        }

        for sub in subs {
            let freq = if total_pages > 0 { sub.pages.len() as f64 / total_pages as f64 } else { 0.0 };
            clusters.push(RedactCluster {
                y_zone: zone.clone(),
                representative_text: sub.representative,
                pages_seen: sub.pages.len(),
                frequency: freq,
                redact: freq >= FREQ_REDACT_THRESHOLD,
            });
        }
    }
    clusters
}

/// Final Stream Output stage: given the REDACT cluster set, decide whether a
/// specific element (from any page) should be dropped before markdown
/// rendering. Uses the same Y-zone + NLD matching rule as clustering so an
/// element doesn't need to match verbatim (catches "Page 7 of 500" even
/// though the representative might be "Page 1 of 500").
pub fn should_redact(el: &TextElement, page_height: f64, clusters: &[RedactCluster]) -> bool {
    let y_norm = el.y_norm(page_height);
    if y_norm >= HEADER_ZONE_MAX && y_norm <= FOOTER_ZONE_MIN {
        return false; // not even in header/footer territory
    }
    let zone_key = format!("{:.2}", y_norm);
    let text = el.text.trim();
    clusters.iter().any(|c| {
        c.redact
            && c.y_zone == zone_key
            && normalized_levenshtein(&c.representative_text, text) >= NLD_SIMILARITY_THRESHOLD
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(text: &str, y_min: f64, page_num: usize) -> TextElement {
        TextElement { text: text.to_string(), x_min: 0.0, y_min, x_max: 10.0, y_max: y_min + 8.0, font_size: 10.0, page_num }
    }

    #[test]
    fn nld_catches_dynamic_page_numbers() {
        // "Page 1 of 500" vs "Page 2 of 500" should be recognized as the
        // same recurring template, per the spec's worked example.
        let sim = normalized_levenshtein("Page 1 of 500", "Page 2 of 500");
        assert!(sim >= NLD_SIMILARITY_THRESHOLD, "expected high similarity, got {sim}");
    }

    #[test]
    fn unrelated_footer_strings_are_not_merged() {
        let sim = normalized_levenshtein("Page 1 of 500", "Confidential Draft");
        assert!(sim < NLD_SIMILARITY_THRESHOLD, "expected low similarity, got {sim}");
    }

    #[test]
    fn recurring_footer_crosses_redact_threshold() {
        // Page height 800 (PDF points), y_min=770 => y_norm = 1 - 770/800 = 0.0375 (header zone).
        let page_height = 800.0;
        let total_pages = 10;
        let mut pages = Vec::new();
        for p in 1..=total_pages {
            pages.push(PageContent {
                page_num: p,
                page_height,
                page_width: 612.0,
                elements: vec![el(&format!("Page {p} of {total_pages}"), 770.0, p)],
                vector_lines: vec![],
            });
        }
        let candidates = pass1_profile_candidates(&pages);
        assert_eq!(candidates.len(), total_pages);
        let clusters = pass2_build_redact_clusters(&candidates, total_pages);
        assert_eq!(clusters.len(), 1, "dynamic page numbers should merge into a single cluster");
        assert!(clusters[0].redact, "appearing on 100% of pages must exceed the 30% threshold");
    }

    #[test]
    fn rare_one_off_header_is_not_redacted() {
        // A one-off note that happens to sit in the header zone on a single
        // page out of 20 (5% frequency) must NOT be redacted.
        let page_height = 800.0;
        let total_pages = 20;
        let mut pages = Vec::new();
        for p in 1..=total_pages {
            let elements = if p == 1 {
                vec![el("Draft reviewed by Legal on 2026-01-04", 770.0, p)]
            } else {
                vec![]
            };
            pages.push(PageContent { page_num: p, page_height, page_width: 612.0, elements, vector_lines: vec![] });
        }
        let candidates = pass1_profile_candidates(&pages);
        let clusters = pass2_build_redact_clusters(&candidates, total_pages);
        assert!(!clusters.is_empty());
        assert!(!clusters[0].redact, "a single-page occurrence (5%) must stay below the 30% threshold");
    }

    #[test]
    fn body_text_in_middle_of_page_is_never_a_candidate() {
        // y_norm around 0.5 (middle of page) must never enter header/footer
        // profiling regardless of repetition.
        let page_height = 800.0;
        let pages = vec![PageContent {
            page_num: 1,
            page_height,
            page_width: 612.0,
            elements: vec![el("System Requirements", 400.0, 1)],
            vector_lines: vec![],
        }];
        let candidates = pass1_profile_candidates(&pages);
        assert!(candidates.is_empty());
    }
}
