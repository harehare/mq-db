use mq_markdown::Node;

use crate::block::{Block, BlockId, BlockType, DocumentId, Properties, PropertyValue, Span};

// ─────────────────────────────────────────────────────────────────────────────
// Helper: extract span from a node's position
// ─────────────────────────────────────────────────────────────────────────────

fn node_to_span(node: &Node) -> Option<Span> {
    node.position().map(|p| Span {
        start_line: p.start.line,
        start_col: p.start.column,
        end_line: p.end.line,
        end_col: p.end.column,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Helper: if a node is a Heading, return its depth
// ─────────────────────────────────────────────────────────────────────────────

fn heading_depth(node: &Node) -> Option<u8> {
    if let Node::Heading(h) = node {
        Some(h.depth)
    } else {
        None
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helper: convert a serde_yaml Value to a PropertyValue
// ─────────────────────────────────────────────────────────────────────────────

fn yaml_value_to_property(v: serde_yaml::Value) -> PropertyValue {
    match v {
        serde_yaml::Value::Null => PropertyValue::Null,
        serde_yaml::Value::Bool(b) => PropertyValue::Bool(b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                PropertyValue::Int(i)
            } else if let Some(f) = n.as_f64() {
                PropertyValue::Float(f)
            } else {
                PropertyValue::String(n.to_string())
            }
        }
        serde_yaml::Value::String(s) => PropertyValue::String(s),
        serde_yaml::Value::Sequence(seq) => {
            PropertyValue::Array(seq.into_iter().map(yaml_value_to_property).collect())
        }
        serde_yaml::Value::Mapping(_) => PropertyValue::String(format!("{v:?}")),
        serde_yaml::Value::Tagged(t) => yaml_value_to_property(t.value),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Core conversion: Node → (BlockType, content, Properties)
// Returns None for nodes that should be skipped (Fragment, Empty).
// ─────────────────────────────────────────────────────────────────────────────

fn node_to_parts(node: &Node) -> Option<(BlockType, String, Properties)> {
    let mut props = Properties::new();

    match node {
        Node::Heading(h) => {
            props.set("depth", PropertyValue::Int(h.depth as i64));
            let content: String = h.values.iter().map(|n| n.value()).collect();
            let slug = content
                .to_lowercase()
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == ' ' {
                        c
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join("-");
            props.set("slug", PropertyValue::String(slug));
            Some((BlockType::Heading, content, props))
        }

        Node::Code(c) => {
            if let Some(lang) = &c.lang {
                props.set("lang", PropertyValue::String(lang.clone()));
            }
            if let Some(meta) = &c.meta {
                props.set("meta", PropertyValue::String(meta.clone()));
            }
            props.set("fence", PropertyValue::Bool(c.fence));
            Some((BlockType::Code, c.value.clone(), props))
        }

        Node::List(l) => {
            props.set("ordered", PropertyValue::Bool(l.ordered));
            props.set("level", PropertyValue::Int(l.level as i64));
            if let Some(checked) = l.checked {
                props.set("checked", PropertyValue::Bool(checked));
            }
            let content: String = l.values.iter().map(|n| n.value()).collect();
            Some((BlockType::List, content, props))
        }

        Node::TableRow(r) => {
            let content: String = r.values.iter().map(|n| n.value()).collect();
            Some((BlockType::TableRow, content, props))
        }

        Node::TableCell(c) => {
            props.set("row", PropertyValue::Int(c.row as i64));
            props.set("column", PropertyValue::Int(c.column as i64));
            let content: String = c.values.iter().map(|n| n.value()).collect();
            Some((BlockType::TableCell, content, props))
        }

        Node::TableAlign(_) => Some((BlockType::TableAlign, String::new(), props)),

        Node::Blockquote(b) => {
            let content: String = b.values.iter().map(|n| n.value()).collect();
            Some((BlockType::Blockquote, content, props))
        }

        Node::Html(h) => Some((BlockType::Html, h.value.clone(), props)),

        Node::Yaml(y) => {
            // Parse YAML frontmatter and promote key-value pairs as properties
            if let Ok(serde_yaml::Value::Mapping(map)) = serde_yaml::from_str::<serde_yaml::Value>(&y.value) {
                for (k, v) in map {
                    if let serde_yaml::Value::String(key) = k {
                        props.set(key, yaml_value_to_property(v));
                    }
                }
            }
            Some((BlockType::Yaml, y.value.clone(), props))
        }

        Node::Toml(t) => Some((BlockType::Toml, t.value.clone(), props)),

        Node::Math(m) => Some((BlockType::Math, m.value.clone(), props)),

        Node::Definition(d) => {
            props.set("url", PropertyValue::String(d.url.as_str().to_string()));
            if let Some(label) = &d.label {
                props.set("label", PropertyValue::String(label.clone()));
            }
            Some((BlockType::Definition, d.ident.clone(), props))
        }

        Node::Footnote(f) => {
            let content: String = f.values.iter().map(|n| n.value()).collect();
            props.set("ident", PropertyValue::String(f.ident.clone()));
            Some((BlockType::Footnote, content, props))
        }

        Node::HorizontalRule(_) => Some((BlockType::HorizontalRule, String::new(), props)),

        // Inline / paragraph-level nodes – all map to Paragraph blocks.
        // mq-markdown flattens mdast Paragraph children into the top-level
        // node list, so Text, Emphasis, Strong, etc. can appear at the top
        // level and represent paragraph content.
        Node::Text(_)
        | Node::Emphasis(_)
        | Node::Strong(_)
        | Node::Delete(_)
        | Node::Link(_)
        | Node::LinkRef(_)
        | Node::Image(_)
        | Node::ImageRef(_)
        | Node::CodeInline(_)
        | Node::MathInline(_)
        | Node::FootnoteRef(_)
        | Node::Break(_)
        | Node::MdxFlowExpression(_)
        | Node::MdxJsxFlowElement(_)
        | Node::MdxJsxTextElement(_)
        | Node::MdxTextExpression(_)
        | Node::MdxJsEsm(_) => Some((BlockType::Paragraph, node.value(), props)),

        Node::Fragment(_) | Node::Empty => None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Public API: build_blocks
// ─────────────────────────────────────────────────────────────────────────────

/// Converts a flat mq-markdown node list into a [`Vec<Block>`] with
/// **Nested Set / Pre-Post Order** interval indexing.
///
/// ## Algorithm
///
/// **Phase 1 – section tree construction:**  
/// Walk the flat node list and assign each node as a child of its enclosing
/// heading section. Headings push a new "section scope" onto a stack;
/// non-heading nodes belong to the innermost open scope.
///
/// **Phase 2 – DFS interval assignment:**  
/// Iterative DFS assigns a strictly increasing `pre` counter on entry and a
/// `post` counter on exit for each tree slot. This produces the invariant:
/// ```text
/// ancestor.pre < descendant.pre  &&  descendant.post < ancestor.post
/// ```
///
/// **Phase 3 – Block construction:**  
/// Each accepted node is converted to a [`Block`] using the slot's pre/post
/// pair and the type-specific properties extracted in [`node_to_parts`].
pub fn build_blocks(doc_id: DocumentId, nodes: &[Node]) -> Vec<Block> {
    let n = nodes.len();
    if n == 0 {
        return Vec::new();
    }

    // ── Phase 1: build section tree ──────────────────────────────────────────
    //
    // `children[slot]` holds the tree-slot indices of slot's direct children.
    // Slot 0 is the virtual document root; nodes start at slot 1.

    let mut children: Vec<Vec<usize>> = Vec::with_capacity(n + 1);
    children.push(Vec::new()); // slot 0 = root

    // node_slot[i] = tree slot index for nodes[i]
    let mut node_slot: Vec<usize> = Vec::with_capacity(n);

    // Stack: (slot_index, heading_depth). Starts with the virtual root at depth 0.
    let mut stack: Vec<(usize, u8)> = vec![(0, 0)];

    for node in nodes.iter() {
        let slot = children.len();
        children.push(Vec::new());
        node_slot.push(slot);

        if let Some(depth) = heading_depth(node) {
            // Close sections that are at the same or deeper nesting level
            while let Some(&(_, d)) = stack.last() {
                if d >= depth {
                    stack.pop();
                } else {
                    break;
                }
            }
            let parent = stack.last().map_or(0, |&(s, _)| s);
            children[parent].push(slot);
            stack.push((slot, depth));
        } else {
            let parent = stack.last().map_or(0, |&(s, _)| s);
            children[parent].push(slot);
        }
    }

    // ── Phase 2: iterative DFS to assign pre/post numbers ────────────────────

    let num_slots = children.len();
    let mut pre = vec![0u32; num_slots];
    let mut post = vec![0u32; num_slots];
    let mut counter = 0u32;

    // Stack entries: (slot_index, next_child_to_visit)
    let mut dfs: Vec<(usize, usize)> = Vec::with_capacity(num_slots);
    pre[0] = counter;
    counter += 1;
    dfs.push((0, 0));

    while let Some(frame) = dfs.last_mut() {
        let slot = frame.0;
        let child_idx = frame.1;

        if child_idx < children[slot].len() {
            let child = children[slot][child_idx];
            frame.1 += 1; // advance before pushing
            pre[child] = counter;
            counter += 1;
            dfs.push((child, 0));
        } else {
            post[slot] = counter;
            counter += 1;
            dfs.pop();
        }
    }

    // ── Phase 3: construct Block objects ─────────────────────────────────────

    let mut blocks: Vec<Block> = Vec::with_capacity(n);
    let mut next_id: BlockId = 0;

    for (idx, node) in nodes.iter().enumerate() {
        let slot = node_slot[idx];
        if let Some((block_type, content, properties)) = node_to_parts(node) {
            blocks.push(Block {
                id: next_id,
                document_id: doc_id,
                block_type,
                content,
                span: node_to_span(node),
                pre: pre[slot],
                post: post[slot],
                properties,
            });
            next_id += 1;
        }
    }

    blocks
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use mq_markdown::Markdown;

    fn parse_blocks(md: &str) -> Vec<Block> {
        let doc = md.parse::<Markdown>().unwrap();
        build_blocks(0, &doc.nodes)
    }

    #[test]
    fn test_heading_depth_property() {
        let blocks = parse_blocks("## Section\n\nParagraph\n");
        let h = blocks
            .iter()
            .find(|b| b.block_type == BlockType::Heading)
            .unwrap();
        assert_eq!(h.heading_depth(), Some(2));
        assert_eq!(h.content, "Section");
    }

    #[test]
    fn test_code_lang_property() {
        let blocks = parse_blocks("```rust\nfn main() {}\n```\n");
        let c = blocks
            .iter()
            .find(|b| b.block_type == BlockType::Code)
            .unwrap();
        assert_eq!(c.code_lang(), Some("rust"));
    }

    #[test]
    fn test_interval_index_ancestor_check() {
        let md = "# H1\n\npara1\n\n## H2\n\npara2\n\n# H1b\n\npara3\n";
        let blocks = parse_blocks(md);

        let h1 = blocks
            .iter()
            .find(|b| {
                b.block_type == BlockType::Heading
                    && b.heading_depth() == Some(1)
                    && b.content == "H1"
            })
            .unwrap();
        let h2 = blocks
            .iter()
            .find(|b| b.block_type == BlockType::Heading && b.heading_depth() == Some(2))
            .unwrap();
        let para2 = blocks
            .iter()
            .find(|b| b.block_type == BlockType::Paragraph && b.content == "para2")
            .unwrap();
        let para3 = blocks
            .iter()
            .find(|b| b.block_type == BlockType::Paragraph && b.content == "para3")
            .unwrap();

        // para2 is under both H1 and H2
        assert!(para2.is_under(h1), "para2 should be under H1");
        assert!(para2.is_under(h2), "para2 should be under H2");

        // para3 is NOT under H1 (it belongs to H1b)
        assert!(!para3.is_under(h1), "para3 should not be under first H1");

        // H2 is under H1
        assert!(h2.is_under(h1), "H2 should be under H1");
    }

    #[test]
    fn test_sibling_via_post_plus_one() {
        let md = "## A\n\n## B\n\n## C\n";
        let blocks = parse_blocks(md);

        let a = blocks.iter().find(|b| b.content == "A").unwrap();
        let b_block = blocks.iter().find(|b| b.content == "B").unwrap();

        // For headings with no children (leaf sections), B.pre == A.post + 1
        assert_eq!(
            b_block.pre,
            a.post + 1,
            "B.pre should be A.post + 1 (next sibling)"
        );
    }

    #[test]
    fn test_first_child_via_pre_plus_one() {
        let md = "## Section\n\nContent paragraph\n";
        let blocks = parse_blocks(md);

        let heading = blocks
            .iter()
            .find(|b| b.block_type == BlockType::Heading)
            .unwrap();
        let para = blocks
            .iter()
            .find(|b| b.block_type == BlockType::Paragraph)
            .unwrap();

        // The paragraph is the first child of the heading section
        assert_eq!(
            para.pre,
            heading.pre + 1,
            "first child pre == heading.pre + 1"
        );
    }

    #[test]
    fn test_yaml_frontmatter_properties() {
        let md = "---\ntitle: My Doc\ntags:\n  - rust\n  - db\n---\n\n# Hello\n";
        let blocks = parse_blocks(md);

        let yaml = blocks
            .iter()
            .find(|b| b.block_type == BlockType::Yaml)
            .unwrap();
        assert_eq!(
            yaml.properties.get("title").and_then(|v| v.as_str()),
            Some("My Doc")
        );
        let tags = yaml
            .properties
            .get("tags")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].as_str(), Some("rust"));
    }
}
