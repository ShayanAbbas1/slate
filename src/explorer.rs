use std::collections::HashMap;

use gpui_component::tree::TreeItem;

use crate::db::{Catalog, Relation, RelationKind, Routine, RoutineKind};

/// The row counts a preview can be asked for, and the one it opens with. Every
/// result set is capped (spec §4.3); this is the part of the cap the user gets
/// to move, and the grid shows which one is in effect.
pub const ROW_LIMITS: [usize; 4] = [100, 1_000, 10_000, 100_000];
pub const PREVIEW_ROW_LIMIT: usize = ROW_LIMITS[1];

const RELATION_CATEGORIES: [(RelationKind, &str); 5] = [
    (RelationKind::Table, "Tables"),
    (RelationKind::PartitionedTable, "Partitioned Tables"),
    (RelationKind::View, "Views"),
    (RelationKind::MaterializedView, "Materialized Views"),
    (RelationKind::ForeignTable, "Foreign Tables"),
];

const ROUTINE_CATEGORIES: [(RoutineKind, &str); 2] = [
    (RoutineKind::Function, "Functions"),
    (RoutineKind::Procedure, "Procedures"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplorerTarget {
    Relation {
        schema_index: usize,
        relation_index: usize,
    },
    Routine {
        schema_index: usize,
        routine_index: usize,
    },
}

/// What kind of object a row stands for. The sidebar draws an icon from this;
/// the kind lives here rather than an `IconName` so the tree stays comparable
/// in tests and free of the widget library's types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    Relation(RelationKind),
    Routine(RoutineKind),
}

/// What the sidebar knows about one openable row: where it points, and what it
/// is. One map rather than two, so a leaf can never end up with a target and no
/// kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExplorerLeaf {
    pub target: ExplorerTarget,
    pub kind: ObjectKind,
}

pub struct ExplorerTree {
    pub items: Vec<TreeItem>,
    pub leaves: HashMap<String, ExplorerLeaf>,
}

pub fn tree(catalog: &Catalog, filter: &str) -> ExplorerTree {
    let filter = filter.trim().to_lowercase();
    let mut leaves = HashMap::new();

    let items = catalog
        .schemas
        .iter()
        .enumerate()
        .filter_map(|(schema_index, schema)| {
            let schema_matches = matches_filter(&schema.name, &filter);
            let mut groups = Vec::new();

            for (kind, label) in RELATION_CATEGORIES {
                let children = schema
                    .relations
                    .iter()
                    .enumerate()
                    .filter(|(_, relation)| {
                        relation.kind == kind
                            && (schema_matches || relation_matches(relation, &filter))
                    })
                    .map(|(relation_index, relation)| {
                        let id = format!("relation-{schema_index}-{relation_index}");
                        leaves.insert(
                            id.clone(),
                            ExplorerLeaf {
                                target: ExplorerTarget::Relation {
                                    schema_index,
                                    relation_index,
                                },
                                kind: ObjectKind::Relation(kind),
                            },
                        );
                        TreeItem::new(id, relation.name.clone())
                    })
                    .collect::<Vec<_>>();

                if !children.is_empty() {
                    groups.push(category(label, schema_index, children));
                }
            }

            for (kind, label) in ROUTINE_CATEGORIES {
                let children = schema
                    .routines
                    .iter()
                    .enumerate()
                    .filter(|(_, routine)| {
                        routine.kind == kind && (schema_matches || routine_matches(routine, &filter))
                    })
                    .map(|(routine_index, routine)| {
                        let id = format!("routine-{schema_index}-{routine_index}");
                        leaves.insert(
                            id.clone(),
                            ExplorerLeaf {
                                target: ExplorerTarget::Routine {
                                    schema_index,
                                    routine_index,
                                },
                                kind: ObjectKind::Routine(kind),
                            },
                        );
                        TreeItem::new(
                            id,
                            format!("{}({})", routine.name, routine.identity_arguments),
                        )
                    })
                    .collect::<Vec<_>>();

                if !children.is_empty() {
                    groups.push(category(label, schema_index, children));
                }
            }

            if !schema_matches && groups.is_empty() {
                return None;
            }

            Some(
                TreeItem::new(format!("schema-{schema_index}"), schema.name.clone())
                    .expanded(true)
                    .children(groups),
            )
        })
        .collect();

    ExplorerTree { items, leaves }
}

fn category(label: &'static str, schema_index: usize, children: Vec<TreeItem>) -> TreeItem {
    TreeItem::new(format!("category-{label}-{schema_index}"), label)
        .expanded(true)
        .children(children)
}

pub fn preview_sql(schema: &str, relation: &str, limit: usize) -> String {
    format!(
        "SELECT * FROM {}.{} LIMIT {limit}",
        quote_identifier(schema),
        quote_identifier(relation)
    )
}

pub(crate) fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn relation_matches(relation: &Relation, filter: &str) -> bool {
    matches_filter(&relation.name, filter)
}

fn routine_matches(routine: &Routine, filter: &str) -> bool {
    matches_filter(&routine.name, filter)
        || matches_filter(&routine.identity_arguments, filter)
        || matches_filter(&routine.result_type, filter)
        || matches_filter(&routine.language, filter)
}

fn matches_filter(value: &str, filter: &str) -> bool {
    filter.is_empty() || value.to_lowercase().contains(filter)
}

#[cfg(test)]
mod tests {
    use crate::db::{Catalog, Relation, RelationKind, Routine, RoutineKind, Schema};

    use super::*;

    fn catalog() -> Catalog {
        Catalog {
            schemas: vec![
                Schema {
                    name: "analytics".into(),
                    relations: vec![Relation {
                        name: "events".into(),
                        kind: RelationKind::Table,
                    }],
                    routines: Vec::new(),
                },
                Schema {
                    name: "public".into(),
                    relations: vec![
                        Relation {
                            name: "active_accounts".into(),
                            kind: RelationKind::View,
                        },
                        Relation {
                            name: "accounts".into(),
                            kind: RelationKind::Table,
                        },
                    ],
                    routines: vec![
                        Routine {
                            name: "reindex".into(),
                            kind: RoutineKind::Procedure,
                            identity_arguments: String::new(),
                            result_type: String::new(),
                            language: "plpgsql".into(),
                            definition: String::new(),
                        },
                        Routine {
                            name: "account_name".into(),
                            kind: RoutineKind::Function,
                            identity_arguments: "account_id bigint".into(),
                            result_type: "text".into(),
                            language: "sql".into(),
                            definition: String::new(),
                        },
                    ],
                },
            ],
        }
    }

    #[test]
    fn filter_keeps_the_matching_object_hierarchy() {
        let explorer = tree(&catalog(), "account_name");
        let items = explorer.items;

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "public");
        assert_eq!(items[0].children.len(), 1);
        assert_eq!(items[0].children[0].label, "Functions");
        assert_eq!(
            items[0].children[0].children[0].label,
            "account_name(account_id bigint)"
        );
    }

    #[test]
    fn each_object_kind_gets_its_own_category() {
        let explorer = tree(&catalog(), "public");
        let categories = &explorer.items[0].children;

        assert_eq!(
            categories.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>(),
            ["Tables", "Views", "Functions", "Procedures"]
        );
        assert_eq!(categories[0].children[0].label, "accounts");
        assert_eq!(categories[1].children[0].label, "active_accounts");
        assert_eq!(categories[3].children[0].label, "reindex()");
    }

    #[test]
    fn matching_a_schema_keeps_all_of_its_objects() {
        let explorer = tree(&catalog(), "analytics");
        let items = explorer.items;

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "analytics");
        assert_eq!(items[0].children[0].children[0].label, "events");
    }

    #[test]
    fn tree_targets_use_catalog_indices_not_database_names() {
        let explorer = tree(&catalog(), "account_name");

        assert_eq!(
            explorer.leaves.get("routine-1-1"),
            Some(&ExplorerLeaf {
                target: ExplorerTarget::Routine {
                    schema_index: 1,
                    routine_index: 1,
                },
                kind: ObjectKind::Routine(RoutineKind::Function),
            })
        );
    }

    #[test]
    fn a_leaf_carries_the_kind_its_icon_is_drawn_from() {
        let explorer = tree(&catalog(), "public");
        let kind = |id: &str| explorer.leaves.get(id).map(|leaf| leaf.kind);

        // public: relation 0 is a view, relation 1 is a table.
        assert_eq!(
            kind("relation-1-0"),
            Some(ObjectKind::Relation(RelationKind::View))
        );
        assert_eq!(
            kind("relation-1-1"),
            Some(ObjectKind::Relation(RelationKind::Table))
        );
    }

    #[test]
    fn preview_sql_quotes_every_identifier_and_exposes_the_limit() {
        assert_eq!(
            preview_sql(r#"odd"schema"#, r#"table"name"#, PREVIEW_ROW_LIMIT),
            r#"SELECT * FROM "odd""schema"."table""name" LIMIT 1000"#
        );
    }
}
