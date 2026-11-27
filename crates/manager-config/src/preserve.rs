//! Keeping settings this build does not know.
//!
//! Serde ignores fields it has no member for, so a build that predates a
//! setting (or comes from a branch without it) would drop that setting on
//! its next save: it loads config.json, keeps only what it knows, and
//! writes that back. On 2026-11-26 a build without tiered storage
//! dropped `model_storage` that way, and the cold tier vanished from
//! the app that did know it.
//!
//! [`carry_unknown`] runs at save time against the file being replaced.
//! A field is *unknown* when it is in the file but not in what this
//! build reads from the file (`migrate` then serialize). Unknown fields
//! are carried into the new file at the same place: object members by
//! key, array elements by their `"id"`, or by index when the array
//! kept its length. Known fields are never carried, so a setting this
//! build removed on purpose (a deleted backend, a removed map entry)
//! stays removed.
//!
//! Invariant: carrying never changes what this build reads. Every carried
//! field is checked by re-reading the merged file; one that would change
//! the reading (say, a legacy key that `migrate` would apply over the
//! current value) is dropped instead.

use serde_json::{Map, Value};

use crate::migrate;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    Key(String),
    /// An array element matched by its `"id"` string.
    Id(String),
    /// An array element matched by position; the array's length on disk.
    Index(usize, usize),
}

#[derive(Debug, Clone)]
struct Unknown {
    parent: Vec<Seg>,
    key: String,
    value: Value,
}

impl Unknown {
    fn display(&self) -> String {
        let mut s = String::new();
        for seg in &self.parent {
            match seg {
                Seg::Key(k) => {
                    if !s.is_empty() {
                        s.push('.');
                    }
                    s.push_str(k);
                }
                Seg::Id(id) => s.push_str(&format!("[id={id}]")),
                Seg::Index(i, _) => s.push_str(&format!("[{i}]")),
            }
        }
        if !s.is_empty() {
            s.push('.');
        }
        s.push_str(&self.key);
        s
    }
}

/// What this build reads from `raw`: migrated, then serialized. `None`
/// when `raw` does not load.
fn reading(raw: &Value) -> Option<Value> {
    let cfg = migrate(raw.clone()).ok()?;
    serde_json::to_value(cfg).ok()
}

fn element_id(v: &Value) -> Option<&str> {
    v.get("id")?.as_str()
}

fn collect(disk: &Value, known: &Value, path: &mut Vec<Seg>, out: &mut Vec<Unknown>) {
    match (disk, known) {
        (Value::Object(d), Value::Object(k)) => {
            for (key, dv) in d {
                match k.get(key) {
                    None => out.push(Unknown {
                        parent: path.clone(),
                        key: key.clone(),
                        value: dv.clone(),
                    }),
                    Some(kv) => {
                        path.push(Seg::Key(key.clone()));
                        collect(dv, kv, path, out);
                        path.pop();
                    }
                }
            }
        }
        (Value::Array(d), Value::Array(k)) => {
            for (i, dv) in d.iter().enumerate() {
                if let Some(id) = element_id(dv) {
                    if let Some(kv) = k.iter().find(|kv| element_id(kv) == Some(id)) {
                        path.push(Seg::Id(id.to_string()));
                        collect(dv, kv, path, out);
                        path.pop();
                    }
                } else if d.len() == k.len() {
                    path.push(Seg::Index(i, d.len()));
                    collect(dv, &k[i], path, out);
                    path.pop();
                }
            }
        }
        _ => {}
    }
}

fn locate<'a>(v: &'a mut Value, path: &[Seg]) -> Option<&'a mut Map<String, Value>> {
    let mut cur = v;
    for seg in path {
        cur = match seg {
            Seg::Key(k) => cur.as_object_mut()?.get_mut(k)?,
            Seg::Id(id) => cur
                .as_array_mut()?
                .iter_mut()
                .find(|e| element_id(e) == Some(id.as_str()))?,
            Seg::Index(i, len) => {
                let a = cur.as_array_mut()?;
                if a.len() != *len {
                    return None;
                }
                a.get_mut(*i)?
            }
        };
    }
    cur.as_object_mut()
}

/// Insert `u` into `v`; false when its parent is gone or the key is
/// already there.
fn insert(v: &mut Value, u: &Unknown) -> bool {
    match locate(v, &u.parent) {
        Some(obj) if !obj.contains_key(&u.key) => {
            obj.insert(u.key.clone(), u.value.clone());
            true
        }
        _ => false,
    }
}

/// `fresh` (the config about to be written, serialized) with the fields
/// of `on_disk` (the file it replaces) that this build does not know.
/// Returns the merged value and the carried fields' paths
/// (`gateway.openrouter.overflow`, `backends[id=coder].flags_advanced`);
/// an empty list means `fresh` is returned unchanged.
pub(crate) fn carry_unknown(on_disk: &Value, fresh: &Value) -> (Value, Vec<String>) {
    let Some(known) = reading(on_disk) else {
        return (fresh.clone(), Vec::new());
    };
    let mut unknowns = Vec::new();
    collect(on_disk, &known, &mut Vec::new(), &mut unknowns);
    if unknowns.is_empty() {
        return (fresh.clone(), Vec::new());
    }
    let Some(target) = reading(fresh) else {
        return (fresh.clone(), Vec::new());
    };
    // Fast path: all at once.
    let mut all = fresh.clone();
    let applied: Vec<&Unknown> = unknowns.iter().filter(|u| insert(&mut all, u)).collect();
    if applied.is_empty() {
        return (fresh.clone(), Vec::new());
    }
    if reading(&all).as_ref() == Some(&target) {
        return (all, applied.iter().map(|u| u.display()).collect());
    }
    // One at a time, keeping only those that leave the reading alone.
    let mut merged = fresh.clone();
    let mut carried = Vec::new();
    for u in applied {
        let mut trial = merged.clone();
        if insert(&mut trial, u) && reading(&trial).as_ref() == Some(&target) {
            merged = trial;
            carried.push(u.display());
        }
    }
    (merged, carried)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppConfig;
    use serde_json::json;

    fn base() -> Value {
        let mut cfg = AppConfig::default_config();
        cfg.backends = crate::test_fixtures::working_config().backends;
        serde_json::to_value(cfg).unwrap()
    }

    #[test]
    fn nothing_unknown_is_a_no_op() {
        let v = base();
        let (out, carried) = carry_unknown(&v, &v);
        assert!(carried.is_empty());
        assert_eq!(out, v);
    }

    #[test]
    fn carries_unknown_fields_at_every_level() {
        let fresh = base();
        let mut disk = fresh.clone();
        let id = disk["backends"][0]["id"].as_str().unwrap().to_string();
        disk["future_section"] = json!({"cold": ["E:\\x"]});
        disk["admin_api"]["future_bind"] = json!("all");
        disk["backends"][0]["future_flag"] = json!(true);
        let (out, carried) = carry_unknown(&disk, &fresh);
        assert_eq!(out["future_section"], json!({"cold": ["E:\\x"]}));
        assert_eq!(out["admin_api"]["future_bind"], json!("all"));
        assert_eq!(out["backends"][0]["future_flag"], json!(true));
        assert!(carried.contains(&"future_section".to_string()));
        assert!(carried.contains(&"admin_api.future_bind".to_string()));
        assert!(carried.contains(&format!("backends[id={id}].future_flag")));
    }

    #[test]
    fn array_elements_follow_their_id() {
        let fresh = base();
        let mut disk = fresh.clone();
        disk["backends"][1]["future_flag"] = json!(7);
        let id1 = disk["backends"][1]["id"].clone();
        // The new file lists the backends in the other order.
        let mut reordered = fresh.clone();
        reordered["backends"].as_array_mut().unwrap().reverse();
        let (out, _) = carry_unknown(&disk, &reordered);
        let b = out["backends"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["id"] == id1)
            .unwrap();
        assert_eq!(b["future_flag"], json!(7));
    }

    #[test]
    fn a_removed_element_takes_its_unknown_fields_with_it() {
        let fresh = base();
        let mut disk = fresh.clone();
        disk["backends"][0]["future_flag"] = json!(true);
        let mut removed = fresh.clone();
        removed["backends"].as_array_mut().unwrap().remove(0);
        let (out, carried) = carry_unknown(&disk, &removed);
        assert!(carried.is_empty());
        assert_eq!(out, removed);
    }

    #[test]
    fn known_fields_are_never_carried() {
        // A field this build knows and the new config changed (or a map
        // entry it removed) is the new config's call.
        let fresh = base();
        let mut disk = fresh.clone();
        disk["admin_api"]["port"] = json!(1);
        disk["updates"]["components"]["stale-component"] =
            json!({"pinned_version": null, "auto_update": false, "last_checked": null});
        let (out, carried) = carry_unknown(&disk, &fresh);
        assert!(carried.is_empty(), "{carried:?}");
        assert_eq!(out, fresh);
    }

    #[test]
    fn an_unreadable_file_carries_nothing() {
        let fresh = base();
        let (out, carried) = carry_unknown(&json!([1, 2]), &fresh);
        assert!(carried.is_empty());
        assert_eq!(out, fresh);
    }

    #[test]
    fn the_real_incident_round_trips() {
        // The console build's save dropped these; a build with this fix
        // that does not know them keeps them.
        let fresh = base();
        let mut disk = fresh.clone();
        disk["model_storage_v9"] = json!({
            "cold_tiers": [{"path": "E:\\llm-models", "label": "Data1"}],
            "keep_hot": ["qwen_qwen3-5-0-8b-q8_0"]
        });
        disk["gateway"]["openrouter"]["overflow_v9"] = json!({"enabled": false, "queue_depth": 8});
        let (out, carried) = carry_unknown(&disk, &fresh);
        assert_eq!(out["model_storage_v9"], disk["model_storage_v9"]);
        assert_eq!(
            out["gateway"]["openrouter"]["overflow_v9"],
            disk["gateway"]["openrouter"]["overflow_v9"]
        );
        assert_eq!(carried.len(), 2);
        // And the build still reads exactly what it would have.
        assert_eq!(reading(&out), reading(&fresh));
    }
}
