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

use tree_sitter::{Node, Parser, Tree};

use crate::db::Engine;

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
/// would, and SQLite applies the column's type affinity to the same effect. A
/// cast Slate chose for itself could only ever be the wrong one. A cleared cell
/// is therefore the empty string, and a `None` in `sets` is a `NULL`: the two
/// are different writes, which is the whole point of spelling one of them as an
/// absence.
///
/// `keys` carries no `None`, because a row identified by a `NULL` is a row `=`
/// does not find; the caller drops such a row before it gets here.
///
/// `None` when either list is empty. A statement with no `WHERE` rewrites every
/// row in the table and one with no `SET` is not a statement at all, so a caller
/// that has lost the row's key gets nothing to run rather than something that
/// runs.
pub fn update_row(
    engine: Engine,
    schema: &str,
    table: &str,
    sets: &[(&str, Option<&str>)],
    keys: &[(&str, &str)],
) -> Option<String> {
    if sets.is_empty() || keys.is_empty() {
        return None;
    }

    let keys: Vec<(&str, Option<&str>)> = keys
        .iter()
        .map(|&(column, value)| (column, Some(value)))
        .collect();
    Some(format!(
        "UPDATE {} SET {} WHERE {}",
        engine.qualified(schema, table),
        assignments(engine, sets, ", "),
        assignments(engine, &keys, " AND ")
    ))
}

/// An `INSERT` naming exactly the columns it was given, and no others.
///
/// A column the caller does not pass is not mentioned in the statement at all,
/// which is what leaves the server's default to apply to it. That is the whole
/// reason this takes a list of columns rather than a row: a row would have a
/// value for every column, and every default would be unreachable.
///
/// The asymmetry with `update_row` is deliberate and worth stating: this needs
/// a schema and a table but **no primary key**, because an insert has no
/// existing row to name yet, where editing has to name a row that already
/// exists. So a table without a primary key can be inserted into and not
/// edited.
///
/// `None` on an empty list. The alternative is `INSERT INTO t DEFAULT VALUES`,
/// a statement nobody has asked Slate for.
pub fn insert_row(
    engine: Engine,
    schema: &str,
    table: &str,
    columns: &[(&str, Option<&str>)],
) -> Option<String> {
    if columns.is_empty() {
        return None;
    }

    let names: Vec<String> = columns
        .iter()
        .map(|&(column, _)| engine.quote_identifier(column))
        .collect();
    let values: Vec<String> = columns
        .iter()
        .map(|&(_, value)| literal(engine, value))
        .collect();
    Some(format!(
        "INSERT INTO {} ({}) VALUES ({})",
        engine.qualified(schema, table),
        names.join(", "),
        values.join(", ")
    ))
}

/// One row's `DELETE`: every column in `keys` matched, and nothing else.
///
/// One row per statement. Multi-row deletion is cut, and the upgrade path when
/// it is wanted is the `BEGIN`/`COMMIT` bracketing multi-row edits already use
/// on the engines that commit each statement alone — one `DELETE` per row, each
/// naming its own key, never one statement with a predicate covering several.
///
/// `None` on an empty key list. A `DELETE` with no `WHERE` empties the table, so
/// it must not be possible to produce one: a caller that has lost the row's key
/// gets nothing to run rather than something that runs.
pub fn delete_row(
    engine: Engine,
    schema: &str,
    table: &str,
    keys: &[(&str, &str)],
) -> Option<String> {
    if keys.is_empty() {
        return None;
    }

    let keys: Vec<(&str, Option<&str>)> = keys
        .iter()
        .map(|&(column, value)| (column, Some(value)))
        .collect();
    Some(format!(
        "DELETE FROM {} WHERE {}",
        engine.qualified(schema, table),
        assignments(engine, &keys, " AND ")
    ))
}

/// Whether `sql` is a statement Slate could have written: one or more `UPDATE`s,
/// a single `INSERT`, or a single `DELETE` naming one row, and nothing else at
/// all.
///
/// The one gate every Slate-generated statement passes before anything runs,
/// and the code half of hard rule 1 — Slate never writes a `DROP` or a
/// `TRUNCATE`, whatever the user asked for, and writes a `DELETE` only as a
/// conjunction of equalities over distinct, unqualified columns. A whitelist,
/// because a blocklist of keywords is only a list of the spellings someone
/// thought of.
///
/// The delete's shape is read out of the parse tree rather than trusted because
/// `delete_row` produced it. A gate that trusts its caller is a comment, and the
/// day the generator and the check disagree is the day this earns its keep.
/// Whether the columns it names are the row's *key* is `delete_matches_key`'s
/// answer, which this cannot give: no key reaches here to compare against.
///
/// Named for what it admits rather than for one of the shapes, because it
/// admits more than one now: a rule that lets an `INSERT` through under a name
/// promising an `UPDATE` is how a whitelist quietly becomes a list of things
/// nobody refused.
pub fn is_generated_write(sql: &str) -> bool {
    let Some(tree) = parse(sql) else {
        return false;
    };
    let root = tree.root_node();
    // Before any shape is considered, because no shape redeems either.
    if forbidden(root) {
        return false;
    }
    let Some(statements) = generated_statements(&root) else {
        return false;
    };

    // Comments are tree-sitter extras and land at the root too, so anything
    // that is not a statement here is something Slate did not generate.
    let kinds: Vec<&str> = statements
        .iter()
        .map(|statement| match statement.kind() == "statement" {
            true => statement.named_child(0).map_or("", |node| node.kind()),
            false => "",
        })
        .collect();

    // The one place a `delete` node is tolerated, and only for the shape read
    // back out of the tree rather than trusted because Slate wrote it.
    if kinds == ["delete"] {
        return delete_key_columns(sql).is_some();
    }

    // One insert alone, or a batch of updates. A batch of inserts is a shape
    // nothing generates, so admitting it would widen the gate for nobody.
    (kinds == ["insert"] || (!kinds.is_empty() && kinds.iter().all(|kind| *kind == "update")))
        && !deletes_anything(root)
}

/// Whether `sql` is a `DELETE` whose `WHERE` names exactly `keys` — nothing
/// absent from the key, and nothing in the key absent from the predicate.
///
/// The half of the delete admission `is_generated_write` cannot make alone: it
/// has no key to compare a predicate against. This is not a second gate and
/// admits nothing — it is a readout — and a caller runs both.
///
/// Set equality, order-independent. A composite key matched on half of itself
/// reaches every row sharing that half.
pub fn delete_matches_key(sql: &str, keys: &[&str]) -> bool {
    let Some(columns) = delete_key_columns(sql) else {
        return false;
    };
    columns.len() == keys.len() && keys.iter().all(|key| columns.iter().any(|c| c == key))
}

/// Whether `sql` is a `SELECT` Slate could have written: exactly one root
/// statement, a query, with nothing destructive anywhere under it.
///
/// The filter bar is a trust boundary. Everywhere else a statement is either
/// wholly the user's or wholly Slate's; a filter is the user's text spliced
/// into Slate's statement, so this is what makes `id = 1; DROP TABLE t`
/// structurally impossible rather than merely unlikely. It also guards the
/// filters Slate writes for itself.
///
/// Not the second gate `AGENTS.md` rule 2 forbids. That rule governs the one
/// path by which the grid writes, and `is_generated_write` remains its only
/// gate; this guards a path that did not previously admit user text at all,
/// and it admits no write -- a statement reaching it must be a query. Neither
/// is a way around the other, and no generated statement passes through both.
///
/// Exactly one root statement rather than `generated_statements`' view through
/// a transaction: a preview never brackets anything, so seeing through
/// brackets here would only widen what is accepted.
pub fn is_generated_select(sql: &str) -> bool {
    let Some(tree) = parse(sql) else {
        return false;
    };
    let root = tree.root_node();
    let mut cursor = root.walk();
    // Comments are tree-sitter extras and land at the root too, so anything
    // that is not the one statement is something Slate did not generate.
    let children: Vec<_> = root.named_children(&mut cursor).collect();
    let [statement] = children.as_slice() else {
        return false;
    };
    let mut cursor = statement.walk();

    // A `select` among the statement's own children, not its first: `WITH`
    // puts `keyword_with` and the cte ahead of the outer query's select, the
    // same level `select_anchor` reads it back from. A write hides its select
    // inside its own `insert` or `update` node, so none reaches this level.
    statement.kind() == "statement"
        && statement
            .named_children(&mut cursor)
            .any(|node| node.kind() == "select")
        && !forbidden(root)
        && !deletes_anything(root)
}

/// The statements to check, seeing through the transaction that brackets a
/// batch on an engine which does not make one submission atomic by itself.
///
/// The brackets are verified rather than assumed. A `BEGIN` without its
/// `COMMIT` would leave the session in an open transaction, and putting a user
/// in that state without them having written it is exactly what this gate
/// exists to prevent.
fn generated_statements<'tree>(root: &Node<'tree>) -> Option<Vec<Node<'tree>>> {
    let mut cursor = root.walk();
    let children: Vec<_> = root.named_children(&mut cursor).collect();

    let [transaction] = children.as_slice() else {
        return Some(children);
    };
    if transaction.kind() != "transaction" {
        return Some(children);
    }

    let mut cursor = transaction.walk();
    let bracketed: Vec<_> = transaction.named_children(&mut cursor).collect();
    match bracketed.as_slice() {
        [begin, statements @ .., commit]
            if begin.kind() == "keyword_begin" && commit.kind() == "keyword_commit" =>
        {
            Some(statements.to_vec())
        }
        _ => None,
    }
}

fn assignments(engine: Engine, columns: &[(&str, Option<&str>)], separator: &str) -> String {
    columns
        .iter()
        .map(|&(column, value)| {
            format!(
                "{} = {}",
                engine.quote_identifier(column),
                literal(engine, value)
            )
        })
        .collect::<Vec<_>>()
        .join(separator)
}

/// A value as it goes into a statement: quoted, or the keyword for there being
/// no value. Unquoted is the only way to write it — `'NULL'` is the word.
fn literal(engine: Engine, value: Option<&str>) -> String {
    match value {
        Some(value) => engine.quote_literal(value),
        None => "NULL".to_string(),
    }
}

/// `DROP` and `TRUNCATE`, anywhere in the tree and under every spelling. Never
/// admitted, by any shape, for any reason.
///
/// The grammar offers no `drop` or `truncate` node to look for. `DROP TABLE` is
/// `drop_table`, one of thirteen `drop_*` siblings, and `TRUNCATE t` is a bare
/// `statement` holding a `keyword_truncate` with no wrapper node at all. The
/// keyword is the one part every spelling of either has.
fn forbidden(node: tree_sitter::Node) -> bool {
    let mut cursor = node.walk();
    matches!(node.kind(), "keyword_drop" | "keyword_truncate")
        || node.children(&mut cursor).any(forbidden)
}

/// Any `delete` at all, anywhere in the tree, not only at the root.
/// `WITH x AS (DELETE FROM t RETURNING *) UPDATE …` is a real statement shape
/// whose root child is an `update` node, so the whitelist alone would let it
/// through.
fn deletes_anything(node: tree_sitter::Node) -> bool {
    let mut cursor = node.walk();
    matches!(node.kind(), "delete" | "keyword_delete")
        || node.children(&mut cursor).any(deletes_anything)
}

/// The columns a single-row `DELETE`'s `WHERE` names, read out of the parse
/// tree, or `None` for anything that is not exactly that shape.
///
/// Exactly one root statement whose named children are `["delete", "from"]` —
/// which is where the grammar puts them, with the `where` under the `from` —
/// and whose `WHERE` is a conjunction of equality predicates over distinct,
/// unqualified columns against single-quoted literals. A CTE beside the delete,
/// a `RETURNING`, a `LIMIT`, an `OR`, a subquery, a function call, a qualified
/// column or a second statement all change that child list or that expression
/// tree, and so all arrive here as `None`.
fn delete_key_columns(sql: &str) -> Option<Vec<String>> {
    let tree = parse(sql)?;
    let root = tree.root_node();
    if forbidden(root) {
        return None;
    }

    let mut cursor = root.walk();
    let children: Vec<_> = root.named_children(&mut cursor).collect();
    let [statement] = children.as_slice() else {
        return None;
    };
    if statement.kind() != "statement" {
        return None;
    }

    let mut cursor = statement.walk();
    let parts: Vec<_> = statement.named_children(&mut cursor).collect();
    let [delete, from] = parts.as_slice() else {
        return None;
    };
    if delete.kind() != "delete" || from.kind() != "from" {
        return None;
    }

    let mut cursor = from.walk();
    let inside: Vec<_> = from.named_children(&mut cursor).collect();
    let [keyword, relation, filter] = inside.as_slice() else {
        return None;
    };
    if keyword.kind() != "keyword_from"
        || relation.kind() != "object_reference"
        || filter.kind() != "where"
    {
        return None;
    }

    let mut cursor = filter.walk();
    let clause: Vec<_> = filter.named_children(&mut cursor).collect();
    let [keyword_where, predicate] = clause.as_slice() else {
        return None;
    };
    if keyword_where.kind() != "keyword_where" {
        return None;
    }

    let mut columns = Vec::new();
    if !equality_columns(*predicate, sql, &mut columns) {
        return None;
    }

    // A column named twice is a predicate Slate never writes, and reading it as
    // a one-column key would call a half-matched composite key a whole one.
    let distinct = columns.iter().collect::<std::collections::HashSet<_>>();
    (distinct.len() == columns.len()).then_some(columns)
}

/// Walks a conjunction, pushing the column each `=` predicate names. False the
/// moment anything else appears — an `OR`, another operator, a parenthesized
/// group, a subquery, a function call.
fn equality_columns(node: tree_sitter::Node, sql: &str, columns: &mut Vec<String>) -> bool {
    if node.kind() != "binary_expression" {
        return false;
    }
    let mut cursor = node.walk();
    let children: Vec<_> = node.children(&mut cursor).collect();
    let [left, operator, right] = children.as_slice() else {
        return false;
    };

    match operator.kind() {
        "keyword_and" => {
            equality_columns(*left, sql, columns) && equality_columns(*right, sql, columns)
        }
        "=" => {
            let Some(value) = sql.get(right.byte_range()) else {
                return false;
            };
            // A value is a single-quoted literal and nothing else. `"other"` is
            // a `literal` to this grammar too, and matching a column against a
            // column is not naming a row.
            if right.kind() != "literal" || !value.starts_with('\'') {
                return false;
            }
            match column_name(*left, sql) {
                Some(column) => {
                    columns.push(column);
                    true
                }
                None => false,
            }
        }
        _ => false,
    }
}

/// The unqualified column a predicate's left side names, unquoted.
///
/// To this grammar a double quote opens a **string**: a bare or backticked name
/// arrives as a `field`, but `"id"` arrives as a `literal` indistinguishable by
/// kind from `'id'`, so the quote character is what tells them apart. That
/// matters because Postgres and SQLite quote identifiers with `"`, which is
/// what `delete_row` writes on both.
fn column_name(node: tree_sitter::Node, sql: &str) -> Option<String> {
    let text = sql.get(node.byte_range())?;
    match node.kind() {
        // `t.id` puts an `object_reference` under the field beside the
        // identifier. A qualified column is not one this reads.
        "field" => {
            let mut cursor = node.walk();
            let named: Vec<_> = node.named_children(&mut cursor).collect();
            let [identifier] = named.as_slice() else {
                return None;
            };
            (identifier.kind() == "identifier").then(|| unquote(text, '`'))
        }
        "literal" if text.starts_with('"') => Some(unquote(text, '"')),
        _ => None,
    }
}

fn unquote(text: &str, quote: char) -> String {
    let doubled = [quote, quote].iter().collect::<String>();
    match text.strip_prefix(quote).and_then(|t| t.strip_suffix(quote)) {
        Some(inner) => inner.replace(&doubled, &quote.to_string()),
        None => text.to_string(),
    }
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
fn clause_anchor<'tree>(tree: &'tree Tree, sql: &str) -> Option<tree_sitter::Node<'tree>> {
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
    if let Some(set_operation) = children.iter().find(|node| node.kind() == "set_operation") {
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
        assert_eq!(
            &sql[buffer.statement_at(inside_second).unwrap()],
            "SELECT 2"
        );
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
        assert_eq!(
            &sql[buffer.statement_at(sql.len()).unwrap()],
            "SELECT * FROM"
        );
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
            update_row(
                Engine::Postgres,
                "public",
                "measurements",
                &[("note", Some("ok"))],
                &[("id", "7")]
            )
            .unwrap(),
            r#"UPDATE "public"."measurements" SET "note" = 'ok' WHERE "id" = '7'"#
        );
        assert_eq!(
            update_row(
                Engine::Postgres,
                "public",
                "measurements",
                &[("note", Some("ok")), ("depth", Some("12"))],
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
                Engine::Postgres,
                "app",
                "memberships",
                &[("role", Some("owner"))],
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
            update_row(
                Engine::Postgres,
                "s",
                "t",
                &[("a", Some("it's"))],
                &[("id", "o'hara")]
            )
            .unwrap(),
            r#"UPDATE "s"."t" SET "a" = 'it''s' WHERE "id" = 'o''hara'"#
        );
        assert_eq!(
            update_row(
                Engine::Postgres,
                "s",
                r#"od"d"#,
                &[(r#"we"ird"#, Some("x"))],
                &[("id", "1")]
            )
            .unwrap(),
            r#"UPDATE "s"."od""d" SET "we""ird" = 'x' WHERE "id" = '1'"#
        );
    }

    #[test]
    fn a_null_goes_in_as_the_keyword_and_never_as_a_quoted_word() {
        // `'NULL'` is a four-letter string and `NULL` is the absence of a
        // value. The whole worth of the gesture is that the two differ.
        assert_eq!(
            update_row(
                Engine::Postgres,
                "public",
                "measurements",
                &[("note", None)],
                &[("id", "7")]
            )
            .unwrap(),
            r#"UPDATE "public"."measurements" SET "note" = NULL WHERE "id" = '7'"#
        );
        // Mixed, on the engine whose identifier quote is its own: a NULL beside
        // a value must not disturb the separator between them.
        assert_eq!(
            update_row(
                Engine::MySql,
                "slate_dev",
                "measurements",
                &[("note", None), ("depth", Some("12"))],
                &[("id", "7")]
            )
            .unwrap(),
            "UPDATE `slate_dev`.`measurements` SET `note` = NULL, `depth` = '12' WHERE `id` = '7'"
        );
        // And the word itself, typed into a cell, is still a string.
        assert_eq!(
            update_row(
                Engine::Sqlite,
                "main",
                "measurements",
                &[("note", Some("NULL"))],
                &[("id", "7")]
            )
            .unwrap(),
            r#"UPDATE "main"."measurements" SET "note" = 'NULL' WHERE "id" = '7'"#
        );
        // The gate is untouched by this: `SET x = NULL` is an `update` node
        // like any other, and a test here is what proves it rather than hopes.
        let statement =
            update_row(Engine::Postgres, "s", "t", &[("a", None)], &[("id", "1")]).unwrap();
        assert!(is_generated_write(&statement), "{statement} was refused");
    }

    #[test]
    fn an_update_with_nothing_to_match_on_is_refused() {
        // No WHERE rewrites every row in the table. It must not be possible to
        // produce that statement, so a caller with no key gets nothing.
        assert!(update_row(Engine::Postgres, "s", "t", &[("a", Some("1"))], &[]).is_none());
        assert!(update_row(Engine::Postgres, "s", "t", &[], &[("id", "1")]).is_none());
    }

    #[test]
    fn a_generated_insert_names_only_the_columns_it_was_given() {
        // The omission is the design: a column absent from this list is absent
        // from the statement, so the server's default applies to it.
        assert_eq!(
            insert_row(
                Engine::Postgres,
                "public",
                "measurements",
                &[("note", Some("ok")), ("depth", None)]
            )
            .unwrap(),
            r#"INSERT INTO "public"."measurements" ("note", "depth") VALUES ('ok', NULL)"#
        );
        assert_eq!(
            insert_row(Engine::Sqlite, "main", "t", &[("a", Some("o'hara"))]).unwrap(),
            r#"INSERT INTO "main"."t" ("a") VALUES ('o''hara')"#
        );
        // The engine whose identifier quote and literal escape are both its
        // own: a backtick doubles, and a backslash doubles before the
        // apostrophe after it does.
        assert_eq!(
            insert_row(
                Engine::MySql,
                "slate_dev",
                "me`as",
                &[("no`te", Some(r"a\'b"))]
            )
            .unwrap(),
            r"INSERT INTO `slate_dev`.`me``as` (`no``te`) VALUES ('a\\''b')"
        );
        // An empty form is not `INSERT INTO t DEFAULT VALUES`, which is a
        // statement Slate has never been asked for.
        assert!(insert_row(Engine::Postgres, "s", "t", &[]).is_none());
    }

    #[test]
    fn the_gate_admits_an_insert_and_still_admits_an_update() {
        assert!(is_generated_write(
            r#"INSERT INTO "public"."t" ("a") VALUES ('1')"#
        ));
        assert!(is_generated_write("UPDATE t SET a = '1' WHERE id = '2'"));
        assert!(is_generated_write(
            "BEGIN;\nUPDATE t SET a = '1' WHERE id = '2';\n\
             UPDATE t SET a = '3' WHERE id = '4';\nCOMMIT;"
        ));
        // And what the generator writes, which is the test that keeps the two
        // from drifting apart.
        let statement = insert_row(
            Engine::Postgres,
            "public",
            "measurements",
            &[("note", Some("it's fine")), ("depth", None)],
        )
        .unwrap();
        assert!(is_generated_write(&statement), "{statement} was refused");
    }

    #[test]
    fn the_gate_refuses_an_insert_carrying_something_else() {
        // One insert, alone. A batch of them is a shape nothing generates, and
        // a `DELETE` riding along in a CTE is the shape an injected value takes.
        for sql in [
            "INSERT INTO t (a) VALUES ('1'); DROP TABLE t",
            "INSERT INTO t (a) VALUES ('1'); TRUNCATE t",
            "WITH x AS (DELETE FROM t RETURNING *) INSERT INTO u (a) VALUES ('1')",
            "INSERT INTO t (a) VALUES ('1'); INSERT INTO t (a) VALUES ('2')",
        ] {
            assert!(!is_generated_write(sql), "{sql} passed the gate");
        }
    }

    #[test]
    fn the_gate_accepts_an_update_and_a_batch_of_updates() {
        assert!(is_generated_write("UPDATE t SET a = '1' WHERE id = '2'"));
        assert!(is_generated_write(
            "UPDATE t SET a = '1' WHERE id = '2'; UPDATE t SET a = '3' WHERE id = '4'"
        ));
    }

    #[test]
    fn the_gate_accepts_a_batch_bracketed_by_a_transaction() {
        // What Slate writes for an engine that commits each statement on its
        // own. The brackets are part of the generated statement, so the gate
        // has to know the shape or it would refuse Slate's own output.
        assert!(is_generated_write(
            "BEGIN;\nUPDATE t SET a = '1' WHERE id = '2';\n\
             UPDATE t SET a = '3' WHERE id = '4';\nCOMMIT;"
        ));
    }

    #[test]
    fn the_gate_refuses_a_transaction_it_does_not_see_closed() {
        // A BEGIN whose COMMIT went missing leaves the session holding an open
        // transaction the user never wrote, which is worse than not applying
        // the edit at all.
        for sql in [
            "BEGIN;\nUPDATE t SET a = '1' WHERE id = '2';",
            "BEGIN;\nUPDATE t SET a = '1' WHERE id = '2';\nROLLBACK;",
        ] {
            assert!(!is_generated_write(sql), "{sql:?} passed the gate");
        }
    }

    #[test]
    fn the_gate_refuses_a_destructive_statement_inside_the_brackets() {
        // Seeing through the transaction must not mean trusting what is in it.
        assert!(!is_generated_write(
            "BEGIN;\nUPDATE t SET a = '1' WHERE id = '2';\nDELETE FROM t;\nCOMMIT;"
        ));
    }

    #[test]
    fn the_gate_refuses_everything_that_is_not_a_write_slate_writes() {
        // Hard rule 1 in code: DROP and TRUNCATE never leave Slate, whatever
        // the user asked for. SELECT is here because the gate is a whitelist --
        // being harmless is not the test, being one of the three shapes Slate
        // generates is. A keyed DELETE is no longer in this list because it is
        // one of those shapes; `delete_matches_key` is what asks whether the key
        // it names is the row's.
        for sql in [
            "DROP TABLE t",
            "DROP VIEW v",
            "DROP DATABASE d",
            "TRUNCATE t",
            "TRUNCATE TABLE t",
            "SELECT 1",
        ] {
            assert!(!is_generated_write(sql), "{sql} passed the gate");
        }
    }

    #[test]
    fn the_gate_refuses_a_batch_with_one_destructive_statement_in_it() {
        // Every statement is checked, not the first one. A DELETE appended to a
        // run of legitimate updates is the shape an injected value would take.
        assert!(!is_generated_write(
            "UPDATE t SET a = '1' WHERE id = '2'; DELETE FROM t; UPDATE t SET a = '3' WHERE id = '4'"
        ));
    }

    #[test]
    fn the_gate_refuses_a_destructive_statement_wrapped_in_a_cte() {
        // The root statement's first child here really is an `update` node, so
        // the whitelist passes it and only the subtree scan catches it.
        assert!(!is_generated_write(
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
            assert!(!is_generated_write(sql), "{sql:?} passed the gate");
        }
    }

    #[test]
    fn the_gate_accepts_what_update_row_writes() {
        // The one test that keeps the generator and the gate from drifting
        // apart: whatever quoting or clause order changes here, the statement
        // Slate builds is still one the gate can read as an UPDATE.
        let statement = update_row(
            Engine::Postgres,
            "public",
            "measurements",
            &[("note", Some("it's fine")), ("depth", Some("12"))],
            &[("id", "7"), ("run", "a'b")],
        )
        .unwrap();

        assert!(is_generated_write(&statement), "{statement} was refused");
        assert!(is_generated_write(&format!("{statement}; {statement}")));
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

    #[test]
    fn the_select_gate_accepts_the_shape_a_preview_has() {
        for sql in [
            r#"SELECT * FROM "public"."accounts" LIMIT 1000"#,
            r#"SELECT * FROM "public"."accounts" WHERE "state" = 'ok' LIMIT 1000"#,
            r#"SELECT * FROM "public"."accounts" WHERE "state" = 'ok' ORDER BY "id" ASC LIMIT 100 OFFSET 200"#,
            "SELECT * FROM `slate_dev`.`accounts` WHERE `state` = 'ok' LIMIT 100",
            r#"WITH x AS (SELECT 1 AS a) SELECT * FROM x LIMIT 10"#,
        ] {
            assert!(is_generated_select(sql), "{sql} was refused");
        }
    }

    #[test]
    fn the_select_gate_refuses_a_filter_carrying_a_second_statement() {
        // The reason this gate exists. Whether each is refused for having two
        // roots or for not parsing is not the point -- refused is the point.
        for sql in [
            r#"SELECT * FROM "public"."t" WHERE "id" = '1'; DROP TABLE "t" LIMIT 1000"#,
            r#"SELECT * FROM "public"."t" WHERE "id" = '1' LIMIT 1000; DROP TABLE "t""#,
            r#"SELECT * FROM "public"."t" WHERE "id" = '1'; DELETE FROM "t" LIMIT 1000"#,
            r#"SELECT * FROM "public"."t" WHERE "id" = '1'; TRUNCATE "t" LIMIT 1000"#,
        ] {
            assert!(!is_generated_select(sql), "{sql} passed the gate");
        }
    }

    #[test]
    fn the_select_gate_refuses_a_filter_that_does_not_parse() {
        for sql in [
            r#"SELECT * FROM "public"."t" WHERE "id" = ((( LIMIT 1000"#,
            r#"SELECT * FROM "public"."t" WHERE "id" = 'unclosed LIMIT 1000"#,
            r#"SELECT * FROM "public"."t" WHERE LIMIT 1000"#,
            "",
        ] {
            assert!(!is_generated_select(sql), "{sql:?} passed the gate");
        }
    }

    #[test]
    fn the_select_gate_refuses_a_destructive_statement_hidden_in_a_cte() {
        // The root's first named child here is a `select`, so the whitelist
        // alone passes it and only the recursive scan catches it. THIS TEST IS
        // LOAD-BEARING: a later task splits `destructive` apart for the DELETE
        // path, and this is what fails if the SELECT gate is not updated too.
        for sql in [
            r#"WITH x AS (DELETE FROM "t" RETURNING *) SELECT * FROM x LIMIT 1000"#,
            r#"WITH x AS (SELECT 1 AS a) SELECT * FROM x WHERE a IN (SELECT 1); DROP TABLE "t""#,
        ] {
            assert!(!is_generated_select(sql), "{sql} passed the gate");
        }
    }

    #[test]
    fn the_select_gate_admits_no_write_at_all() {
        // A whitelist, not a blocklist: being harmless is not the test, being
        // a SELECT is.
        for sql in [
            "UPDATE t SET a = '1' WHERE id = '2'",
            "INSERT INTO t (a) VALUES ('1')",
            "DELETE FROM t WHERE a = '1'",
            "DROP TABLE t",
            "TRUNCATE t",
            "BEGIN; SELECT 1; COMMIT",
            "SELECT 1; SELECT 2",
            "-- SELECT * FROM t",
        ] {
            assert!(!is_generated_select(sql), "{sql} passed the gate");
        }
    }

    #[test]
    fn a_generated_delete_names_the_row_and_only_the_row() {
        assert_eq!(
            delete_row(Engine::Postgres, "public", "measurements", &[("id", "7")]).unwrap(),
            r#"DELETE FROM "public"."measurements" WHERE "id" = '7'"#
        );
        // Joined by OR, or with a column dropped, this deletes rows the user
        // never pointed at.
        assert_eq!(
            delete_row(
                Engine::Postgres,
                "app",
                "memberships",
                &[("org_id", "1"), ("user_id", "2")]
            )
            .unwrap(),
            r#"DELETE FROM "app"."memberships" WHERE "org_id" = '1' AND "user_id" = '2'"#
        );
        assert_eq!(
            delete_row(Engine::MySql, "slate_dev", "measurements", &[("id", "7")]).unwrap(),
            "DELETE FROM `slate_dev`.`measurements` WHERE `id` = '7'"
        );
        assert_eq!(
            delete_row(Engine::Sqlite, "main", "t", &[("id", "o'hara")]).unwrap(),
            r#"DELETE FROM "main"."t" WHERE "id" = 'o''hara'"#
        );
        // No WHERE empties the table, so it must not be possible to produce.
        assert!(delete_row(Engine::Postgres, "s", "t", &[]).is_none());
    }

    #[test]
    fn the_gate_admits_the_delete_slate_writes_and_reads_its_key_back() {
        for engine in [Engine::Postgres, Engine::MySql, Engine::Sqlite] {
            let statement = delete_row(engine, "s", "t", &[("id", "7")]).unwrap();
            assert!(is_generated_write(&statement), "{statement} was refused");
            assert!(delete_matches_key(&statement, &["id"]), "{statement}");

            let composite =
                delete_row(engine, "s", "t", &[("org_id", "1"), ("user_id", "2")]).unwrap();
            assert!(is_generated_write(&composite), "{composite} was refused");
            assert!(delete_matches_key(&composite, &["org_id", "user_id"]));
            // Set equality: the key is a set of columns, not a sequence.
            assert!(delete_matches_key(&composite, &["user_id", "org_id"]));
        }
        // A value carrying the quote character still reads back.
        let statement = delete_row(Engine::Postgres, "s", "t", &[("id", "o'hara")]).unwrap();
        assert!(is_generated_write(&statement), "{statement} was refused");
        assert!(delete_matches_key(&statement, &["id"]));

        // A column name carrying one does not: `"we""ird"` is two adjacent
        // strings to this grammar and the whole statement fails to parse, so
        // the gate refuses Slate's own output. That is the safe direction --
        // the row stays -- and a gate that guessed past an unreadable tree is
        // the unsafe one.
        let odd = delete_row(Engine::Postgres, "s", "t", &[(r#"we"ird"#, "x")]).unwrap();
        assert!(!is_generated_write(&odd), "{odd} passed the gate");
    }

    #[test]
    fn the_gate_refuses_every_delete_that_is_not_one_named_row() {
        for sql in [
            "DELETE FROM t",
            r#"DELETE FROM "public"."t""#,
            r#"DELETE FROM t WHERE "id" = '1' OR "id" = '2'"#,
            r#"DELETE FROM t WHERE "id" = '1' AND ("a" = '2' OR "b" = '3')"#,
            r#"DELETE FROM t WHERE "id" > '1'"#,
            r#"DELETE FROM t WHERE "id" <> '1'"#,
            r#"DELETE FROM t WHERE "id" LIKE '1%'"#,
            r#"DELETE FROM t WHERE "id" IS NULL"#,
            "DELETE FROM t WHERE id IN (SELECT id FROM u)",
            "DELETE FROM t WHERE id = lower('a')",
            "WITH x AS (SELECT 1) DELETE FROM t WHERE id = '1'",
            "DELETE FROM t WHERE id = '1' LIMIT 1",
            "DELETE FROM t WHERE id = '1' RETURNING *",
            "DELETE FROM t WHERE id = '1'; DELETE FROM t WHERE id = '2'",
            "DELETE FROM t WHERE id = '1'; DROP TABLE t",
            "UPDATE t SET a = '1' WHERE id = '2'; DELETE FROM t WHERE id = '3'",
            r#"DELETE FROM t WHERE "id" = "other""#,
            "DELETE FROM t WHERE t.id = '1'",
            "WITH x AS (DROP TABLE u) DELETE FROM t WHERE id = '1'",
            "WITH x AS (DROP TABLE u) UPDATE t SET a = '1' WHERE id = '2'",
        ] {
            assert!(!is_generated_write(sql), "{sql:?} passed the gate");
        }
    }

    #[test]
    fn a_delete_keyed_on_the_wrong_columns_is_refused_by_the_key_check() {
        // These are the right SHAPE -- is_generated_write admits the first two,
        // and must, since it has no key to compare against. delete_matches_key
        // is what refuses them, and a caller runs both.
        let wrong_column = r#"DELETE FROM "s"."t" WHERE "note" = 'x'"#;
        assert!(is_generated_write(wrong_column));
        assert!(!delete_matches_key(wrong_column, &["id"]));

        let half = r#"DELETE FROM "s"."t" WHERE "org_id" = '1'"#;
        assert!(is_generated_write(half));
        assert!(!delete_matches_key(half, &["org_id", "user_id"]));

        // A column named twice would read as a one-column key. This one the
        // shape check itself refuses, and the readout agrees.
        let twice = r#"DELETE FROM "s"."t" WHERE "id" = '1' AND "id" = '2'"#;
        assert!(!is_generated_write(twice));
        assert!(!delete_matches_key(twice, &["id"]));

        // And a statement that is not a delete at all answers no here too.
        assert!(!delete_matches_key(
            r#"UPDATE "s"."t" SET "a" = '1' WHERE "id" = '2'"#,
            &["id"]
        ));
        assert!(!delete_matches_key("DROP TABLE t", &["id"]));
    }
}
