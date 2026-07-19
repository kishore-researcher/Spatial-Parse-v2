use crate::types::ChunkPayload;
use std::sync::OnceLock;
use tokenizers::Tokenizer;

pub const T_MAX_TOKENS: usize = 512;
const MAX_HEADER_DEPTH: usize = 6;

#[derive(Debug, Clone, PartialEq)]
enum NodeType {
    Header(usize),
    Paragraph,
    Table,
    List,
}

struct MdNode {
    node_type: NodeType,
    text: String,
    page_num: usize,
}

/// Vendored, offline, cl100k_base-equivalent BPE vocabulary + merges,
/// converted from OpenAI's published tiktoken rank table (MIT-licensed
/// mirror; see resources/README.md for provenance) into the tokenizers
/// crate's JSON schema. Embedded at compile time via include_str! so there
/// is zero runtime network dependency, satisfying the network-isolated
/// execution constraint.
static TOKENIZER_JSON: &str = include_str!("../resources/tokenizer.json");
static TOKENIZER: OnceLock<Tokenizer> = OnceLock::new();

/// Loads (once) and returns the shared tokenizer instance. Parsing a
/// 100k-vocab/100k-merge JSON document is not free -- doing it once via
/// OnceLock rather than on every `get_exact_token_count` call (as a naive
/// `Tokenizer::from_str(...).unwrap()` inside the function body would) is
/// the difference between a one-time ~50ms cost and paying that cost for
/// every single paragraph/table/list node the chunker evaluates, which for
/// a 500-page document would make chunking itself the bottleneck of the
/// whole pipeline.
fn get_tokenizer() -> &'static Tokenizer {
    TOKENIZER.get_or_init(|| {
        Tokenizer::from_bytes(TOKENIZER_JSON.as_bytes()).unwrap_or_else(|e| {
            // This is a build-time asset, not user input -- a failure here
            // means resources/tokenizer.json itself is corrupted, which is
            // a packaging bug, not a runtime condition callers can recover
            // from.
            panic!("embedded tokenizer.json failed to parse: {e}")
        })
    })
}

/// Exact token count via the real (offline, embedded) BPE tokenizer,
/// replacing the earlier `words * 1.3` heuristic now that we have a fully
/// offline tokenizer available. Named to match the sprint spec's requested
/// `get_exact_token_count`; `chunk_document` below calls this exclusively.
pub fn get_exact_token_count(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    match get_tokenizer().encode(text, false) {
        Ok(encoding) => encoding.get_ids().len(),
        Err(_) => {
            // encode() essentially never fails for a BPE tokenizer on
            // arbitrary UTF-8 text, but a single malformed node must never
            // panic the whole chunking pass -- fall back to a conservative
            // estimate rather than propagate the error.
            text.split_whitespace().count()
        }
    }
}

/// Tracks ancestral header lineage: H_context = [h_1, h_2, ..., h_d].
/// Updating at depth `d` clears every deeper slot, matching the semantics of
/// a breadcrumb trail (entering a new "## Foo" ends whatever "### Bar"
/// sub-section we were previously inside).
#[derive(Default, Clone)]
struct HeaderContext {
    slots: [Option<String>; MAX_HEADER_DEPTH],
}

impl HeaderContext {
    fn update_at_depth(&mut self, level: usize, text: &str) {
        let idx = level.saturating_sub(1).min(MAX_HEADER_DEPTH - 1);
        self.slots[idx] = Some(text.to_string());
        for slot in self.slots.iter_mut().skip(idx + 1) {
            *slot = None;
        }
    }

    fn to_list(&self) -> Vec<String> {
        self.slots.iter().filter_map(|s| s.clone()).collect()
    }

    fn to_string_for_tokens(&self) -> String {
        self.to_list().join(" > ")
    }
}

#[derive(Default)]
struct ChunkAccumulator {
    nodes: Vec<(NodeType, String, usize)>, // (type, text, page_num)
    current_tokens: usize,
    injected_context: Option<HeaderContext>,
}

impl ChunkAccumulator {
    fn add_node(&mut self, node_type: NodeType, text: String, page_num: usize, tokens: usize) {
        self.current_tokens += tokens;
        self.nodes.push((node_type, text, page_num));
    }

    fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    fn compile_to_markdown(&self) -> String {
        self.nodes
            .iter()
            .map(|(_, text, _)| text.clone())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn source_pages(&self) -> Vec<usize> {
        let mut pages: Vec<usize> = self.nodes.iter().map(|(_, _, p)| *p).collect();
        pages.sort_unstable();
        pages.dedup();
        pages
    }

    fn clear(&mut self) {
        self.nodes.clear();
        self.current_tokens = 0;
        self.injected_context = None;
    }

    fn inject_context(&mut self, ctx: &HeaderContext) {
        self.injected_context = Some(ctx.clone());
    }
}

/// Very small Markdown line classifier: header / table-row / paragraph.
/// (List detection kept simple and permissive -- "-", "*", or "N." prefixes.)
fn classify_and_group(markdown_by_page: &[(usize, String)]) -> Vec<MdNode> {
    let mut nodes = Vec::new();
    let mut para_buf: Vec<&str> = Vec::new();
    let mut para_page: Option<usize> = None;

    let flush_para = |buf: &mut Vec<&str>, page: &mut Option<usize>, nodes: &mut Vec<MdNode>| {
        if !buf.is_empty() {
            let text = buf.join(" ").trim().to_string();
            if !text.is_empty() {
                nodes.push(MdNode {
                    node_type: NodeType::Paragraph,
                    text,
                    page_num: page.unwrap_or(0),
                });
            }
            buf.clear();
            *page = None;
        }
    };

    for (page_num, page_md) in markdown_by_page {
        let mut lines = page_md.lines().peekable();
        while let Some(line) = lines.next() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                flush_para(&mut para_buf, &mut para_page, &mut nodes);
                continue;
            }
            if let Some(level) = header_level(trimmed) {
                flush_para(&mut para_buf, &mut para_page, &mut nodes);
                let text = trimmed.trim_start_matches('#').trim().to_string();
                nodes.push(MdNode { node_type: NodeType::Header(level), text, page_num: *page_num });
            } else if trimmed.starts_with('|') {
                flush_para(&mut para_buf, &mut para_page, &mut nodes);
                let mut table_lines = vec![trimmed.to_string()];
                while let Some(next) = lines.peek() {
                    let nt = next.trim();
                    if nt.starts_with('|') {
                        table_lines.push(nt.to_string());
                        lines.next();
                    } else {
                        break;
                    }
                }
                nodes.push(MdNode {
                    node_type: NodeType::Table,
                    text: table_lines.join("\n"),
                    page_num: *page_num,
                });
            } else if trimmed.starts_with('-') || trimmed.starts_with('*') || starts_with_ordinal(trimmed) {
                flush_para(&mut para_buf, &mut para_page, &mut nodes);
                let mut list_lines = vec![trimmed.to_string()];
                while let Some(next) = lines.peek() {
                    let nt = next.trim();
                    if nt.starts_with('-') || nt.starts_with('*') || starts_with_ordinal(nt) {
                        list_lines.push(nt.to_string());
                        lines.next();
                    } else {
                        break;
                    }
                }
                nodes.push(MdNode {
                    node_type: NodeType::List,
                    text: list_lines.join("\n"),
                    page_num: *page_num,
                });
            } else {
                para_buf.push(trimmed);
                para_page.get_or_insert(*page_num);
            }
        }
        flush_para(&mut para_buf, &mut para_page, &mut nodes);
    }
    nodes
}

fn header_level(line: &str) -> Option<usize> {
    if !line.starts_with('#') {
        return None;
    }
    let level = line.chars().take_while(|c| *c == '#').count();
    if level >= 1 && level <= 6 && line.as_bytes().get(level) == Some(&b' ') {
        Some(level)
    } else {
        None
    }
}

fn starts_with_ordinal(line: &str) -> bool {
    let mut chars = line.chars();
    let mut saw_digit = false;
    for c in chars.by_ref() {
        if c.is_ascii_digit() {
            saw_digit = true;
        } else {
            return saw_digit && c == '.';
        }
    }
    false
}

fn node_type_label(nt: &NodeType) -> &'static str {
    match nt {
        NodeType::Header(_) => "header",
        NodeType::Paragraph => "paragraph",
        NodeType::Table => "table",
        NodeType::List => "list",
    }
}

/// Runs the AST tree-walking chunker described in the spec: headers update
/// ancestral context; paragraph/table/list nodes accumulate into a chunk
/// until adding one would exceed T_MAX_TOKENS (including the ancestral
/// context's own token cost), at which point the current chunk is emitted
/// and a new one starts, re-seeded with the same context.
pub fn chunk_document(
    markdown_by_page: &[(usize, String)],
    document_id: &str,
) -> Vec<ChunkPayload> {
    let nodes = classify_and_group(markdown_by_page);
    let mut context = HeaderContext::default();
    let mut accumulator = ChunkAccumulator::default();
    let mut chunks = Vec::new();
    let mut chunk_id = 0;

    let emit = |acc: &mut ChunkAccumulator, chunks: &mut Vec<ChunkPayload>, id: &mut usize| {
        if acc.is_empty() {
            return;
        }
        let ctx = acc.injected_context.clone().unwrap_or_default();
        chunks.push(ChunkPayload {
            chunk_id: *id,
            text: acc.compile_to_markdown(),
            parent_hierarchy: ctx.to_list(),
            root_document: document_id.to_string(),
            token_count: acc.current_tokens,
            source_pages: acc.source_pages(),
        });
        *id += 1;
    };

    for node in nodes {
        match node.node_type {
            NodeType::Header(level) => {
                let idx = level.saturating_sub(1).min(MAX_HEADER_DEPTH - 1);
                let is_same_section = context.slots[idx].as_deref() == Some(node.text.as_str());
                if !is_same_section {
                    // A genuinely new header value starts a new section. If
                    // the accumulator already holds content from the
                    // *previous* section, flush it now under its own
                    // (still-correct) context before the context changes --
                    // otherwise a short section could get silently merged
                    // into the next one's chunk while keeping the stale
                    // parent_hierarchy, mislabeling real content. This is a
                    // deliberate strengthening of the spec's literal
                    // pseudocode (which only flushes on token overflow);
                    // see README "Design corrections".
                    //
                    // Repeats of the *same* header text (e.g. a running
                    // "Appendix Notes" sub-heading printed again on every
                    // page of a long section) are deliberately treated as a
                    // continuation, not a new section -- otherwise every
                    // single page would fragment into its own near-empty
                    // chunk, which defeats the purpose of chunking at all.
                    if !accumulator.is_empty() {
                        emit(&mut accumulator, &mut chunks, &mut chunk_id);
                        accumulator.clear();
                    }
                    context.update_at_depth(level, &node.text);
                }
            }
            NodeType::Paragraph | NodeType::Table | NodeType::List => {
                let node_tokens = get_exact_token_count(&node.text);
                let context_tokens = get_exact_token_count(&context.to_string_for_tokens());
                let rendered_text = match node.node_type {
                    NodeType::Table => node.text.clone(),
                    NodeType::List => node.text.clone(),
                    _ => node.text.clone(),
                };
                let _ = node_type_label(&node.node_type);

                if accumulator.current_tokens + node_tokens + context_tokens <= T_MAX_TOKENS
                    || accumulator.is_empty()
                {
                    // The `is_empty()` escape hatch guarantees progress: a
                    // single oversized node (e.g. a huge table) still gets
                    // emitted as its own chunk rather than looping forever
                    // trying to fit it into zero remaining budget.
                    accumulator.add_node(node.node_type.clone(), rendered_text, node.page_num, node_tokens);
                    if accumulator.injected_context.is_none() {
                        accumulator.inject_context(&context);
                    }
                } else {
                    emit(&mut accumulator, &mut chunks, &mut chunk_id);
                    accumulator.clear();
                    accumulator.inject_context(&context);
                    accumulator.add_node(node.node_type.clone(), rendered_text, node.page_num, node_tokens);
                }
            }
        }
    }
    emit(&mut accumulator, &mut chunks, &mut chunk_id); // flush trailing chunk

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_context_inherits_across_chunk_boundary() {
        let long_para = "word ".repeat(700); // ~701 real BPE tokens, comfortably over the 512 budget
        let markdown_by_page = vec![
            (1, format!("# Product X\n\n## System Requirements\n\n{long_para}")),
            (2, "Second paragraph that continues the same section.".to_string()),
        ];
        let chunks = chunk_document(&markdown_by_page, "doc1");
        assert!(chunks.len() >= 2, "oversized first paragraph should force at least 2 chunks");
        for c in &chunks {
            assert_eq!(c.parent_hierarchy, vec!["Product X".to_string(), "System Requirements".to_string()]);
            assert_eq!(c.root_document, "doc1");
        }
    }

    #[test]
    fn new_h1_resets_deeper_context() {
        let markdown_by_page = vec![(
            1,
            "# Product X\n\n## System Requirements\n\nText about X.\n\n# Product Y\n\nText about Y with no sub-header.".to_string(),
        )];
        let chunks = chunk_document(&markdown_by_page, "doc1");
        // Find the chunk containing "Product Y" body text and confirm the
        // old "System Requirements" sub-header did NOT leak into its context.
        let y_chunk = chunks.iter().find(|c| c.text.contains("Text about Y")).unwrap();
        assert_eq!(y_chunk.parent_hierarchy, vec!["Product Y".to_string()]);
    }

    #[test]
    fn respects_token_budget() {
        // Each paragraph is ~200 words (~260 estimated tokens), so any two
        // together exceed T_MAX_TOKENS=512 but each individually fits --
        // this exercises real budget-driven splitting, as opposed to the
        // single-oversized-node escape hatch covered by a separate test.
        let para = "word ".repeat(200);
        let markdown_by_page = vec![(1, format!("# Title\n\n{para}\n\n{para}\n\n{para}"))];
        let chunks = chunk_document(&markdown_by_page, "doc1");
        for c in &chunks {
            assert!(
                c.token_count <= T_MAX_TOKENS,
                "chunk exceeded token budget: {} tokens",
                c.token_count
            );
        }
        assert!(chunks.len() >= 2, "three ~260-token paragraphs must not fit in a single 512-token chunk");
    }

    #[test]
    fn repeated_identical_header_does_not_fragment_chunks() {
        // The same "## Appendix Notes" sub-heading printed again on every
        // page of a long running section must be treated as a continuation
        // of one section, not as N new sections each forcing their own
        // near-empty chunk.
        let mut markdown_by_page = Vec::new();
        for p in 1..=10 {
            markdown_by_page.push((p, format!("## Appendix Notes\n\nShort note on page {p}.")));
        }
        let chunks = chunk_document(&markdown_by_page, "doc1");
        assert_eq!(chunks.len(), 1, "identical repeated headers should merge into a single chunk, got {}", chunks.len());
        assert_eq!(chunks[0].source_pages.len(), 10);
    }

    #[test]
    fn oversized_single_node_still_emits_progress() {
        // A single node larger than T_MAX_TOKENS must still be emitted on
        // its own rather than looping forever waiting for budget that will
        // never arrive.
        let huge = "word ".repeat(2000);
        let markdown_by_page = vec![(1, huge)];
        let chunks = chunk_document(&markdown_by_page, "doc1");
        assert_eq!(chunks.len(), 1);
    }
}
