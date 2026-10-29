#[derive(Debug, PartialEq, Eq)]
pub enum BlockKind<'a> {
    Markdown,
    Code {
        language: &'a str,
        body: &'a str,
    },
    /// A paragraph holding only `![alt](url)`, rendered as its own interactive image.
    Image {
        alt: &'a str,
        url: &'a str,
    },
}

#[derive(Debug)]
pub struct Block<'a> {
    pub start: usize,
    pub source: &'a str,
    pub complete: bool,
    pub kind: BlockKind<'a>,
}

pub fn blocks(text: &str) -> Vec<Block<'_>> {
    let mut blocks = Vec::new();
    let mut start = 0;
    let mut position = 0;
    let mut fence: Option<(u8, usize, &str, usize)> = None;
    for line in text.split_inclusive('\n') {
        let end = position + line.len();
        if let Some((marker, width, _, _)) = fence {
            if let Some((close_marker, close_width, info)) = fence_line(line)
                && close_marker == marker
                && close_width >= width
                && info.is_empty()
            {
                blocks.push(Block {
                    start,
                    source: &text[start..end],
                    complete: true,
                    kind: BlockKind::Markdown,
                });
                start = end;
                fence = None;
            }
        } else if let Some((marker, width, language)) = fence_line(line) {
            if !text[start..position].trim().is_empty() {
                blocks.push(Block {
                    start,
                    source: &text[start..position],
                    complete: true,
                    kind: paragraph_kind(&text[start..position]),
                });
            }
            start = position;
            fence = Some((marker, width, language, end));
        } else if line.ends_with('\n') && line.trim().is_empty() {
            if !text[start..end].trim().is_empty() {
                blocks.push(Block {
                    start,
                    source: &text[start..end],
                    complete: true,
                    kind: paragraph_kind(&text[start..end]),
                });
            }
            start = end;
        }
        position = end;
    }
    if let Some((_, _, language, body_start)) = fence {
        blocks.push(Block {
            start,
            source: &text[start..],
            complete: false,
            kind: BlockKind::Code {
                language,
                body: &text[body_start..],
            },
        });
    } else if !text[start..].trim().is_empty() {
        blocks.push(Block {
            start,
            source: &text[start..],
            complete: false,
            kind: paragraph_kind(&text[start..]),
        });
    }
    blocks
}

fn paragraph_kind(source: &str) -> BlockKind<'_> {
    // Four spaces or a tab start an indented code block, not a paragraph.
    if source.starts_with("    ") || source.starts_with('\t') {
        return BlockKind::Markdown;
    }
    standalone_image(source.trim())
        .map(|(alt, url)| BlockKind::Image { alt, url })
        .unwrap_or(BlockKind::Markdown)
}

/// Parses `![alt](url)` or `![alt](url "title")` when it is the whole paragraph.
fn standalone_image(source: &str) -> Option<(&str, &str)> {
    if source.contains('\n') {
        return None;
    }
    let rest = source.strip_prefix("![")?;
    let (alt, rest) = rest.split_once("](")?;
    if alt.contains(['[', ']']) {
        return None;
    }
    let target = rest.strip_suffix(')')?.trim();
    let (url, title) = match target.strip_prefix('<') {
        Some(target) => target.split_once('>')?,
        None => target
            .split_once(char::is_whitespace)
            .unwrap_or((target, "")),
    };
    let title = title.trim();
    let quoted =
        |open, close| title.len() >= 2 && title.starts_with(open) && title.ends_with(close);
    if url.is_empty()
        || url.contains(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '<' | '>'))
        || !(title.is_empty() || quoted('"', '"') || quoted('\'', '\''))
    {
        return None;
    }
    Some((alt, url))
}

fn fence_line(line: &str) -> Option<(u8, usize, &str)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let marker = *trimmed.as_bytes().first()?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let width = trimmed.bytes().take_while(|byte| *byte == marker).count();
    let info = trimmed[width..].trim();
    if width < 3 || (marker == b'`' && info.contains('`')) {
        return None;
    }
    Some((marker, width, info))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_paragraphs_keep_their_identity_as_the_next_paragraph_grows() {
        let first = blocks("One.\n\nTwo");
        let next = blocks("One.\n\nTwo more words");
        assert_eq!(first.len(), 2);
        assert_eq!(
            (first[0].start, first[0].source, first[0].complete),
            (0, "One.\n\n", true)
        );
        assert_eq!(
            (next[0].start, next[0].source, next[0].complete),
            (0, "One.\n\n", true)
        );
        assert_eq!(
            (next[1].start, next[1].source, next[1].complete),
            (6, "Two more words", false)
        );
    }

    #[test]
    fn unfinished_fenced_code_uses_the_cheap_live_code_path_and_keeps_blank_lines() {
        let parsed = blocks("Hello.\n\n```python\nfor i in range(2):\n\n    print(i)");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].start, 8);
        assert!(!parsed[1].complete);
        assert_eq!(
            parsed[1].kind,
            BlockKind::Code {
                language: "python",
                body: "for i in range(2):\n\n    print(i)"
            }
        );
    }

    #[test]
    fn closing_fences_finalize_code_once_without_treating_shorter_fences_as_closers() {
        let parsed = blocks("~~~~rust\nlet n = 1;\n~~~\n\n~~~~\nAfter");
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].complete);
        assert_eq!(parsed[0].source, "~~~~rust\nlet n = 1;\n~~~\n\n~~~~\n");
        assert_eq!(parsed[1].source, "After");
    }

    #[test]
    fn standalone_image_paragraphs_become_image_blocks() {
        let parsed = blocks(
            "Look:\n\n![A cat](https://example.com/cat.png \"Cat\")\n\n![](data:image/png;base64,AAAA)",
        );
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].kind, BlockKind::Markdown);
        assert_eq!(
            parsed[1].kind,
            BlockKind::Image {
                alt: "A cat",
                url: "https://example.com/cat.png"
            }
        );
        assert_eq!(parsed[2].start, 52);
        assert_eq!(
            parsed[2].kind,
            BlockKind::Image {
                alt: "",
                url: "data:image/png;base64,AAAA"
            }
        );
    }

    #[test]
    fn images_mixed_with_text_or_links_stay_markdown() {
        for source in [
            "See ![a](https://example.com/a.png) here",
            "[![a](https://example.com/a.png)](https://example.com)",
            "![a](https://example.com/a.png) and ![b](https://example.com/b.png)",
            "![a](https://example.com/a.png\n)",
            "![a](https://example.com/a.png",
            "    ![a](https://example.com/a.png)",
        ] {
            assert_eq!(blocks(source)[0].kind, BlockKind::Markdown, "{source}");
        }
    }

    #[test]
    fn unicode_offsets_and_incomplete_opening_fences_stay_valid() {
        let parsed = blocks("日本語\n\n```rust");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].start, 11);
        assert_eq!(
            parsed[1].kind,
            BlockKind::Code {
                language: "rust",
                body: ""
            }
        );
    }
}
