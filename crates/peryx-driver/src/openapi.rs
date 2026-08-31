use serde_json::json;
use utoipa::openapi::content::ContentBuilder;
use utoipa::openapi::path::{OperationBuilder, ParameterBuilder, ParameterIn};
use utoipa::openapi::schema::{ArrayBuilder, ObjectBuilder, Schema, SchemaType, Type};
use utoipa::openapi::{RefOr, Required, ResponseBuilder};

#[must_use]
pub fn route_param() -> ParameterBuilder {
    parameter(
        "route",
        ParameterIn::Path,
        "The index route, for example `team/catalog`",
        json!("team/catalog"),
    )
}

#[must_use]
pub fn query_param(name: &'static str, description: &'static str, example: serde_json::Value) -> ParameterBuilder {
    parameter(name, ParameterIn::Query, description, example)
}

#[must_use]
pub fn parameter(
    name: impl Into<String>,
    parameter_in: ParameterIn,
    description: impl Into<String>,
    example: serde_json::Value,
) -> ParameterBuilder {
    let schema = schema_for(&example);
    parameter_with_schema(name, parameter_in, description, example, schema)
}

fn parameter_with_schema(
    name: impl Into<String>,
    parameter_in: ParameterIn,
    description: impl Into<String>,
    example: serde_json::Value,
    schema: impl Into<RefOr<Schema>>,
) -> ParameterBuilder {
    let required = if parameter_in == ParameterIn::Path {
        Required::True
    } else {
        Required::False
    };
    ParameterBuilder::new()
        .name(name)
        .parameter_in(parameter_in)
        .required(required)
        .description(Some(description))
        .schema(Some(schema))
        .example(Some(example))
}

#[must_use]
pub fn bounded_integer_parameter(
    name: impl Into<String>,
    parameter_in: ParameterIn,
    description: impl Into<String>,
    example: serde_json::Value,
    minimum: Option<i64>,
    maximum: Option<i64>,
) -> ParameterBuilder {
    parameter_with_schema(
        name,
        parameter_in,
        description,
        example,
        ObjectBuilder::new()
            .schema_type(Type::Integer)
            .minimum(minimum)
            .maximum(maximum),
    )
}

#[must_use]
pub fn enum_parameter(
    name: impl Into<String>,
    parameter_in: ParameterIn,
    description: impl Into<String>,
    example: serde_json::Value,
    values: impl IntoIterator<Item = serde_json::Value>,
) -> ParameterBuilder {
    let schema = schema_for(&example).enum_values(Some(values));
    parameter_with_schema(name, parameter_in, description, example, schema)
}

#[must_use]
pub fn string_array_parameter(
    name: impl Into<String>,
    parameter_in: ParameterIn,
    description: impl Into<String>,
    example: serde_json::Value,
) -> ParameterBuilder {
    parameter_with_schema(
        name,
        parameter_in,
        description,
        example,
        ArrayBuilder::new().items(ObjectBuilder::new().schema_type(Type::String)),
    )
}

fn schema_for(example: &serde_json::Value) -> ObjectBuilder {
    ObjectBuilder::new().schema_type(match example {
        serde_json::Value::Null => SchemaType::AnyValue,
        serde_json::Value::Bool(_) => Type::Boolean.into(),
        serde_json::Value::Number(number) if number.is_i64() || number.is_u64() => Type::Integer.into(),
        serde_json::Value::Number(_) => Type::Number.into(),
        serde_json::Value::String(_) => Type::String.into(),
        serde_json::Value::Array(_) => Type::Array.into(),
        serde_json::Value::Object(_) => Type::Object.into(),
    })
}

#[must_use]
pub fn api_json_response(description: &str, example: serde_json::Value) -> ResponseBuilder {
    ResponseBuilder::new()
        .description(description)
        .content("application/json", ContentBuilder::new().example(Some(example)).build())
}

#[must_use]
pub fn text_response(description: &str, content_type: &str, example: &str) -> ResponseBuilder {
    ResponseBuilder::new().description(description).content(
        content_type,
        ContentBuilder::new().example(Some(json!(example))).build(),
    )
}

#[must_use]
pub fn artifact_search(scoped: bool) -> OperationBuilder {
    let mut operation = OperationBuilder::new()
        .tag("search")
        .summary(Some(if scoped {
            "Search one index route"
        } else {
            "Search cached resources"
        }))
        .description(Some(
            "Searches the derived artifact index built from cached listings, local writes, \
             and cached metadata. `q` uses substring matching and needs at least two \
             characters; prefix it with `re:` for a regex, which reads every indexed \
             document and is restricted to operators. Index policy removes denied entries \
             before indexing. Results are sorted by display name and paged without \
             collecting every match.",
        ))
        .parameter(query_param(
            "q",
            "Search text of at least two characters. Prefix with `re:` to use a regex, which operators alone may run.",
            json!("widget"),
        ))
        .parameter(enum_parameter(
            "type",
            ParameterIn::Query,
            "`uploaded`, `cached`, or `override`; omit for all sources.",
            json!("override"),
            [json!("uploaded"), json!("cached"), json!("override")],
        ))
        .parameter(enum_parameter(
            "availability",
            ParameterIn::Query,
            "`local` returns only resources whose bytes are available from local storage now; omit or \
             `all` returns every indexed resource.",
            json!("local"),
            [json!("local"), json!("all")],
        ))
        .parameter(bounded_integer_parameter(
            "page",
            ParameterIn::Query,
            "One-based page number.",
            json!(1),
            Some(1),
            None,
        ))
        .parameter(enum_parameter(
            "page_size",
            ParameterIn::Query,
            "Page size: 25, 50, or 100.",
            json!(25),
            [json!(25), json!(50), json!(100)],
        ))
        .response(
            "200",
            api_json_response(
                "Search results",
                json!({
                    "query": "widget",
                    "type": "all",
                    "availability": "all",
                    "page": 1,
                    "page_size": 25,
                    "total": 1,
                    "results": [{
                        "display_label": "Widget",
                        "resource_key": "widget",
                        "route": "team/catalog",
                        "index": "team/catalog",
                        "ecosystem": "pypi",
                        "type_label": "project",
                        "type": "cached",
                        "available": true,
                        "summary": "An indexed artifact.",
                    }],
                }),
            ),
        )
        .response(
            "400",
            api_json_response(
                "Invalid search parameters",
                json!({"error": "invalid resource source type"}),
            ),
        )
        .response(
            "403",
            api_json_response(
                "Pattern search without operator authority",
                json!({"error": "pattern search requires operator authority"}),
            ),
        );
    if scoped {
        operation = operation.parameter(route_param());
    }
    operation
}
