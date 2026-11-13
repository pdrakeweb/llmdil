//! Structured-output schema for the consultation (design §3
//! "Request shape", Phase 3 of §7).
//!
//! v1 relied on the salvage parser alone because llama.cpp's JSON
//! modes vary by build. Phase 3 additionally asks the server to
//! constrain generation to the plan contract (§4) with an
//! OpenAI-style `response_format: {"type": "json_schema", ...}`,
//! which llama-server turns into a sampling grammar. The salvage
//! parser stays the reader of record either way: a build that
//! rejects the parameter is retried without it (the supervisor's
//! streaming client), and a build that accepts but ignores it
//! produces prose the parser already handles.
//!
//! The schema is derived from the parser's own expectations, not
//! written beside them: the per-kind `change` fields come from
//! [`crate::parse::change_fields`] (the list `normalize_change`
//! enforces), each field's JSON type from one table here, and the
//! required fields per kind mirror what `normalize_change` drops
//! an item for lacking. The tests below assert that a minimal
//! schema-conformant item of every kind survives the parser
//! un-dropped and un-demoted, so the two cannot drift apart.

use serde_json::{json, Map, Value};

use crate::experiment::MAX_EXPERIMENTS;
use crate::parse::{change_fields, MAX_ITEMS};
use crate::plan::PlanKind;

/// The kinds the consultant may author (design §4's table). The
/// app-built `model.download` is deliberately absent: plan
/// building chains it for experiment provisioning; the model
/// never proposes it.
pub const AUTHORED_KINDS: [PlanKind; 15] = [
    PlanKind::BackendCreate,
    PlanKind::BackendDelete,
    PlanKind::BackendSetContext,
    PlanKind::BackendSetFlags,
    PlanKind::BackendSetOffload,
    PlanKind::BackendSetPlacement,
    PlanKind::BackendSetModel,
    PlanKind::BackendSetEnabled,
    PlanKind::GroupSetMembers,
    PlanKind::GroupSetStrategy,
    PlanKind::GroupSetFallbacks,
    PlanKind::GroupCreate,
    PlanKind::GroupDelete,
    PlanKind::ModelAdd,
    PlanKind::Advice,
];

/// Routing strategies the parser admits (`normalize_change`).
const STRATEGIES: [&str; 3] = ["simple-shuffle", "least-busy", "latency-based"];

/// Split modes the parser admits for `backend.set_placement`.
const SPLIT_MODES: [&str; 3] = ["none", "layer", "row"];

/// The JSON-schema type of one `change` field. Every field any
/// kind admits must have an entry; a missing one is a programming
/// error caught by the tests (the schema builder panics).
fn field_schema(field: &str) -> Value {
    match field {
        "model_id" | "group" | "source_url" | "quant" | "sha256" | "text" => {
            json!({ "type": "string" })
        }
        "port" => json!({ "type": "integer", "minimum": 1, "maximum": 65535 }),
        "n_ctx" | "n_batch" | "offload_layers" | "moe_cache_mib" => {
            json!({ "type": "integer", "minimum": 0 })
        }
        "params_b" => json!({ "type": "number", "exclusiveMinimum": 0 }),
        "tensor_split" => json!({ "type": "array", "items": { "type": "number" } }),
        "split_mode" => json!({ "type": "string", "enum": SPLIT_MODES }),
        "enabled" => json!({ "type": "boolean" }),
        "members" | "fallbacks" => json!({ "type": "array", "items": { "type": "string" } }),
        "strategy" => json!({ "type": "string", "enum": STRATEGIES }),
        other => panic!("change field '{other}' has no schema type"),
    }
}

/// The `change` fields `normalize_change` drops an item for
/// lacking. `backend.set_flags` needs at least one of its two
/// fields: see [`change_schema`], which expresses that as an
/// `anyOf` (llama-server's grammar converter supports `anyOf`;
/// `minProperties` is not reliably supported across builds).
fn required_change_fields(kind: PlanKind) -> &'static [&'static str] {
    match kind {
        PlanKind::BackendCreate | PlanKind::BackendSetModel => &["model_id"],
        PlanKind::BackendSetContext => &["n_ctx"],
        PlanKind::BackendSetOffload => &["offload_layers"],
        PlanKind::BackendSetPlacement => &["tensor_split"],
        PlanKind::BackendSetEnabled => &["enabled"],
        PlanKind::GroupSetMembers | PlanKind::GroupCreate => &["members"],
        PlanKind::GroupSetStrategy => &["strategy"],
        PlanKind::GroupSetFallbacks => &["fallbacks"],
        PlanKind::ModelAdd => &["source_url"],
        PlanKind::Advice => &["text"],
        PlanKind::BackendSetFlags
        | PlanKind::BackendDelete
        | PlanKind::GroupDelete
        | PlanKind::ModelDownload => &[],
    }
}

/// The `change` object schema for one kind.
fn change_schema(kind: PlanKind) -> Value {
    let mut change_props = Map::new();
    for field in change_fields(kind) {
        change_props.insert((*field).to_string(), field_schema(field));
    }
    let shape = |required: &[&str]| {
        json!({
            "type": "object",
            "properties": Value::Object(change_props.clone()),
            "required": required,
            "additionalProperties": false,
        })
    };
    if kind == PlanKind::BackendSetFlags {
        // At least one of the two fields (the parser drops an
        // empty set_flags change).
        return json!({ "anyOf": [shape(&["n_ctx"]), shape(&["n_batch"])] });
    }
    shape(required_change_fields(kind))
}

/// One `anyOf` branch: an item of exactly this kind.
fn item_schema(kind: PlanKind) -> Value {
    let change = change_schema(kind);
    // Every executable kind names its target (the parser drops
    // one without); advice may or may not.
    let required: Vec<&str> = if kind == PlanKind::Advice {
        vec![
            "kind",
            "change",
            "rationale",
            "expected_effect",
            "confidence",
        ]
    } else {
        vec![
            "kind",
            "target",
            "change",
            "rationale",
            "expected_effect",
            "confidence",
        ]
    };
    json!({
        "type": "object",
        "properties": {
            "kind": { "type": "string", "const": kind.as_str() },
            "target": { "type": "string" },
            "change": change,
            "rationale": { "type": "string" },
            "expected_effect": { "type": "string" },
            "confidence": { "type": "string", "enum": ["high", "medium", "low"] },
        },
        "required": required,
        "additionalProperties": false,
    })
}

/// The experiments sibling array's entry (addendum A.3), as
/// `parse_experiments` reads it.
fn experiment_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "candidate_model_id": { "type": "string" },
            "flags": {
                "type": "object",
                "properties": {
                    "n_ctx": field_schema("n_ctx"),
                    "offload_layers": field_schema("offload_layers"),
                    "tensor_split": field_schema("tensor_split"),
                    "moe_cache_mib": field_schema("moe_cache_mib"),
                },
                "additionalProperties": false,
            },
            "serves": { "type": "string" },
            "hypothesis": { "type": "string" },
            "predicted": {
                "type": "object",
                "properties": {
                    "predicted_per_second": { "type": "number" },
                    "fit": { "type": "string" },
                },
                "additionalProperties": false,
            },
            "requires_stopping": { "type": "array", "items": { "type": "string" } },
            "run_eval": { "type": "boolean" },
        },
        "required": ["candidate_model_id", "serves"],
        "additionalProperties": false,
    })
}

/// The plan contract (§4) as a JSON schema.
pub fn plan_json_schema() -> Value {
    let branches: Vec<Value> = AUTHORED_KINDS.iter().map(|k| item_schema(*k)).collect();
    json!({
        "type": "object",
        "properties": {
            "summary": { "type": "string" },
            "items": {
                "type": "array",
                "items": { "anyOf": branches },
                "maxItems": MAX_ITEMS,
            },
            "experiments": {
                "type": "array",
                "items": experiment_schema(),
                "maxItems": MAX_EXPERIMENTS,
            },
        },
        "required": ["summary", "items"],
        "additionalProperties": false,
    })
}

/// The `response_format` request field carrying the schema, in
/// the OpenAI `json_schema` form llama-server's
/// `/v1/chat/completions` accepts (it converts the schema to a
/// sampling grammar). No `strict` flag: strict mode's
/// all-properties-required rule does not fit a contract with
/// optional fields, and llama-server does not need it.
pub fn plan_response_format() -> Value {
    json!({
        "type": "json_schema",
        "json_schema": {
            "name": "optimization_plan",
            "schema": plan_json_schema(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    use chrono::Utc;
    use manager_config::AppConfig;

    use crate::parse::parse_plan;
    use crate::plan::Decision;

    /// A sample value conforming to one field schema.
    fn sample(field_schema: &Value) -> Value {
        if let Some(e) = field_schema.get("enum").and_then(|e| e.as_array()) {
            return e[0].clone();
        }
        match field_schema["type"].as_str().unwrap() {
            "string" => json!("https://example.invalid/m.gguf"),
            "integer" => json!(4096),
            "number" => json!(1.0),
            "boolean" => json!(true),
            "array" => match field_schema["items"]["type"].as_str().unwrap() {
                "number" => json!([1.0]),
                _ => json!(["tool-runner"]),
            },
            other => panic!("no sample for {other}"),
        }
    }

    /// The minimal item a schema branch admits: required fields
    /// only, sample values from their schemas.
    fn minimal_item(branch: &Value) -> Value {
        if let Some(alts) = branch.get("anyOf").and_then(|a| a.as_array()) {
            return minimal_item(&alts[0]);
        }
        let mut out = Map::new();
        let props = branch["properties"].as_object().unwrap();
        for req in branch["required"].as_array().unwrap() {
            let key = req.as_str().unwrap();
            let schema = &props[key];
            let v = match key {
                "kind" => schema["const"].clone(),
                "target" => json!("tool-runner"),
                "change" => minimal_item(schema),
                _ => sample(schema),
            };
            out.insert(key.to_string(), v);
        }
        Value::Object(out)
    }

    fn parse(response: &str) -> crate::plan::Plan {
        parse_plan(
            response,
            &AppConfig::default_config(),
            "planner",
            "hash",
            json!({}),
            &HashSet::new(),
            "",
            Utc::now(),
        )
        .unwrap()
    }

    #[test]
    fn schema_has_one_branch_per_authored_kind() {
        let schema = plan_json_schema();
        let branches = schema["properties"]["items"]["items"]["anyOf"]
            .as_array()
            .unwrap();
        let kinds: Vec<&str> = branches
            .iter()
            .map(|b| b["properties"]["kind"]["const"].as_str().unwrap())
            .collect();
        assert_eq!(kinds.len(), AUTHORED_KINDS.len());
        for k in AUTHORED_KINDS {
            assert!(kinds.contains(&k.as_str()), "{} missing", k.as_str());
            // Every kind the schema offers is one the parser knows.
            assert_eq!(PlanKind::parse(k.as_str()), Some(k));
        }
        assert!(!kinds.contains(&"model.download"));
        assert_eq!(schema["required"], json!(["summary", "items"]));
        let rf = plan_response_format();
        assert_eq!(rf["type"], json!("json_schema"));
        assert_eq!(rf["json_schema"]["schema"], schema);
    }

    /// Every field the parser admits for a kind is in that kind's
    /// branch with a type, and nothing else is: the schema's
    /// `change` properties are exactly `change_fields`.
    #[test]
    fn change_properties_match_the_parser_allowlist() {
        let schema = plan_json_schema();
        for branch in schema["properties"]["items"]["items"]["anyOf"]
            .as_array()
            .unwrap()
        {
            let kind =
                PlanKind::parse(branch["properties"]["kind"]["const"].as_str().unwrap()).unwrap();
            let change = &branch["properties"]["change"];
            let shapes: Vec<&Value> = match change.get("anyOf").and_then(|a| a.as_array()) {
                Some(alts) => alts.iter().collect(),
                None => vec![change],
            };
            for shape in shapes {
                let props = shape["properties"].as_object().unwrap();
                let mut got: Vec<&str> = props.keys().map(|k| k.as_str()).collect();
                let mut want: Vec<&str> = change_fields(kind).to_vec();
                got.sort_unstable();
                want.sort_unstable();
                assert_eq!(got, want, "{}", kind.as_str());
                for (k, v) in props {
                    assert!(v.get("type").is_some(), "{k} has no type");
                }
                assert_eq!(shape["additionalProperties"], json!(false));
            }
        }
    }

    /// A minimal schema-conformant item of every kind parses
    /// without being dropped as invalid or demoted to advice — the
    /// schema never constrains the model into an answer the parser
    /// then throws away.
    #[test]
    fn minimal_conformant_item_of_every_kind_survives_the_parser() {
        let schema = plan_json_schema();
        for branch in schema["properties"]["items"]["items"]["anyOf"]
            .as_array()
            .unwrap()
        {
            let item = minimal_item(branch);
            let kind = item["kind"].as_str().unwrap().to_string();
            let response = json!({ "summary": "s", "items": [item] }).to_string();
            let plan = parse(&response);
            assert_eq!(plan.dropped_invalid, 0, "{kind} dropped: {response}");
            // Model-referencing kinds may be excluded at plan build
            // (grounding — the model id is not registered); that
            // is a verdict on the reference, not on the shape.
            let survived = plan
                .items
                .iter()
                .any(|i| i.kind.as_str() == kind && i.decision != Decision::Accepted)
                || plan.excluded_items.iter().any(|e| e.kind.as_str() == kind);
            assert!(survived, "{kind} did not survive as itself: {response}");
        }
    }

    #[test]
    fn minimal_experiment_survives_the_parser() {
        let response = json!({
            "summary": "s",
            "items": [],
            "experiments": [minimal_item(&experiment_schema())],
        })
        .to_string();
        let plan = parse(&response);
        assert_eq!(plan.experiments_dropped_invalid, 0);
        assert_eq!(plan.experiments.len(), 1);
    }
}
