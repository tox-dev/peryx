use crate::{FieldClass, OutputColumn, Page, Params, Row, Value, ValueType, run};

use super::support::{TestSource, decision, operator_scope};

#[test]
fn test_run_excludes_null_from_inequality_results() {
    assert_eq!(
        run(
            r#"from policy.decisions where source != "cache" select resource"#,
            &Params::new(),
            &operator_scope(),
            None,
            &TestSource::new(rows()),
        ),
        Ok(Page {
            outputs: vec![OutputColumn {
                name: "resource".to_owned(),
                class: FieldClass::Repository,
                value_type: ValueType::Str,
            }],
            rows: vec![vec![Value::Str("resource-b".to_owned())]],
            next_cursor: None,
        })
    );
}

#[test]
fn test_run_excludes_null_from_inequality_count() {
    assert_eq!(
        run(
            r#"from policy.decisions where source != "cache" aggregate count() as total by state"#,
            &Params::new(),
            &operator_scope(),
            None,
            &TestSource::new(rows()),
        ),
        Ok(Page {
            outputs: vec![
                OutputColumn {
                    name: "state".to_owned(),
                    class: FieldClass::Repository,
                    value_type: ValueType::Str,
                },
                OutputColumn {
                    name: "total".to_owned(),
                    class: FieldClass::Public,
                    value_type: ValueType::Int,
                },
            ],
            rows: vec![vec![Value::Str("allowed".to_owned()), Value::Int(1)]],
            next_cursor: None,
        })
    );
}

fn rows() -> Vec<Row> {
    vec![
        decision("alpha", "resource-a", "allowed", "cache", 300, 10),
        decision("alpha", "resource-b", "allowed", "origin", 200, 5),
        Row::new()
            .with("repository", Value::Str("alpha".to_owned()))
            .with("resource", Value::Str("resource-c".to_owned()))
            .with("state", Value::Str("allowed".to_owned()))
            .with("evaluated_at", Value::Timestamp(100)),
    ]
}
