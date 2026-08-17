use std::collections::HashMap;

use gpui_component::tree::TreeItem;

use crate::db::{Catalog, Relation, Routine};

pub const PREVIEW_ROW_LIMIT: usize = 1_000;

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

pub struct ExplorerTree {
    pub items: Vec<TreeItem>,
    pub targets: HashMap<String, ExplorerTarget>,
}

pub fn tree(catalog: &Catalog, filter: &str) -> ExplorerTree {
    let filter = filter.trim().to_lowercase();
    let mut targets = HashMap::new();

    let items = catalog
        .schemas
        .iter()
        .enumerate()
        .filter_map(|(schema_index, schema)| {
            let schema_matches = matches_filter(&schema.name, &filter);
            let relations = schema
                .relations
                .iter()
                .enumerate()
                .filter_map(|(relation_index, relation)| {
                    if !schema_matches && !relation_matches(relation, &filter) {
                        return None;
                    }

                    let id = format!("relation-{schema_index}-{relation_index}");
                    targets.insert(
                        id.clone(),
                        ExplorerTarget::Relation {
                            schema_index,
                            relation_index,
                        },
                    );
                    Some(TreeItem::new(id, relation.name.clone()))
                })
                .collect::<Vec<_>>();
            let routines = schema
                .routines
                .iter()
                .enumerate()
                .filter_map(|(routine_index, routine)| {
                    if !schema_matches && !routine_matches(routine, &filter) {
                        return None;
                    }

                    let id = format!("routine-{schema_index}-{routine_index}");
                    targets.insert(
                        id.clone(),
                        ExplorerTarget::Routine {
                            schema_index,
                            routine_index,
                        },
                    );
                    Some(TreeItem::new(
                        id,
                        format!("{}({})", routine.name, routine.identity_arguments),
                    ))
                })
                .collect::<Vec<_>>();

            if !schema_matches && relations.is_empty() && routines.is_empty() {
                return None;
            }

            let mut groups = Vec::new();
            if !relations.is_empty() {
                groups.push(
                    TreeItem::new(format!("relations-{schema_index}"), "Tables & Views")
                        .expanded(true)
                        .children(relations),
                );
            }
            if !routines.is_empty() {
                groups.push(
                    TreeItem::new(format!("routines-{schema_index}"), "Functions & Procedures")
                        .expanded(true)
                        .children(routines),
                );
            }

            Some(
                TreeItem::new(format!("schema-{schema_index}"), schema.name.clone())
                    .expanded(true)
                    .children(groups),
            )
        })
        .collect();

    ExplorerTree { items, targets }
}

pub fn preview_sql(schema: &str, relation: &str) -> String {
    format!(
        "SELECT * FROM {}.{} LIMIT {PREVIEW_ROW_LIMIT}",
        quote_identifier(schema),
        quote_identifier(relation)
    )
}

fn quote_identifier(identifier: &str) -> String {
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
                    relations: vec![Relation {
                        name: "accounts".into(),
                        kind: RelationKind::Table,
                    }],
                    routines: vec![Routine {
                        name: "account_name".into(),
                        kind: RoutineKind::Function,
                        identity_arguments: "account_id bigint".into(),
                        result_type: "text".into(),
                        language: "sql".into(),
                        definition: String::new(),
                    }],
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
        assert_eq!(items[0].children[0].label, "Functions & Procedures");
        assert_eq!(
            items[0].children[0].children[0].label,
            "account_name(account_id bigint)"
        );
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
            explorer.targets.get("routine-1-0"),
            Some(&ExplorerTarget::Routine {
                schema_index: 1,
                routine_index: 0,
            })
        );
    }

    #[test]
    fn preview_sql_quotes_every_identifier_and_exposes_the_limit() {
        assert_eq!(
            preview_sql(r#"odd"schema"#, r#"table"name"#),
            r#"SELECT * FROM "odd""schema"."table""name" LIMIT 1000"#
        );
    }
}
