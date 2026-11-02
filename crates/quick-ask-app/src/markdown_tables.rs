use markdown::mdast::AlignKind;

#[derive(Debug)]
pub struct Table<'a> {
    pub start: usize,
    pub end: usize,
    pub align: Vec<AlignKind>,
    pub rows: Vec<Vec<&'a str>>,
}

pub fn tables(source: &str) -> Vec<Table<'_>> {
    use markdown::{ParseOptions, mdast::Node};

    if !source.contains('|') {
        return vec![];
    }
    let Ok(Node::Root(root)) = markdown::to_mdast(source, &ParseOptions::gfm()) else {
        return vec![];
    };
    root.children
        .into_iter()
        .filter_map(|node| {
            let Node::Table(table) = node else {
                return None;
            };
            let position = table.position?;
            let rows = table
                .children
                .iter()
                .filter_map(|node| {
                    let Node::TableRow(row) = node else {
                        return None;
                    };
                    Some(
                        row.children
                            .iter()
                            .map(|node| {
                                let Some(children) = node.children() else {
                                    return "";
                                };
                                // Slice inline nodes, not the cell's delimiters. This preserves
                                // emphasis, links, escaped pipes and Unicode byte offsets.
                                children
                                    .first()
                                    .and_then(Node::position)
                                    .zip(children.last().and_then(Node::position))
                                    .map(|(first, last)| {
                                        &source[first.start.offset..last.end.offset]
                                    })
                                    .unwrap_or("")
                            })
                            .collect(),
                    )
                })
                .collect();
            Some(Table {
                start: position.start.offset,
                end: position.end.offset,
                align: table.align,
                rows,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_table_cells_without_losing_formatting_or_surrounding_text() {
        let source = "Before.\n\n| Accessory | Why |\n| --- | ---: |\n| **Smart bulbs** | A long description with [a link](https://example.com). |\n| 日本語 | a \\| b |\n\nAfter.";
        let parsed = tables(source);
        assert_eq!(parsed.len(), 1);
        let table = &parsed[0];
        assert_eq!(&source[..table.start], "Before.\n\n");
        assert_eq!(&source[table.end..], "\n\nAfter.");
        assert_eq!(table.align, [AlignKind::None, AlignKind::Right]);
        assert_eq!(
            table.rows,
            vec![
                vec!["Accessory", "Why"],
                vec![
                    "**Smart bulbs**",
                    "A long description with [a link](https://example.com)."
                ],
                vec!["日本語", "a \\| b"],
            ]
        );
    }

    #[test]
    fn ignores_pipe_text_in_code_and_handles_empty_and_streaming_cells() {
        assert!(tables("```\n| A | B |\n| --- | --- |\n```").is_empty());
        let parsed = tables("| A | B |\n| --- | --- |\n| | growing");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].rows, vec![vec!["A", "B"], vec!["", "growing"]]);
    }
}
