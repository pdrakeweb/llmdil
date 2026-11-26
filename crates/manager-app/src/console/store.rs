//! Console persistence: recipes under `<data_dir>/diagnostics/recipes/`
//! (one JSON document per recipe, like `config.json`) and runs under
//! `<data_dir>/diagnostics/runs/<timestamp>-<recipe>/`:
//!
//! - `run.json` — the record: recipe snapshot, resolved commands and
//!   environment, backend stop/restore outcomes, per-step results,
//!   analysis, telemetry and timings (rewritten as the run advances);
//! - `output.jsonl` — every output line with its sequence number;
//! - `steps/<n>.stdout.log` / `.stderr.log` — raw per-step output;
//! - `export.json` — written on request (`console_run_export`).

use std::path::{Path, PathBuf};

use manager_diag::recipe::valid_name;
use manager_diag::Recipe;
use serde_json::{json, Value};

use crate::ApiError;

/// Largest step output returned inline by a `full` result.
const FULL_OUTPUT_CAP: u64 = 4 * 1024 * 1024;

pub(crate) fn diag_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("diagnostics")
}

pub(crate) fn recipes_dir(data_dir: &Path) -> PathBuf {
    diag_dir(data_dir).join("recipes")
}

pub(crate) fn runs_dir(data_dir: &Path) -> PathBuf {
    diag_dir(data_dir).join("runs")
}

/// A run id is `<yyyymmdd-hhmmss>-<recipe>[-n]`: the charset of a
/// recipe name, so it can never escape the runs directory.
pub(crate) fn valid_run_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

pub(crate) fn run_dir(data_dir: &Path, id: &str) -> Result<PathBuf, ApiError> {
    if !valid_run_id(id) {
        return Err(ApiError::InvalidInput(format!("invalid run id {id:?}")));
    }
    Ok(runs_dir(data_dir).join(id))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ApiError> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn summary(r: &Recipe, error: Option<String>) -> Value {
    json!({
        "name": r.name,
        "title": r.title,
        "description": r.description,
        "builtin": r.builtin,
        "source": r.source,
        "steps": r.steps.len(),
        "variants": r.variant_count(),
        "params": r.params,
        "error": error,
    })
}

/// Built-ins first, then saved recipes by name.
pub(crate) fn list_recipes(data_dir: &Path) -> Vec<Value> {
    let mut out: Vec<Value> = manager_diag::builtins::builtins()
        .iter()
        .map(|r| summary(r, None))
        .collect();
    let mut saved: Vec<(String, Value)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(recipes_dir(data_dir)) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let stem = p
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let entry = match std::fs::read_to_string(&p)
                .map_err(|e| e.to_string())
                .and_then(|t| serde_json::from_str::<Recipe>(&t).map_err(|e| e.to_string()))
            {
                Ok(r) => {
                    let err = r.validate().err().map(|e| e.to_string());
                    summary(&r, err)
                }
                Err(e) => json!({ "name": stem, "builtin": false, "error": e }),
            };
            saved.push((stem, entry));
        }
    }
    saved.sort_by(|a, b| a.0.cmp(&b.0));
    out.extend(saved.into_iter().map(|(_, v)| v));
    out
}

pub(crate) fn load_recipe(data_dir: &Path, name: &str) -> Result<Recipe, ApiError> {
    if !valid_name(name) {
        return Err(ApiError::InvalidInput(format!(
            "invalid recipe name {name:?}"
        )));
    }
    if let Some(r) = manager_diag::builtins::builtin(name) {
        return Ok(r);
    }
    let p = recipes_dir(data_dir).join(format!("{name}.json"));
    let text =
        std::fs::read_to_string(&p).map_err(|_| ApiError::NotFound(format!("recipe {name}")))?;
    let mut r: Recipe = serde_json::from_str(&text)
        .map_err(|e| ApiError::InvalidInput(format!("recipe {name} is unreadable: {e}")))?;
    r.builtin = false;
    Ok(r)
}

pub(crate) fn save_recipe(data_dir: &Path, r: &Recipe, overwrite: bool) -> Result<(), ApiError> {
    r.validate()
        .map_err(|e| ApiError::InvalidInput(e.to_string()))?;
    if manager_diag::builtins::builtin(&r.name).is_some() {
        return Err(ApiError::InvalidInput(format!(
            "{} is a built-in recipe name; save under another name",
            r.name
        )));
    }
    let p = recipes_dir(data_dir).join(format!("{}.json", r.name));
    if p.exists() && !overwrite {
        return Err(ApiError::InvalidInput(format!(
            "recipe {} already exists (update it, or pass overwrite)",
            r.name
        )));
    }
    let mut doc = r.clone();
    doc.builtin = false;
    if doc.source.is_none() {
        doc.source = Some("api".into());
    }
    write_atomic(&p, serde_json::to_string_pretty(&doc)?.as_bytes())
}

pub(crate) fn delete_recipe(data_dir: &Path, name: &str) -> Result<(), ApiError> {
    if !valid_name(name) {
        return Err(ApiError::InvalidInput(format!(
            "invalid recipe name {name:?}"
        )));
    }
    if manager_diag::builtins::builtin(name).is_some() {
        return Err(ApiError::InvalidInput(format!(
            "{name} is built in and cannot be deleted"
        )));
    }
    let p = recipes_dir(data_dir).join(format!("{name}.json"));
    std::fs::remove_file(&p).map_err(|_| ApiError::NotFound(format!("recipe {name}")))?;
    Ok(())
}

/// The recipe a plan/run names: a stored one by `name`, or an inline
/// (reviewed, unsaved) document — exactly one of the two.
pub(crate) fn recipe_arg(
    data_dir: &Path,
    name: Option<String>,
    doc: Option<Value>,
) -> Result<Recipe, ApiError> {
    match (name, doc) {
        (Some(n), None) => load_recipe(data_dir, &n),
        (None, Some(d)) => {
            let r: Recipe = serde_json::from_value(d)
                .map_err(|e| ApiError::InvalidInput(format!("recipe_doc: {e}")))?;
            r.validate()
                .map_err(|e| ApiError::InvalidInput(e.to_string()))?;
            Ok(Recipe {
                builtin: false,
                ..r
            })
        }
        (Some(_), Some(_)) => Err(ApiError::InvalidInput(
            "pass either `recipe` (a saved name) or `recipe_doc`, not both".into(),
        )),
        (None, None) => Err(ApiError::InvalidInput(
            "pass `recipe` (a saved name) or `recipe_doc` (an inline recipe)".into(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

/// A fresh run id for `recipe` (unique within the runs directory).
pub(crate) fn new_run_id(data_dir: &Path, recipe: &str) -> String {
    let base = format!("{}-{recipe}", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let base: String = base.chars().take(88).collect();
    let mut id = base.clone();
    let mut n = 2;
    while runs_dir(data_dir).join(&id).exists() {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

pub(crate) fn write_run(data_dir: &Path, id: &str, record: &Value) -> Result<(), ApiError> {
    let dir = run_dir(data_dir, id)?;
    write_atomic(
        &dir.join("run.json"),
        serde_json::to_string_pretty(record)?.as_bytes(),
    )
}

pub(crate) fn read_run(data_dir: &Path, id: &str) -> Result<Value, ApiError> {
    let dir = run_dir(data_dir, id)?;
    let text = std::fs::read_to_string(dir.join("run.json"))
        .map_err(|_| ApiError::NotFound(format!("run {id}")))?;
    Ok(serde_json::from_str(&text)?)
}

pub(crate) fn list_runs(data_dir: &Path, limit: usize) -> Vec<Value> {
    let mut ids: Vec<String> = std::fs::read_dir(runs_dir(data_dir))
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| valid_run_id(n))
                .collect()
        })
        .unwrap_or_default();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    ids.into_iter()
        .take(limit.clamp(1, 1000))
        .filter_map(|id| {
            let r = read_run(data_dir, &id).ok()?;
            Some(json!({
                "id": id,
                "recipe": r["recipe"]["name"],
                "status": r["status"],
                "started_ms": r["started_ms"],
                "ended_ms": r["ended_ms"],
                "variant": r["variant"],
                "steps": r["steps"].as_array().map(|a| a.len()).unwrap_or(0),
                "caller": r["caller"],
                "headline": r["summary"]["headline"],
            }))
        })
        .collect()
}

fn read_capped(p: &Path) -> Value {
    match std::fs::metadata(p) {
        Ok(m) => {
            let len = m.len();
            let text = if len > FULL_OUTPUT_CAP {
                use std::io::{Read, Seek, SeekFrom};
                let mut f = match std::fs::File::open(p) {
                    Ok(f) => f,
                    Err(_) => return Value::Null,
                };
                let _ = f.seek(SeekFrom::Start(len - FULL_OUTPUT_CAP));
                let mut buf = Vec::new();
                let _ = f.read_to_end(&mut buf);
                format!(
                    "[… {} earlier bytes omitted; the full file is {} …]\n{}",
                    len - FULL_OUTPUT_CAP,
                    p.display(),
                    String::from_utf8_lossy(&buf)
                )
            } else {
                std::fs::read_to_string(p).unwrap_or_default()
            };
            json!(text)
        }
        Err(_) => Value::Null,
    }
}

/// Compact result: the run record without per-sample telemetry or
/// raw bench rows. Full: plus every step's stdout/stderr text (capped
/// at 4 MiB each, the tail kept) and the telemetry samples.
pub(crate) fn run_result(data_dir: &Path, id: &str, full: bool) -> Result<Value, ApiError> {
    let mut r = read_run(data_dir, id)?;
    let dir = run_dir(data_dir, id)?;
    if let Some(steps) = r["steps"].as_array_mut() {
        for s in steps.iter_mut() {
            if full {
                let n = s["index"].as_u64().unwrap_or(0);
                s["stdout"] = read_capped(&dir.join("steps").join(format!("{n:02}.stdout.log")));
                s["stderr"] = read_capped(&dir.join("steps").join(format!("{n:02}.stderr.log")));
            } else {
                if let Some(t) = s.get_mut("telemetry").and_then(Value::as_object_mut) {
                    t.remove("samples");
                }
                if let Some(b) = s
                    .get_mut("analysis")
                    .and_then(|a| a.get_mut("bench"))
                    .and_then(Value::as_object_mut)
                {
                    b.remove("rows");
                }
            }
        }
    }
    if !full {
        if let Some(o) = r.as_object_mut() {
            o.remove("recipe_doc");
        }
    }
    r["full"] = json!(full);
    Ok(r)
}

pub(crate) fn export_run(data_dir: &Path, id: &str) -> Result<Value, ApiError> {
    let r = run_result(data_dir, id, true)?;
    let dir = run_dir(data_dir, id)?;
    let path = dir.join("export.json");
    write_atomic(&path, serde_json::to_string_pretty(&r)?.as_bytes())?;
    Ok(json!({ "path": path.display().to_string(), "export": r }))
}
