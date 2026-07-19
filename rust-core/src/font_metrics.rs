use crate::standard_font_metrics;
use lopdf::{Dictionary, Document, Object};

/// The AVG_GLYPH_WIDTH_EM heuristic from before this priority landed, kept
/// as the last-resort fallback for fonts that are neither a standard-14
/// font nor carry their own /Widths or /W array (e.g. a malformed PDF, or
/// an embedded font subset with a missing Widths entry).
const FALLBACK_WIDTH_EM_THOUSANDTHS: f64 = 500.0;

/// Per-font glyph-width source, resolved once per page per font resource
/// name and then consulted per character code during content-stream
/// interpretation. All widths are in "1000 units per em" glyph space, same
/// as the PDF spec's own /Widths convention -- callers multiply by
/// `font_size / 1000.0` to get actual text-space width.
pub enum FontMetrics {
    /// Simple (single-byte code) font with an explicit /Widths array.
    Simple { first_char: i64, widths: Vec<f64>, missing_width: f64 },
    /// Simple font with no /Widths array, falling back to a standard-14
    /// core font's published metrics (see standard_font_metrics.rs).
    StandardTable(&'static [u16; 256]),
    /// Composite (Type0/CID) font. PDF /W arrays map CID -> width; for the
    /// overwhelmingly common Identity-H/Identity-V encodings CID == the
    /// 2-byte character code read directly from the string, which is the
    /// only case handled here (see doc comment on `is_two_byte`).
    Composite { widths_by_cid: std::collections::HashMap<u32, f64>, default_width: f64 },
    /// Neither of the above -- last-resort constant estimate.
    Fallback,
}

impl FontMetrics {
    /// Width of the glyph for `code`, as a fraction of font size (i.e.
    /// already divided by 1000).
    pub fn width_fraction(&self, code: u32) -> f64 {
        let thousandths = match self {
            FontMetrics::Simple { first_char, widths, missing_width } => {
                let idx = code as i64 - first_char;
                if idx >= 0 && (idx as usize) < widths.len() {
                    widths[idx as usize]
                } else {
                    *missing_width
                }
            }
            FontMetrics::StandardTable(table) => {
                if (code as usize) < 256 {
                    table[code as usize] as f64
                } else {
                    FALLBACK_WIDTH_EM_THOUSANDTHS
                }
            }
            FontMetrics::Composite { widths_by_cid, default_width } => {
                *widths_by_cid.get(&code).unwrap_or(default_width)
            }
            FontMetrics::Fallback => FALLBACK_WIDTH_EM_THOUSANDTHS,
        };
        thousandths / 1000.0
    }

    /// Whether character codes for this font should be read 2 bytes at a
    /// time (true composite/CID fonts) rather than 1 byte at a time. See
    /// the caveat on `parse_font_dict` about non-Identity CMaps.
    pub fn is_two_byte(&self) -> bool {
        matches!(self, FontMetrics::Composite { .. })
    }
}

fn dict_get_name<'a>(dict: &'a Dictionary, key: &[u8]) -> Option<&'a str> {
    dict.get(key).ok().and_then(|o| o.as_name_str().ok())
}

fn resolve_dict<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a Dictionary> {
    match obj {
        Object::Dictionary(d) => Some(d),
        Object::Reference(id) => doc.get_object(*id).ok().and_then(|o| o.as_dict().ok()),
        _ => None,
    }
}

fn as_f64(obj: &Object) -> f64 {
    match obj {
        Object::Integer(i) => *i as f64,
        Object::Real(r) => *r as f64,
        _ => 0.0,
    }
}

/// Parses a /W array for composite fonts, which uses two run-length forms
/// per the PDF spec (9.7.4.3):
///   c [w1 w2 ... wn]   -- individual widths for CIDs c, c+1, ..., c+n-1
///   cFirst cLast w     -- one width w for the whole inclusive CID range
fn parse_w_array(doc: &Document, w_array: &[Object]) -> std::collections::HashMap<u32, f64> {
    let mut widths = std::collections::HashMap::new();
    let mut i = 0;
    while i < w_array.len() {
        let c_first = match &w_array[i] {
            Object::Integer(n) => *n,
            _ => break,
        };
        i += 1;
        if i >= w_array.len() {
            break;
        }
        match &w_array[i] {
            Object::Array(list) => {
                for (offset, w) in list.iter().enumerate() {
                    widths.insert((c_first as u32).wrapping_add(offset as u32), as_f64(w));
                }
                i += 1;
            }
            Object::Reference(id) => {
                if let Ok(Object::Array(list)) = doc.get_object(*id) {
                    for (offset, w) in list.iter().enumerate() {
                        widths.insert((c_first as u32).wrapping_add(offset as u32), as_f64(w));
                    }
                }
                i += 1;
            }
            Object::Integer(_) | Object::Real(_) => {
                let c_last = match &w_array[i] {
                    Object::Integer(n) => *n,
                    Object::Real(r) => *r as i64,
                    _ => break,
                };
                i += 1;
                if i >= w_array.len() {
                    break;
                }
                let w = as_f64(&w_array[i]);
                i += 1;
                let mut cid = c_first;
                while cid <= c_last {
                    widths.insert(cid as u32, w);
                    cid += 1;
                }
            }
            _ => break,
        }
    }
    widths
}

/// Parses a font resource dictionary into a `FontMetrics`. Handles, in
/// order of precedence:
///   1. Simple font with an explicit /Widths array (embedded/subset fonts
///      -- the common case for anything produced by Word, LaTeX, or a
///      browser's print-to-PDF).
///   2. Simple font with no /Widths array, whose /BaseFont matches (after
///      normalizing common aliases like Arial->Helvetica) one of the 14
///      standard fonts every PDF-conforming reader must support metrics
///      for -- the common case for lightweight generators like reportlab
///      that reference base fonts directly (this is exactly what our own
///      test fixture does; see resources/README.md).
///   3. Composite (Type0) font with a /DescendantFonts[0]/W array.
///   4. Fallback constant, for anything else (non-standard base font with
///      no Widths array -- rare and technically a malformed PDF, but we
///      should never panic on it).
///
/// Known limitation: composite-font handling assumes 2-byte character
/// codes (true for /Encoding Identity-H or Identity-V, which covers the
/// large majority of Type0 fonts produced by modern tools). A font using a
/// genuine variable-width CMap would need the CMap itself parsed to know
/// codespace boundaries, which is not implemented here.
pub fn parse_font_dict(doc: &Document, font_dict: &Dictionary) -> FontMetrics {
    let subtype = dict_get_name(font_dict, b"Subtype").unwrap_or("");

    if subtype == "Type0" {
        if let Ok(Object::Array(descendants)) = font_dict.get(b"DescendantFonts") {
            if let Some(desc_obj) = descendants.first() {
                if let Some(desc_dict) = resolve_dict(doc, desc_obj) {
                    let default_width = desc_dict
                        .get(b"DW")
                        .map(as_f64)
                        .unwrap_or(1000.0); // PDF spec default DW when absent
                    let widths_by_cid = match desc_dict.get(b"W") {
                        Ok(Object::Array(w)) => parse_w_array(doc, w),
                        Ok(Object::Reference(id)) => match doc.get_object(*id) {
                            Ok(Object::Array(w)) => parse_w_array(doc, w),
                            _ => std::collections::HashMap::new(),
                        },
                        _ => std::collections::HashMap::new(),
                    };
                    return FontMetrics::Composite { widths_by_cid, default_width };
                }
            }
        }
        return FontMetrics::Fallback;
    }

    // Simple font: explicit /Widths array takes precedence.
    if let (Ok(first_char_obj), Ok(Object::Array(widths_arr))) =
        (font_dict.get(b"FirstChar"), font_dict.get(b"Widths"))
    {
        let first_char = match first_char_obj {
            Object::Integer(n) => *n,
            Object::Real(r) => *r as i64,
            _ => 0,
        };
        let widths: Vec<f64> = widths_arr.iter().map(as_f64).collect();
        let missing_width = font_dict
            .get(b"FontDescriptor")
            .ok()
            .and_then(|o| resolve_dict(doc, o))
            .and_then(|fd| fd.get(b"MissingWidth").ok())
            .map(as_f64)
            .unwrap_or(FALLBACK_WIDTH_EM_THOUSANDTHS);
        return FontMetrics::Simple { first_char, widths, missing_width };
    }

    // No /Widths: try the standard-14 table.
    if let Some(base_font) = dict_get_name(font_dict, b"BaseFont") {
        // BaseFont names are sometimes prefixed with a subset tag like
        // "ABCDEF+Helvetica" (6 uppercase letters + '+'); strip it before
        // matching against the standard-14 names.
        let stripped = base_font
            .len()
            .checked_sub(7)
            .and_then(|_| base_font.get(6..7))
            .filter(|c| *c == "+")
            .map(|_| &base_font[7..])
            .unwrap_or(base_font);
        let normalized = standard_font_metrics::normalize_base_font_name(stripped);
        let encoding = dict_get_name(font_dict, b"Encoding").unwrap_or("StandardEncoding");
        if let Some(table) = standard_font_metrics::standard_font_table(normalized, encoding) {
            return FontMetrics::StandardTable(table);
        }
    }

    FontMetrics::Fallback
}

/// Resolves every font resource used on a page into its `FontMetrics`,
/// keyed by resource name (e.g. "F1", matching the operand of the `Tf`
/// operator) so the content-stream interpreter can look widths up by
/// simple string key with no further PDF-object traversal per glyph.
pub fn resolve_page_fonts(doc: &Document, page_id: lopdf::ObjectId) -> std::collections::HashMap<String, FontMetrics> {
    doc.get_page_fonts(page_id)
        .into_iter()
        .map(|(name, dict)| (String::from_utf8_lossy(&name).to_string(), parse_font_dict(doc, dict)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_font_dict(first_char: i64, widths: Vec<i64>) -> Dictionary {
        let mut d = Dictionary::new();
        d.set("Subtype", Object::Name(b"Type1".to_vec()));
        d.set("FirstChar", Object::Integer(first_char));
        d.set("Widths", Object::Array(widths.into_iter().map(Object::Integer).collect()));
        d
    }

    #[test]
    fn simple_font_widths_are_read_correctly() {
        let doc = Document::new();
        let dict = simple_font_dict(65, vec![600, 700, 800]); // codes 65,66,67 -> A,B,C
        let metrics = parse_font_dict(&doc, &dict);
        assert_eq!(metrics.width_fraction(65), 0.6);
        assert_eq!(metrics.width_fraction(66), 0.7);
        assert_eq!(metrics.width_fraction(67), 0.8);
    }

    #[test]
    fn out_of_range_code_uses_missing_width_fallback() {
        let doc = Document::new();
        let dict = simple_font_dict(65, vec![600]);
        let metrics = parse_font_dict(&doc, &dict);
        // code 90 ('Z') is outside [65,65] -> falls back to default MissingWidth.
        assert_eq!(metrics.width_fraction(90), 0.5);
    }

    #[test]
    fn missing_widths_array_falls_back_to_standard_helvetica_table() {
        let doc = Document::new();
        let mut d = Dictionary::new();
        d.set("Subtype", Object::Name(b"Type1".to_vec()));
        d.set("BaseFont", Object::Name(b"Helvetica".to_vec()));
        d.set("Encoding", Object::Name(b"WinAnsiEncoding".to_vec()));
        let metrics = parse_font_dict(&doc, &d);
        // 'A' = code 65, known Helvetica width 667/1000.
        assert_eq!(metrics.width_fraction(65), 0.667);
    }

    #[test]
    fn arial_aliases_to_helvetica_metrics() {
        let doc = Document::new();
        let mut d = Dictionary::new();
        d.set("Subtype", Object::Name(b"TrueType".to_vec()));
        d.set("BaseFont", Object::Name(b"Arial,Bold".to_vec()));
        d.set("Encoding", Object::Name(b"WinAnsiEncoding".to_vec()));
        let metrics = parse_font_dict(&doc, &d);
        assert_eq!(metrics.width_fraction(65), standard_font_metrics::HELVETICA_BOLD_WINANSIENCODING[65] as f64 / 1000.0);
    }

    #[test]
    fn subset_prefix_is_stripped_before_matching() {
        let doc = Document::new();
        let mut d = Dictionary::new();
        d.set("Subtype", Object::Name(b"TrueType".to_vec()));
        d.set("BaseFont", Object::Name(b"ABCDEF+Helvetica".to_vec()));
        d.set("Encoding", Object::Name(b"WinAnsiEncoding".to_vec()));
        let metrics = parse_font_dict(&doc, &d);
        assert_eq!(metrics.width_fraction(65), 0.667);
    }

    #[test]
    fn w_array_range_form_is_parsed() {
        let doc = Document::new();
        let mut d = Dictionary::new();
        d.set("Subtype", Object::Name(b"Type0".to_vec()));
        let mut desc = Dictionary::new();
        desc.set(
            "W",
            Object::Array(vec![
                Object::Integer(10),
                Object::Integer(20),
                Object::Integer(500), // CIDs 10..=20 all width 500
            ]),
        );
        desc.set("DW", Object::Integer(1000));
        d.set("DescendantFonts", Object::Array(vec![Object::Dictionary(desc)]));
        let metrics = parse_font_dict(&doc, &d);
        assert_eq!(metrics.width_fraction(15), 0.5);
        assert_eq!(metrics.width_fraction(999), 1.0); // falls back to DW
        assert!(metrics.is_two_byte());
    }

    #[test]
    fn w_array_list_form_is_parsed() {
        let doc = Document::new();
        let mut d = Dictionary::new();
        d.set("Subtype", Object::Name(b"Type0".to_vec()));
        let mut desc = Dictionary::new();
        desc.set(
            "W",
            Object::Array(vec![
                Object::Integer(100),
                Object::Array(vec![Object::Integer(300), Object::Integer(400), Object::Integer(500)]),
            ]),
        );
        d.set("DescendantFonts", Object::Array(vec![Object::Dictionary(desc)]));
        let metrics = parse_font_dict(&doc, &d);
        assert_eq!(metrics.width_fraction(100), 0.3);
        assert_eq!(metrics.width_fraction(101), 0.4);
        assert_eq!(metrics.width_fraction(102), 0.5);
    }
}
