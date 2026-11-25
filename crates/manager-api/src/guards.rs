//! Per-operation request rules the registry's declaration cannot say:
//! checks that must answer before an asynchronous operation is started
//! (a `202` cannot become a `409`), and the typing of path parameters.

use axum::http::StatusCode;
use axum::response::Response;
use serde_json::Value;

use manager_app::registry::{request_params, Op, OpSpec};

use crate::http::error_response_raw;
use crate::ApiCtx;

/// A path parameter's value: the URL's text, or the number it spells
/// when the operation declares an integer there.
pub fn path_value(spec: &OpSpec, name: &str, raw: &str) -> Value {
    let integer = request_params(spec.op)
        .iter()
        .any(|p| p.name == name && p.json_schema["type"] == "integer");
    match raw.parse::<u64>() {
        Ok(n) if integer => Value::from(n),
        _ => Value::String(raw.to_string()),
    }
}

/// The response that refuses this request, or `None` to go on.
pub fn refuse(ctx: &ApiCtx, spec: &OpSpec, args: &Value) -> Option<Response> {
    match spec.op {
        Op::OptimizeConsult => {
            let replace = args.get("replace").and_then(Value::as_bool);
            manager_app::consult_replace_refusal(&ctx.app, replace).map(|why| {
                error_response_raw(
                    StatusCode::CONFLICT,
                    "PLAN_EXISTS",
                    "PlanExists",
                    &why,
                    None,
                )
            })
        }
        // A misspelled switch would otherwise read as its default (on).
        Op::NotificationsSetConfig => {
            let known = serde_json::to_value(manager_config::NotificationConfig::default()).ok()?;
            let unknown = args["config"]
                .as_object()?
                .keys()
                .find(|k| known.get(k.as_str()).is_none())?;
            Some(error_response_raw(
                StatusCode::BAD_REQUEST,
                "INVALID_INPUT",
                "InvalidInput",
                &format!("unknown field `{unknown}` for {}", spec.name),
                None,
            ))
        }
        _ => None,
    }
}
