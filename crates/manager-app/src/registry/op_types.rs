//! Typed request envelopes and the artifacts generated from them.
//!
//! [`for_each_operation!`](crate::for_each_operation) declares every
//! operation's parameters as `name: Type`. From that one declaration
//! the registry generates (see `gen_requests!` in the parent module):
//!
//! - one request struct per operation in [`super::requests`]
//!   (`requests::StartBackend`, …), whose fields are the declared
//!   parameters under their declared (snake_case) names. `dispatch`
//!   builds the struct from the argument object and passes its fields
//!   to the operation, so the wire format is the one it always was;
//! - [`request_params`], the per-parameter type description used to
//!   render the two checked-in artifacts below.
//!
//! Generated artifacts (checked in; the tests in this module fail
//! when either is stale):
//!
//! - `ui/src/generated/ops.ts` — the `OpName` union and one
//!   `readonly` argument interface per operation, keyed **camelCase**:
//!   that is the key the desktop shell binds each snake_case Rust
//!   parameter from (Tauri 2's command-argument convention).
//! - `ui/src/generated/ops.schema.json` — one JSON Schema per
//!   operation describing the **snake_case** argument object that
//!   `dispatch` and the admin API accept (properties, types,
//!   required).
//!
//! Regenerate both after changing a declaration:
//!
//! ```text
//! UPDATE_GENERATED=1 cargo test -p manager-app --lib generated_
//! ```

use serde_json::{json, Map, Value};

use super::{Op, OP_SPECS};

/// How one declared parameter type is described in the generated
/// artifacts. Implemented for every type a registry declaration
/// uses; a declaration with a new parameter type fails to build
/// until the type is described here.
pub trait ParamType {
    /// Whether the argument must be present. `false` only for
    /// `Option<T>` (a missing key reads as `None`). A `Value`
    /// document parameter counts as required: the operation needs
    /// the document even though a missing key reaches it as `null`.
    const REQUIRED: bool = true;
    /// The JSON Schema of the argument's value.
    fn json_schema() -> Value;
    /// The TypeScript type of the argument's value.
    fn ts_type() -> String;
    /// A representative value (contract tests only).
    fn sample() -> Value;
}

macro_rules! scalar_param {
    ($t:ty, $schema:expr, $ts:expr, $sample:expr) => {
        impl ParamType for $t {
            fn json_schema() -> Value {
                $schema
            }
            fn ts_type() -> String {
                $ts.to_string()
            }
            fn sample() -> Value {
                $sample
            }
        }
    };
}

scalar_param!(String, json!({"type": "string"}), "string", json!("s"));
scalar_param!(bool, json!({"type": "boolean"}), "boolean", json!(true));
scalar_param!(
    u16,
    json!({"type": "integer", "minimum": 0, "maximum": u16::MAX}),
    "number",
    json!(1)
);
scalar_param!(
    u32,
    json!({"type": "integer", "minimum": 0, "maximum": u32::MAX}),
    "number",
    json!(1)
);
scalar_param!(
    u64,
    json!({"type": "integer", "minimum": 0}),
    "number",
    json!(1)
);
scalar_param!(
    usize,
    json!({"type": "integer", "minimum": 0}),
    "number",
    json!(1)
);
scalar_param!(i64, json!({"type": "integer"}), "number", json!(1));
scalar_param!(f32, json!({"type": "number"}), "number", json!(0.5));
scalar_param!(
    Value,
    json!({"description": "Any JSON document (validated by the operation)."}),
    "unknown",
    json!({"k": 1})
);
scalar_param!(
    manager_config::McpServerConfig,
    json!({"type": "object", "description": "manager_config::McpServerConfig"}),
    "Record<string, unknown>",
    serde_json::to_value(manager_config::McpServerConfig::default())
        .expect("default MCP server config serializes")
);
scalar_param!(
    manager_config::NotificationConfig,
    json!({"type": "object", "description": "manager_config::NotificationConfig"}),
    "Record<string, unknown>",
    json!({"enabled": true})
);

/// The MXC mode's wire names, read off its serde form so a renamed
/// or added variant changes the generated artifacts (the exhaustive
/// match below stops compiling on an added variant).
fn mxc_mode_names() -> Vec<String> {
    use manager_config::MxcMode;
    const _EXHAUSTIVE: fn(MxcMode) = |m| match m {
        MxcMode::Learning | MxcMode::Enforced | MxcMode::Disabled => {}
    };
    let all = [MxcMode::Learning, MxcMode::Enforced, MxcMode::Disabled];
    all.iter()
        .map(|m| {
            serde_json::to_value(m)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .expect("MxcMode serializes as a string")
        })
        .collect()
}

impl ParamType for manager_config::MxcMode {
    fn json_schema() -> Value {
        json!({"type": "string", "enum": mxc_mode_names()})
    }
    fn ts_type() -> String {
        mxc_mode_names()
            .iter()
            .map(|n| format!("\"{n}\""))
            .collect::<Vec<_>>()
            .join(" | ")
    }
    fn sample() -> Value {
        json!(mxc_mode_names()[0])
    }
}

impl ParamType for manager_config::SupervisionMode {
    fn json_schema() -> Value {
        json!({"type": "string", "enum": ["process", "service"]})
    }
    fn ts_type() -> String {
        "\"process\" | \"service\"".to_string()
    }
    fn sample() -> Value {
        // Exhaustive: an added mode stops compiling here.
        use manager_config::SupervisionMode as M;
        const _EXHAUSTIVE: fn(M) = |m| match m {
            M::Process | M::Service => {}
        };
        json!(M::Process)
    }
}

impl<T: ParamType> ParamType for Vec<T> {
    fn json_schema() -> Value {
        json!({"type": "array", "items": T::json_schema()})
    }
    fn ts_type() -> String {
        let inner = T::ts_type();
        if inner.contains(' ') {
            format!("({inner})[]")
        } else {
            format!("{inner}[]")
        }
    }
    fn sample() -> Value {
        json!([T::sample()])
    }
}

impl<T: ParamType> ParamType for Option<T> {
    const REQUIRED: bool = false;
    fn json_schema() -> Value {
        json!({"anyOf": [T::json_schema(), {"type": "null"}]})
    }
    fn ts_type() -> String {
        format!("{} | null", T::ts_type())
    }
    fn sample() -> Value {
        T::sample()
    }
}

/// One declared parameter of one operation.
#[derive(Debug, Clone)]
pub struct ParamInfo {
    /// The declared (snake_case) parameter name — the argument key
    /// `dispatch` and the admin API read.
    pub name: &'static str,
    pub required: bool,
    pub json_schema: Value,
    pub ts_type: String,
    pub sample: Value,
}

impl ParamInfo {
    pub(super) fn of<T: ParamType>(name: &'static str) -> Self {
        ParamInfo {
            name,
            required: T::REQUIRED,
            json_schema: T::json_schema(),
            ts_type: T::ts_type(),
            sample: T::sample(),
        }
    }
}

/// The parameters of `op`, in declaration order.
pub fn request_params(op: Op) -> Vec<ParamInfo> {
    super::gen_request_params(op)
}

/// The JSON Schema of `op`'s (snake_case) argument object.
pub fn request_schema(op: Op) -> Value {
    let params = request_params(op);
    let mut properties = Map::new();
    let mut required = Vec::new();
    for p in &params {
        properties.insert(p.name.to_string(), p.json_schema.clone());
        if p.required {
            required.push(Value::from(p.name));
        }
    }
    json!({
        "title": super::spec(op).name,
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

/// The camelCase key the desktop shell binds a snake_case parameter
/// from (Tauri 2's default for command arguments).
pub fn camel_case(snake: &str) -> String {
    let mut out = String::with_capacity(snake.len());
    let mut upper = false;
    for c in snake.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// The TypeScript interface name for an operation's arguments.
fn ts_interface_name(op: Op) -> String {
    format!("{}Args", super::variant_name(op))
}

/// Render `ui/src/generated/ops.schema.json`.
pub fn render_schema_json() -> String {
    let mut ops = Map::new();
    for s in OP_SPECS {
        ops.insert(s.name.to_string(), request_schema(s.op));
    }
    let doc = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "description": "GENERATED from the manager-app operation registry; do not edit. Regenerate: UPDATE_GENERATED=1 cargo test -p manager-app --lib generated_. One schema per operation for its snake_case argument object (dispatch / admin API).",
        "ops": ops,
    });
    let mut text = serde_json::to_string_pretty(&doc).expect("schema serializes");
    text.push('\n');
    text
}

/// Render `ui/src/generated/ops.ts`.
pub fn render_ts() -> String {
    let mut out = String::new();
    out.push_str(
        "// GENERATED from the manager-app operation registry\n\
         // (crates/manager-app/src/registry.rs, for_each_operation!) — do not edit.\n\
         // Regenerate: UPDATE_GENERATED=1 cargo test -p manager-app --lib generated_\n\
         //\n\
         // Argument keys are camelCase: the desktop shell binds each snake_case\n\
         // Rust parameter from its camelCase key. The snake_case argument object\n\
         // the admin API accepts is described in ops.schema.json.\n\n",
    );
    out.push_str("/** Every registered operation name. */\nexport type OpName =\n");
    for s in OP_SPECS {
        out.push_str(&format!("  | \"{}\"\n", s.name));
    }
    out.push_str(";\n\n");
    for s in OP_SPECS {
        out.push_str(&format!("/** Arguments of `{}`. */\n", s.name));
        let params = request_params(s.op);
        if params.is_empty() {
            out.push_str(&format!(
                "export interface {} {{}}\n\n",
                ts_interface_name(s.op)
            ));
            continue;
        }
        out.push_str(&format!(
            "export interface {} {{\n",
            ts_interface_name(s.op)
        ));
        for p in &params {
            let opt = if p.required { "" } else { "?" };
            out.push_str(&format!(
                "  readonly {}{}: {};\n",
                camel_case(p.name),
                opt,
                p.ts_type
            ));
        }
        out.push_str("}\n\n");
    }
    out.push_str("/** Operation name → argument interface. */\nexport interface OpArgs {\n");
    for s in OP_SPECS {
        out.push_str(&format!("  {}: {};\n", s.name, ts_interface_name(s.op)));
    }
    out.push_str("}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::spec;
    use std::path::PathBuf;

    fn generated_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/src/generated")
    }

    /// Compare a checked-in artifact with its rendering; with
    /// `UPDATE_GENERATED=1` rewrite it instead.
    fn check_artifact(file: &str, rendered: &str) {
        let path = generated_dir().join(file);
        if std::env::var_os("UPDATE_GENERATED").is_some() {
            std::fs::create_dir_all(generated_dir()).expect("create ui/src/generated");
            std::fs::write(&path, rendered).expect("write generated artifact");
            return;
        }
        let on_disk = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
            .replace("\r\n", "\n");
        assert!(
            on_disk == rendered,
            "{} is stale — regenerate with `UPDATE_GENERATED=1 cargo test -p manager-app --lib generated_`",
            path.display()
        );
    }

    #[test]
    fn generated_ts_types_are_current() {
        check_artifact("ops.ts", &render_ts());
    }

    #[test]
    fn generated_json_schema_is_current() {
        check_artifact("ops.schema.json", &render_schema_json());
    }

    fn sample_args(op: Op) -> Value {
        let mut m = Map::new();
        for p in request_params(op) {
            m.insert(p.name.to_string(), p.sample);
        }
        Value::Object(m)
    }

    /// Every operation's request struct has exactly the declared
    /// parameters as fields, under the declared names, and parses
    /// the snake_case argument object losslessly.
    #[test]
    fn request_structs_match_op_specs() {
        for s in OP_SPECS {
            let params = request_params(s.op);
            let names: Vec<&str> = params.iter().map(|p| p.name).collect();
            assert_eq!(names, s.params, "{}: param info vs spec", s.name);
            let args = sample_args(s.op);
            let parsed = crate::registry::parse_request(s.op, &args)
                .unwrap_or_else(|e| panic!("{}: sample args rejected: {e}", s.name));
            let obj = parsed.as_object().expect("request serializes as an object");
            let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
            let mut want: Vec<&str> = s.params.to_vec();
            want.sort_unstable();
            let mut got = keys.clone();
            got.sort_unstable();
            assert_eq!(got, want, "{}: request struct fields", s.name);
            // Round trip: the struct's serialization parses back to
            // the same struct.
            let again = crate::registry::parse_request(s.op, &parsed)
                .unwrap_or_else(|e| panic!("{}: round trip rejected: {e}", s.name));
            assert_eq!(again, parsed, "{}: round trip", s.name);
        }
    }

    /// Missing optional arguments read as `None`; a missing required
    /// argument is rejected with the same message `dispatch` has
    /// always produced.
    #[test]
    fn request_structs_keep_argument_semantics() {
        let r = crate::registry::requests::StartBackend::from_args(&json!({"id": "a"}))
            .expect("optional arg may be absent");
        assert_eq!(r.id, "a");
        assert_eq!(r.allow_wddm_overcommit, None);
        let err = crate::registry::requests::TestBackend::from_args(&json!({})).unwrap_err();
        assert_eq!(
            err.to_string(),
            crate::ApiError::InvalidInput(
                "argument `id`: invalid type: null, expected a string".into()
            )
            .to_string()
        );
        // Unknown keys are ignored, as before.
        crate::registry::requests::GetStatus::from_args(&json!({"extra": 1}))
            .expect("no-arg op ignores extra keys");
        // The serde path has the same semantics as from_args.
        let via_serde: crate::registry::requests::StartBackend =
            serde_json::from_value(json!({"id": "b"})).expect("serde path");
        assert_eq!(via_serde.id, "b");
        assert!(
            serde_json::from_value::<crate::registry::requests::TestBackend>(json!({})).is_err()
        );
    }

    #[test]
    fn ts_types_match_op_specs() {
        let ts = render_ts();
        for s in OP_SPECS {
            assert!(
                ts.contains(&format!("  | \"{}\"\n", s.name)),
                "{} missing from OpName",
                s.name
            );
            let iface = ts_interface_name(s.op);
            let start = ts
                .find(&format!("export interface {iface} {{"))
                .unwrap_or_else(|| panic!("no interface {iface}"));
            let body = &ts[start..start + ts[start..].find('}').expect("interface closes")];
            for p in s.params {
                assert!(
                    body.contains(&format!("readonly {}", camel_case(p))),
                    "{iface} lacks {p}"
                );
            }
            assert_eq!(body.matches("readonly ").count(), s.params.len(), "{iface}");
        }
    }

    #[test]
    fn json_schema_matches_op_specs() {
        for s in OP_SPECS {
            let schema = request_schema(s.op);
            let props = schema["properties"].as_object().expect("properties");
            let mut keys: Vec<&str> = props.keys().map(String::as_str).collect();
            keys.sort_unstable();
            let mut want: Vec<&str> = s.params.to_vec();
            want.sort_unstable();
            assert_eq!(keys, want, "{}: schema properties", s.name);
            let required: Vec<&str> = schema["required"]
                .as_array()
                .expect("required")
                .iter()
                .map(|v| v.as_str().expect("name"))
                .collect();
            let want_required: Vec<&str> = request_params(s.op)
                .iter()
                .filter(|p| p.required)
                .map(|p| p.name)
                .collect();
            assert_eq!(required, want_required, "{}: required", s.name);
            assert_eq!(spec(s.op).name, schema["title"]);
        }
    }

    #[test]
    fn camel_case_matches_tauri_binding() {
        assert_eq!(camel_case("allow_wddm_overcommit"), "allowWddmOvercommit");
        assert_eq!(camel_case("n_ctx"), "nCtx");
        assert_eq!(camel_case("id"), "id");
        assert_eq!(camel_case("sha256"), "sha256");
    }
}
