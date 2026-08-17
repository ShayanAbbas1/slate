use gpui_component::tree::TreeItem;

use crate::db::{Catalog, Relation, Routine};

pub fn tree_items(catalog: &Catalog, filter: &str) -> Vec<TreeItem> {
    let filter = filter.trim().to_lowercase();

    catalog
        .schemas
        .iter()
        .filter_map(|schema| {
            let schema_matches = matches_filter(&schema.name, &filter);
            let relations = schema
                .relations
                .iter()
                .filter(|relation| schema_matches || relation_matches(relation, &filter))
                .map(|relation| {
                    TreeItem::new(
                        format!("relation:{}:{}", schema.name, relation.name),
                        relation.name.clone(),
                    )
                })
                .collect::<Vec<_>>();
            let routines = schema
                .routines
                .iter()
                .filter(|routine| schema_matches || routine_matches(routine, &filter))
                .map(|routine| {
                    TreeItem::new(
                        format!(
                            "routine:{}:{}:{}",
                            schema.name, routine.name, routine.identity_arguments
                        ),
                        format!("{}({})", routine.name, routine.identity_arguments),
                    )
                })
                .collect::<Vec<_>>();

            if !schema_matches && relations.is_empty() && routines.is_empty() {
                return None;
            }

            let mut groups = Vec::new();
            if !relations.is_empty() {
                groups.push(
                    TreeItem::new(format!("relations:{}", schema.name), "Tables & Views")
                        .expanded(true)
                        .children(relations),
                );
            }
            if !routines.is_empty() {
                groups.push(
                    TreeItem::new(
                        format!("routines:{}", schema.name),
                        "Functions & Procedures",
                    )
                    .expanded(true)
                    .children(routines),
                );
            }

            Some(
                TreeItem::new(format!("schema:{}", schema.name), schema.name.clone())
                    .expanded(true)
                    .children(groups),
            )
        })
        .collect()
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
        let items = tree_items(&catalog(), "account_name");

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
        let items = tree_items(&catalog(), "analytics");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "analytics");
        assert_eq!(items[0].children[0].children[0].label, "events");
    }
}
