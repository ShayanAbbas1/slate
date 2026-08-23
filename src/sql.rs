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

use crate::{db::quote_literal, explorer::quote_identifier};

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
        return Some(splice(sql, existing.byte_range(), &clause));
    }

    if clause.is_empty() {
        return Some(sql.to_string());
    }

    let insert_at = child_of_kind(&anchor, "limit")
        .map(|limit| limit.byte_range().start)
        .unwrap_or(anchor.byte_range().end);

    Some(splice(sql, insert_at..insert_at, &clause))
}

/// One row's `UPDATE`: every column in `sets` assigned, every column in `keys`
/// matched.
///
/// Values go in as literals and are never cast. Postgres applies the target
/// column's assignment cast, so `'123'` lands in an `int4` exactly as `123`
/// would, and a cast Slate chose for itself could only ever be the wrong one.
/// A cleared cell is therefore the empty string; writing a NULL is not
/// expressible here.
///
/// `None` when either list is empty. A statement with no `WHERE` rewrites every
/// row in the table and one with no `SET` is not a statement at all, so a caller
/// that has lost the row's key gets nothing to run rather than something that
/// runs.
pub fn update_row(
    schema: &str,
    table: &str,
    sets: &[(&str, &str)],
    keys: &[(&str, &str)],
) -> Option<String> {
    if sets.is_empty() || keys.is_empty() {
        return None;
    }

    Some(format!(
        "UPDATE {}.{} SET {} WHERE {}",
        quote_identifier(schema),
        quote_identifier(table),
        assignments(sets, ", "),
        assignments(keys, " AND ")
    ))
}

/// Whether `sql` is a statement Slate could have written: one or more `UPDATE`s
/// and nothing else at all.
///
/// The one gate every Slate-generated statement passes before anything runs,
/// and the code half of hard rule 1 — Slate never writes a `DROP`, `TRUNCATE`
/// or `DELETE`, whatever the user asked for. A whitelist, because a blocklist
/// of keywords is only a list of the spellings someone thought of.
pub fn is_generated_update(sql: &str) -> bool {
    let Some(tree) = parse(sql) else {
        return false;
    };
    let root = tree.root_node();
    let mut cursor = root.walk();
    let statements: Vec<_> = root.named_children(&mut cursor).collect();

    // Comments are tree-sitter extras and land at the root too, so anything
    // that is not a statement here is something Slate did not generate.
    !statements.is_empty()
        && statements.iter().all(|statement| {
            statement.kind() == "statement"
                && statement
                    .named_child(0)
                    .is_some_and(|node| node.kind() == "update")
        })
        && !destructive(root)
}

fn assignments(columns: &[(&str, &str)], separator: &str) -> String {
    columns
        .iter()
        .map(|(column, value)| format!("{} = {}", quote_identifier(column), quote_literal(value)))
        .collect::<Vec<_>>()
        .join(separator)
}

/// The grammar offers no `drop` or `truncate` node to look for. `DROP TABLE` is
/// `drop_table`, one of thirteen `drop_*` siblings, and `TRUNCATE t` is a bare
/// `statement` holding a `keyword_truncate` with no wrapper node at all. The
/// keyword is the one part every spelling of either has.
const DESTRUCTIVE_KINDS: [&str; 4] = [
    "delete",
    "keyword_delete",
    "keyword_drop",
    "keyword_truncate",
];

/// Anywhere in the tree, not only at the root. `WITH x AS (DELETE FROM t
/// RETURNING *) UPDATE …` is a real statement shape whose root child is an
/// `update` node, so the whitelist alone would let it through.
fn destructive(node: tree_sitter::Node) -> bool {
    let mut cursor = node.walk();
    DESTRUCTIVE_KINDS.contains(&node.kind()) || node.children(&mut cursor).any(destructive)
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
    // A `UNION` puts the whole query's `ORDER BY` after its last branch, so its
    // clauses hang under the set operation rather than the statement.
    if let Some(set_operation) = children
        .iter()
        .find(|node| node.kind() == "set_operation")
    {
        let mut cursor = set_operation.walk();
        let branches: Vec<_> = set_operation.named_children(&mut cursor).collect();
        return select_anchor(&branches);
    }

    select_anchor(&children)
}

/// The `from` of a query, and only of a query.
///
/// The grammar gives `DELETE FROM t` the same `from` child a `SELECT` has, so a
/// `from` alone is not evidence that a sort belongs here — and writing one into
/// a `DELETE` is what hard rule 1 forbids outright. A `select` beside it is the
/// evidence. `WITH` leaves the outer query's `select` and `from` at this level
/// too, beside the cte, so a CTE still sorts.
fn select_anchor<'tree>(children: &[tree_sitter::Node<'tree>]) -> Option<tree_sitter::Node<'tree>> {
    if !children.iter().any(|node| node.kind() == "select") {
        return None;
    }

    children.iter().rfind(|node| node.kind() == "from").copied()
}

fn child_of_kind<'tree>(
    node: &tree_sitter::Node<'tree>,
    kind: &str,
) -> Option<tree_sitter::Node<'tree>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind)
}

/// `sql` with `range` replaced by `clause`, tidying only the seam.
///
/// Only the whitespace either side of the splice point is touched. Collapsing
/// runs of spaces across the whole statement instead would rewrite string
/// literals, quoted identifiers and the user's indentation — a silent edit to
/// what the statement means, which is the one thing this module must not do.
fn splice(sql: &str, range: Range<usize>, clause: &str) -> String {
    let head = sql[..range.start].trim_end();
    let tail = sql[range.end..].trim_start();

    let mut spliced = String::with_capacity(head.len() + clause.len() + tail.len() + 2);
    spliced.push_str(head);
    for part in [clause, tail] {
        if part.is_empty() {
            continue;
        }
        if !spliced.is_empty() {
            spliced.push(' ');
        }
        spliced.push_str(part);
    }
    spliced
}

/// The grammar declares exactly these three as the root's statement children.
/// Filtering on them is not optional: comments are tree-sitter *extras*, so
/// `comment` and `marginalia` also land at the root, and sending one of those
/// to the server returns an empty response the user cannot explain.
const STATEMENT_KINDS: [&str; 3] = ["statement", "block", "transaction"];

fn collect_statements(tree: &Tree, sql: &str) -> Vec<Range<usize>> {
    let root = tree.root_node();
    let mut cursor = root.walk();
    let mut statements: Vec<Range<usize>> = Vec::new();

    for node in root.named_children(&mut cursor) {
        // Whatever the grammar could not read lands in a sibling ERROR node.
        // Dropping it would send the statement's head alone, and the head of a
        // half-typed `DELETE … WHERE` is an unqualified DELETE. The tail was
        // typed into this statement, so it goes to the server with it and the
        // server is what explains the problem.
        //
        // Backwards only, deliberately. An ERROR *before* a statement means the
        // statement's opening keyword is the part that did not parse, so what
        // is left is a fragment the server rejects rather than a statement that
        // runs and means something else — `GRANT SELECT ON t TO r` sends
        // `SELECT ON t TO r`. Merging that one forward would attach a typo on
        // the first line to the perfectly good statement underneath it.
        if node.is_error() {
            if let Some(last) = statements.last_mut()
                && let Some(merged) = trim_range(sql, last.start..node.byte_range().end)
            {
                *last = merged;
            }
            continue;
        }

        if !STATEMENT_KINDS.contains(&node.kind()) {
            continue;
        }

        if let Some(range) = trim_range(sql, node.byte_range()) {
            statements.push(range);
        }
    }

    statements
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
        // dangling `FROM` is unparsable, so it stays with the statement it was
        // typed into and the server explains the problem.
        let sql = "SELECT * FROM";
        let buffer = Buffer::parse(sql);

        assert!(buffer.statement_at(3).is_some());
        assert_eq!(&sql[buffer.statement_at(sql.len()).unwrap()], "SELECT * FROM");
    }

    #[test]
    fn an_unparsable_tail_stays_with_the_statement_it_was_typed_into() {
        // The head of a half-typed `DELETE ... WHERE` is an unqualified
        // DELETE. Sending it because the grammar could not read the tail is
        // the worst thing this module could do, so the tail comes along and
        // the server is what rejects it.
        for sql in [
            "DELETE FROM t WHERE ",
            "UPDATE t SET a = 1 WHERE ",
            "DELETE FROM t WHERE a = 'x",
            "SELECT 1;\nDELETE FROM t WHERE ",
            "GRANT SELECT ON t TO r",
            "SELECT 1 LIMIT 1",
        ] {
            let buffer = Buffer::parse(sql);
            let run = &sql[buffer.statement_at(sql.len()).unwrap()];
            assert!(
                sql.trim_end().ends_with(run),
                "{sql:?} was truncated to {run:?}"
            );
        }
    }

    #[test]
    fn a_statement_that_is_not_a_query_takes_no_sort() {
        // A header click asks Slate to write an ORDER BY. Hard rule 1 says it
        // never writes a destructive statement, and the grammar gives `DELETE`
        // the same `from` child a `SELECT` has -- so the guard is the presence
        // of a `select`, not of a `from`.
        for sql in [
            "DELETE FROM t WHERE a = 1",
            "DELETE FROM t WHERE a = 1 RETURNING *",
            "UPDATE t SET a = 1",
            "TRUNCATE t",
            "INSERT INTO t (a) VALUES (1) RETURNING *",
        ] {
            assert!(order_by(sql).is_none(), "{sql} reported a sort");
            assert!(
                with_order_by(sql, &[SortKey::new("a", true)]).is_none(),
                "{sql} was spliced"
            );
        }
    }

    #[test]
    fn a_splice_changes_nothing_but_the_clause() {
        // Collapsing whitespace across the whole statement rewrites string
        // literals, quoted identifiers and indentation -- all of which change
        // what the statement means or how it reads.
        assert_eq!(
            with_order_by(
                "SELECT * FROM t WHERE note LIKE 'a  %' LIMIT 10",
                &[SortKey::new("id", true)]
            )
            .unwrap(),
            "SELECT * FROM t WHERE note LIKE 'a  %' ORDER BY id ASC LIMIT 10"
        );
        assert_eq!(
            with_order_by(
                r#"SELECT * FROM "public"."my  table""#,
                &[SortKey::new("id", true)]
            )
            .unwrap(),
            r#"SELECT * FROM "public"."my  table" ORDER BY id ASC"#
        );
        assert_eq!(
            with_order_by(
                "SELECT *\nFROM t\nWHERE a = 1\n  AND b = 2",
                &[SortKey::new("id", true)]
            )
            .unwrap(),
            "SELECT *\nFROM t\nWHERE a = 1\n  AND b = 2 ORDER BY id ASC"
        );
    }

    #[test]
    fn a_generated_update_sets_every_column_it_was_given() {
        // One column and several. A missing separator between assignments is a
        // statement the server rejects; a missing one in the WHERE would be a
        // statement it accepts and applies to the wrong rows.
        assert_eq!(
            update_row("public", "measurements", &[("note", "ok")], &[("id", "7")]).unwrap(),
            r#"UPDATE "public"."measurements" SET "note" = 'ok' WHERE "id" = '7'"#
        );
        assert_eq!(
            update_row(
                "public",
                "measurements",
                &[("note", "ok"), ("depth", "12")],
                &[("id", "7")]
            )
            .unwrap(),
            r#"UPDATE "public"."measurements" SET "note" = 'ok', "depth" = '12' WHERE "id" = '7'"#
        );
    }

    #[test]
    fn a_composite_key_matches_on_all_of_its_columns() {
        // Joined by OR, or with a column dropped, this updates rows the user
        // never edited.
        assert_eq!(
            update_row(
                "app",
                "memberships",
                &[("role", "owner")],
                &[("org_id", "1"), ("user_id", "2")]
            )
            .unwrap(),
            r#"UPDATE "app"."memberships" SET "role" = 'owner' WHERE "org_id" = '1' AND "user_id" = '2'"#
        );
    }

    #[test]
    fn user_data_is_quoted_rather_than_interpolated() {
        // An apostrophe in a value and a double quote in a column name are the
        // two ways a cell's contents become SQL of its own.
        assert_eq!(
            update_row("s", "t", &[("a", "it's")], &[("id", "o'hara")]).unwrap(),
            r#"UPDATE "s"."t" SET "a" = 'it''s' WHERE "id" = 'o''hara'"#
        );
        assert_eq!(
            update_row("s", r#"od"d"#, &[(r#"we"ird"#, "x")], &[("id", "1")]).unwrap(),
            r#"UPDATE "s"."od""d" SET "we""ird" = 'x' WHERE "id" = '1'"#
        );
    }

    #[test]
    fn an_update_with_nothing_to_match_on_is_refused() {
        // No WHERE rewrites every row in the table. It must not be possible to
        // produce that statement, so a caller with no key gets nothing.
        assert!(update_row("s", "t", &[("a", "1")], &[]).is_none());
        assert!(update_row("s", "t", &[], &[("id", "1")]).is_none());
    }

    #[test]
    fn the_gate_accepts_an_update_and_a_batch_of_updates() {
        assert!(is_generated_update("UPDATE t SET a = '1' WHERE id = '2'"));
        assert!(is_generated_update(
            "UPDATE t SET a = '1' WHERE id = '2'; UPDATE t SET a = '3' WHERE id = '4'"
        ));
    }

    #[test]
    fn the_gate_refuses_everything_that_is_not_an_update() {
        // Hard rule 1 in code: DROP, TRUNCATE and DELETE never leave Slate,
        // whatever the user asked for. SELECT and INSERT are here because the
        // gate is a whitelist -- being harmless is not the test, being an
        // UPDATE is.
        for sql in [
            "DROP TABLE t",
            "DROP VIEW v",
            "DROP DATABASE d",
            "TRUNCATE t",
            "TRUNCATE TABLE t",
            "DELETE FROM t WHERE a = '1'",
            "SELECT 1",
            "INSERT INTO t (a) VALUES ('1')",
        ] {
            assert!(!is_generated_update(sql), "{sql} passed the gate");
        }
    }

    #[test]
    fn the_gate_refuses_a_batch_with_one_destructive_statement_in_it() {
        // Every statement is checked, not the first one. A DELETE appended to a
        // run of legitimate updates is the shape an injected value would take.
        assert!(!is_generated_update(
            "UPDATE t SET a = '1' WHERE id = '2'; DELETE FROM t; UPDATE t SET a = '3' WHERE id = '4'"
        ));
    }

    #[test]
    fn the_gate_refuses_a_destructive_statement_wrapped_in_a_cte() {
        // The root statement's first child here really is an `update` node, so
        // the whitelist passes it and only the subtree scan catches it.
        assert!(!is_generated_update(
            "WITH x AS (DELETE FROM t RETURNING *) UPDATE u SET a = '1' WHERE id = '2'"
        ));
    }

    #[test]
    fn the_gate_refuses_what_the_grammar_cannot_read_whole() {
        // An unreadable tree says nothing about what the statement does, and a
        // gate that cannot see has to refuse. The empty buffer is here because
        // it parses cleanly into no statements at all.
        for sql in [
            "not sql at all !!",
            "UPDATE t SET a = ",
            "-- UPDATE t SET a = '1'",
            "",
        ] {
            assert!(!is_generated_update(sql), "{sql:?} passed the gate");
        }
    }

    #[test]
    fn the_gate_accepts_what_update_row_writes() {
        // The one test that keeps the generator and the gate from drifting
        // apart: whatever quoting or clause order changes here, the statement
        // Slate builds is still one the gate can read as an UPDATE.
        let statement = update_row(
            "public",
            "measurements",
            &[("note", "it's fine"), ("depth", "12")],
            &[("id", "7"), ("run", "a'b")],
        )
        .unwrap();

        assert!(is_generated_update(&statement), "{statement} was refused");
        assert!(is_generated_update(&format!("{statement}; {statement}")));
    }

    #[test]
    fn a_cte_still_takes_a_sort() {
        // `WITH` puts the outer SELECT and its FROM at the top level, beside
        // the cte. The select-child guard must not read the cte's own.
        assert_eq!(
            with_order_by(
                "WITH x AS (SELECT 1 AS a) SELECT * FROM x",
                &[SortKey::new("a", true)]
            )
            .unwrap(),
            "WITH x AS (SELECT 1 AS a) SELECT * FROM x ORDER BY a ASC"
        );
    }
}
