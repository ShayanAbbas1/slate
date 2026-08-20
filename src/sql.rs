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

/// The runnable statements of a query buffer, as byte ranges into it.
pub struct Buffer {
    statements: Vec<Range<usize>>,
}

impl Buffer {
    pub fn parse(sql: &str) -> Self {
        let mut parser = Parser::new();
        let statements = parser
            .set_language(&tree_sitter_sequel::LANGUAGE.into())
            .ok()
            .and_then(|_| parser.parse(sql, None))
            .map(|tree| collect_statements(&tree, sql))
            .unwrap_or_default();

        Self { statements }
    }

    /// Byte ranges of each statement, in source order, trimmed of surrounding
    /// whitespace. Empty if the buffer holds no statements.
    #[cfg(test)]
    pub fn statements(&self) -> &[Range<usize>] {
        &self.statements
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

/// One key of an `ORDER BY`, as Slate reads and writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SortKey {
    /// The key exactly as it appears in the statement — `"created_at"`, `3`,
    /// `lower(name)`. Kept verbatim, so a key Slate did not write survives a
    /// click on some other column.
    pub expression: String,
    pub ascending: bool,
}

impl SortKey {
    pub fn new(expression: impl Into<String>, ascending: bool) -> Self {
        Self {
            expression: expression.into(),
            ascending,
        }
    }

    fn render(&self) -> String {
        let direction = match self.ascending {
            true => "ASC",
            false => "DESC",
        };
        format!("{} {direction}", self.expression)
    }
}

/// The keys of a statement's `ORDER BY`, in order. `Some(empty)` is a statement
/// that could carry one and does not; `None` is a statement Slate cannot read
/// well enough to say without guessing.
pub fn order_by(statement: &str) -> Option<Vec<SortKey>> {
    let sql = statement;
    let tree = parse(sql)?;
    let anchor = clause_anchor(&tree, sql)?;
    let Some(clause) = child_of_kind(&anchor, "order_by") else {
        return Some(Vec::new());
    };

    let mut cursor = clause.walk();
    let keys = clause
        .named_children(&mut cursor)
        .filter(|node| node.kind() == "order_target")
        .filter_map(|target| {
            let mut cursor = target.walk();
            let children: Vec<_> = target.named_children(&mut cursor).collect();
            let expression = children
                .iter()
                .find(|node| node.kind() != "direction")
                .and_then(|node| sql.get(node.byte_range()))?;
            let descending = children
                .iter()
                .find(|node| node.kind() == "direction")
                .and_then(|node| sql.get(node.byte_range()))
                .is_some_and(|text| text.trim().eq_ignore_ascii_case("desc"));

            Some(SortKey::new(expression, !descending))
        })
        .collect();

    Some(keys)
}

/// `statement` with `keys` as its `ORDER BY`, replacing the clause it already
/// has and removing it when `keys` is empty.
///
/// The clause is placed where it belongs rather than appended: `ORDER BY` after
/// a `LIMIT` is a syntax error, and a limit that applies *before* the sort
/// would order one arbitrary page of the table instead of the table.
///
/// `None` when Slate cannot see where the clause goes — a statement it cannot
/// parse cleanly, one with no `FROM`, or one that is not a query. Nothing is
/// guessed at, because the alternative is handing the server a statement the
/// user did not write and cannot read.
pub fn with_order_by(statement: &str, keys: &[SortKey]) -> Option<String> {
    let sql = statement;
    let tree = parse(sql)?;
    let anchor = clause_anchor(&tree, sql)?;
    let clause = match keys.is_empty() {
        true => String::new(),
        false => format!(
            "ORDER BY {}",
            keys.iter()
                .map(SortKey::render)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };

    // Replacing the existing clause, rather than adding a second one, is what
    // makes a repeated click a change of sort instead of an accumulation.
    if let Some(existing) = child_of_kind(&anchor, "order_by") {
        let range = existing.byte_range();
        let mut spliced = String::with_capacity(sql.len() + clause.len());
        spliced.push_str(&sql[..range.start]);
        spliced.push_str(&clause);
        spliced.push_str(&sql[range.end..]);
        return Some(collapse_gap(&spliced));
    }

    if clause.is_empty() {
        return Some(sql.to_string());
    }

    let insert_at = child_of_kind(&anchor, "limit")
        .map(|limit| limit.byte_range().start)
        .unwrap_or(anchor.byte_range().end);
    let (head, tail) = sql.split_at(insert_at);

    Some(collapse_gap(&format!("{head} {clause} {tail}")))
}

fn parse(sql: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_sequel::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(sql, None)?;
    // A statement the grammar could not read whole is a statement whose clause
    // boundaries are unknown, and splicing against a guess would corrupt SQL
    // the user wrote. `NULLS FIRST` and `FOR UPDATE` land here today.
    (!tree.root_node().has_error()).then_some(tree)
}

/// The node whose children carry `ORDER BY` and `LIMIT`: the query's outermost
/// `FROM`. A subquery's own clauses hang under its `subquery` node instead, so
/// looking only at this node's children cannot reach into one by accident.
fn clause_anchor<'tree>(
    tree: &'tree Tree,
    sql: &str,
) -> Option<tree_sitter::Node<'tree>> {
    let root = tree.root_node();
    let mut cursor = root.walk();
    let statements: Vec<_> = root
        .named_children(&mut cursor)
        .filter(|node| STATEMENT_KINDS.contains(&node.kind()))
        .collect();
    // One statement, or there is no telling which one the rows came from.
    let [statement] = statements[..] else {
        return None;
    };
    // `BEGIN; …; COMMIT` and `DO $$…$$` are runnable but not queries.
    if statement.kind() != "statement" || sql.get(statement.byte_range()).is_none() {
        return None;
    }

    let mut cursor = statement.walk();
    let children: Vec<_> = statement.named_children(&mut cursor).collect();
    // A `UNION` puts the whole query's `ORDER BY` after its last branch.
    if let Some(set_operation) = children
        .iter()
        .find(|node| node.kind() == "set_operation")
    {
        let mut cursor = set_operation.walk();
        return set_operation
            .named_children(&mut cursor)
            .filter(|node| node.kind() == "from")
            .last();
    }

    children
        .into_iter()
        .find(|node| node.kind() == "from")
}

fn child_of_kind<'tree>(
    node: &tree_sitter::Node<'tree>,
    kind: &str,
) -> Option<tree_sitter::Node<'tree>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind)
}

/// A removed clause leaves the spaces that surrounded it behind.
fn collapse_gap(sql: &str) -> String {
    let mut collapsed = String::with_capacity(sql.len());
    let mut spaces = 0;
    for character in sql.chars() {
        match character {
            ' ' => spaces += 1,
            _ => spaces = 0,
        }
        if spaces < 2 {
            collapsed.push(character);
        }
    }
    collapsed.trim_end().to_string()
}

/// The grammar declares exactly these three as the root's statement children.
/// Filtering on them is not optional: comments are tree-sitter *extras*, so
/// `comment`, `marginalia` and `ERROR` also land at the root, and sending one
/// of those to the server returns an empty response the user cannot explain.
const STATEMENT_KINDS: [&str; 3] = ["statement", "block", "transaction"];

fn collect_statements(tree: &Tree, sql: &str) -> Vec<Range<usize>> {
    let root = tree.root_node();
    let mut cursor = root.walk();

    root.named_children(&mut cursor)
        .filter(|node| STATEMENT_KINDS.contains(&node.kind()))
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
    fn a_sort_goes_in_before_the_limit() {
        // Appended after the limit it would not parse; applied after the limit
        // it would sort one arbitrary thousand rows of the table.
        assert_eq!(
            with_order_by(
                r#"SELECT * FROM "public"."measurements" LIMIT 1000"#,
                &[SortKey::new(r#""id""#, false)]
            )
            .unwrap(),
            r#"SELECT * FROM "public"."measurements" ORDER BY "id" DESC LIMIT 1000"#
        );
    }

    #[test]
    fn a_second_key_joins_the_first() {
        let sorted = with_order_by(
            "SELECT * FROM t",
            &[SortKey::new(r#""a""#, true), SortKey::new("3", false)],
        )
        .unwrap();

        assert_eq!(sorted, r#"SELECT * FROM t ORDER BY "a" ASC, 3 DESC"#);
        assert_eq!(
            order_by(&sorted).unwrap(),
            vec![SortKey::new(r#""a""#, true), SortKey::new("3", false)]
        );
    }

    #[test]
    fn sorting_again_replaces_the_clause_it_wrote() {
        let once = with_order_by("SELECT * FROM t LIMIT 5", &[SortKey::new("a", true)]).unwrap();
        let twice = with_order_by(&once, &[SortKey::new("b", false)]).unwrap();

        assert_eq!(twice, "SELECT * FROM t ORDER BY b DESC LIMIT 5");
        // And clearing it leaves the statement as it was, not a hole.
        assert_eq!(
            with_order_by(&twice, &[]).unwrap(),
            "SELECT * FROM t LIMIT 5"
        );
    }

    #[test]
    fn a_key_the_user_wrote_reads_back_verbatim() {
        let keys = order_by("SELECT * FROM t ORDER BY lower(name), 2 DESC").unwrap();

        assert_eq!(
            keys,
            vec![SortKey::new("lower(name)", true), SortKey::new("2", false)]
        );
    }

    #[test]
    fn a_union_sorts_at_the_end_of_the_whole_query() {
        assert_eq!(
            with_order_by(
                "SELECT a FROM t UNION SELECT a FROM u LIMIT 3",
                &[SortKey::new("a", true)]
            )
            .unwrap(),
            "SELECT a FROM t UNION SELECT a FROM u ORDER BY a ASC LIMIT 3"
        );
    }

    #[test]
    fn a_subquerys_own_sort_is_left_alone() {
        // The inner ORDER BY belongs to the subquery. Reading it as the outer
        // query's sort would flip a clause the user wrote for another purpose.
        let sql = "SELECT * FROM (SELECT a FROM t ORDER BY a LIMIT 3) s";

        assert_eq!(order_by(sql).unwrap(), vec![]);
        assert_eq!(
            with_order_by(sql, &[SortKey::new("a", false)]).unwrap(),
            "SELECT * FROM (SELECT a FROM t ORDER BY a LIMIT 3) s ORDER BY a DESC"
        );
    }

    #[test]
    fn nothing_is_spliced_into_a_statement_slate_cannot_read_whole() {
        // Every one of these is valid SQL the grammar does not cover. Guessing
        // where the clause goes would corrupt a statement the user wrote.
        for sql in [
            "SELECT * FROM t ORDER BY a NULLS FIRST",
            "SELECT * FROM t FOR UPDATE",
            "SELECT * FROM t OFFSET 10 LIMIT 5",
        ] {
            assert!(order_by(sql).is_none(), "{sql} should not be sortable");
            assert!(with_order_by(sql, &[]).is_none(), "{sql} was spliced");
        }
    }

    #[test]
    fn only_a_query_takes_a_sort() {
        for sql in [
            "UPDATE t SET a = 1",
            "SELECT 1",
            "SELECT 1; SELECT 2",
            "BEGIN; SELECT 1; COMMIT",
            "-- nothing here",
        ] {
            assert!(order_by(sql).is_none(), "{sql} should not be sortable");
        }
    }

    #[test]
    fn grammar_loads() {
        assert_eq!(
            texts("SELECT 1;"),
            vec!["SELECT 1"],
            "SQL grammar failed to load"
        );
    }

    #[test]
    fn a_leading_comment_is_not_a_runnable_statement() {
        // Cursor at 0 in a buffer that opens with a header comment. Running the
        // comment returns an empty response with no error to explain it.
        let sql = "-- notes; about this\nSELECT 1;";
        let buffer = Buffer::parse(sql);

        assert_eq!(&sql[buffer.statement_at(0).unwrap()], "SELECT 1");
    }

    #[test]
    fn a_trailing_comment_is_not_a_runnable_statement() {
        let sql = "SELECT 1; -- trailing";
        let buffer = Buffer::parse(sql);

        assert_eq!(&sql[buffer.statement_at(sql.len()).unwrap()], "SELECT 1");
    }

    #[test]
    fn a_cursor_inside_a_gap_comment_selects_the_preceding_statement() {
        // The contract statement_at documents, which the block comment used to
        // win against by matching its own range.
        let sql = "SELECT 1;\n/* gap comment */\nSELECT 2;";
        let buffer = Buffer::parse(sql);
        let inside = sql.find("gap").unwrap();

        assert_eq!(&sql[buffer.statement_at(inside).unwrap()], "SELECT 1");
    }

    #[test]
    fn a_buffer_of_only_comments_has_nothing_to_run() {
        assert!(Buffer::parse("-- only a comment").statement_at(0).is_none());
        assert!(Buffer::parse(";;;").statement_at(0).is_none());
    }

    #[test]
    fn a_transaction_block_runs_as_one_statement() {
        assert_eq!(
            texts("BEGIN; SELECT 1; COMMIT;"),
            vec!["BEGIN; SELECT 1; COMMIT"]
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
        // Half-typed queries must not panic or wipe the statement list. The
        // dangling `FROM` parses as an ERROR node and is dropped, so the cursor
        // at the end runs `SELECT *` and the server explains the problem --
        // better than shipping `FROM` on its own.
        let sql = "SELECT * FROM";
        let buffer = Buffer::parse(sql);

        assert!(buffer.statement_at(3).is_some());
        assert_eq!(&sql[buffer.statement_at(sql.len()).unwrap()], "SELECT *");
    }
}
