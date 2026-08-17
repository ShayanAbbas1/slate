//! Finding statement boundaries in a query buffer.
//!
//! `cmd+enter` runs the statement under the cursor, so we need to know where
//! each statement starts and ends. Splitting on `;` is wrong — a semicolon can
//! sit inside a string literal, a line comment, or a `$$`-quoted function body,
//! and each of those would be cut in the wrong place. We run a real parser.
//!
//! gpui-component highlights with tree-sitter internally but keeps the tree
//! private, so this is a second, independent parse of the same text. For a
//! query buffer that cost is irrelevant.
//!
//! Statement ranges **exclude the terminating semicolon** — that is where the
//! grammar puts the node boundary, and it is what we want, since Postgres does
//! not need a trailing semicolon on a statement sent over the wire.

use std::ops::Range;

use tree_sitter::{Parser, Tree};

/// A parsed query buffer. Holds the tree so repeated cursor moves don't reparse.
pub struct Buffer {
    tree: Option<Tree>,
    statements: Vec<Range<usize>>,
}

impl Buffer {
    pub fn parse(sql: &str) -> Self {
        let mut parser = Parser::new();
        let tree = parser
            .set_language(&tree_sitter_sequel::LANGUAGE.into())
            .ok()
            .and_then(|_| parser.parse(sql, None));

        let statements = tree
            .as_ref()
            .map(|tree| collect_statements(tree, sql))
            .unwrap_or_default();

        Self { tree, statements }
    }

    /// Byte ranges of each statement, in source order, trimmed of surrounding
    /// whitespace. Empty if the buffer holds no statements.
    pub fn statements(&self) -> &[Range<usize>] {
        &self.statements
    }

    pub fn parsed(&self) -> bool {
        self.tree.is_some()
    }

    /// The statement to run for a cursor at `offset`.
    ///
    /// Inside a statement, that statement. In whitespace or a comment between
    /// two statements, the preceding one — you just finished typing it. Before
    /// the first statement, the first one.
    pub fn statement_at(&self, offset: usize) -> Option<Range<usize>> {
        if self.statements.is_empty() {
            return None;
        }

        if let Some(hit) = self
            .statements
            .iter()
            .find(|range| range.contains(&offset) || range.end == offset)
        {
            return Some(hit.clone());
        }

        self.statements
            .iter()
            .rev()
            .find(|range| range.end < offset)
            .or_else(|| self.statements.first())
            .cloned()
    }
}

/// Top-level named children of the root are statements. Taking them from the
/// tree rather than matching node kind names keeps this working across grammar
/// revisions, which rename node kinds more often than they restructure the root.
fn collect_statements(tree: &Tree, sql: &str) -> Vec<Range<usize>> {
    let root = tree.root_node();
    let mut cursor = root.walk();

    root.named_children(&mut cursor)
        .filter_map(|node| trim_range(sql, node.byte_range()))
        .collect()
}

fn trim_range(sql: &str, range: Range<usize>) -> Option<Range<usize>> {
    let slice = sql.get(range.clone())?;
    let leading = slice.len() - slice.trim_start().len();
    let trailing = slice.len() - slice.trim_end().len();
    let trimmed = (range.start + leading)..(range.end - trailing);
    (!trimmed.is_empty()).then_some(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str) -> Vec<&str> {
        Buffer::parse(sql)
            .statements()
            .iter()
            .map(|r| &sql[r.clone()])
            .collect()
    }

    #[test]
    fn grammar_loads() {
        assert!(
            Buffer::parse("SELECT 1;").parsed(),
            "SQL grammar failed to load"
        );
    }

    #[test]
    fn splits_simple_statements() {
        assert_eq!(texts("SELECT 1;\nSELECT 2;"), vec!["SELECT 1", "SELECT 2"]);
    }

    #[test]
    fn unterminated_final_statement_is_still_found() {
        assert_eq!(texts("SELECT 1;\nSELECT 2"), vec!["SELECT 1", "SELECT 2"]);
    }

    #[test]
    fn semicolon_inside_a_string_literal_does_not_split() {
        // The case that breaks every naive splitter.
        assert_eq!(
            texts("SELECT ';' AS sep;\nSELECT 2;"),
            vec!["SELECT ';' AS sep", "SELECT 2"]
        );
    }

    #[test]
    fn dollar_quoted_body_does_not_split() {
        // Two semicolons live inside the function body. A `;` split would
        // produce four fragments, none of them runnable.
        let sql = "CREATE FUNCTION f() RETURNS int AS $$\n\
                   BEGIN\n\
                   RETURN 1;\n\
                   END;\n\
                   $$ LANGUAGE plpgsql;\n\
                   SELECT 1;";
        let found = texts(sql);
        assert_eq!(found.len(), 2, "body was split: {found:#?}");
        assert!(found[0].contains("RETURN 1;"));
        assert!(found[0].contains("END;"));
        assert_eq!(found[1], "SELECT 1");
    }

    #[test]
    fn cursor_inside_a_statement_selects_it() {
        let sql = "SELECT 1;\nSELECT 2;";
        let buffer = Buffer::parse(sql);
        let inside_second = sql.find("SELECT 2").unwrap() + 3;
        assert_eq!(&sql[buffer.statement_at(inside_second).unwrap()], "SELECT 2");
    }

    #[test]
    fn cursor_just_after_a_semicolon_selects_that_statement() {
        let sql = "SELECT 1;\nSELECT 2;";
        let buffer = Buffer::parse(sql);
        let after_first = sql.find(';').unwrap() + 1;
        assert_eq!(&sql[buffer.statement_at(after_first).unwrap()], "SELECT 1");
    }

    #[test]
    fn cursor_in_the_gap_selects_the_preceding_statement() {
        let sql = "SELECT 1;\n\n\nSELECT 2;";
        let buffer = Buffer::parse(sql);
        let gap = sql.find("\n\n").unwrap() + 2;
        assert_eq!(&sql[buffer.statement_at(gap).unwrap()], "SELECT 1");
    }

    #[test]
    fn empty_and_whitespace_buffers_yield_nothing() {
        assert!(Buffer::parse("").statements().is_empty());
        assert!(Buffer::parse("   \n\t ").statements().is_empty());
        assert!(Buffer::parse("").statement_at(0).is_none());
    }

    #[test]
    fn incomplete_input_still_reports_something_runnable() {
        // Half-typed queries must not panic or wipe the statement list.
        let buffer = Buffer::parse("SELECT * FROM");
        assert!(buffer.parsed());
        assert!(buffer.statement_at(3).is_some());
    }
}
