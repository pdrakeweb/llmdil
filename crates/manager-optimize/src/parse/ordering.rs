//! Dependency ordering (plan-build pass): entities each item creates,
//! requires, and removes, and the order-preserving topological sort.

use serde_json::Value;

use crate::plan::{PlanItem, PlanKind};

/// The entities one item creates, requires, and removes, for
/// the dependency-ordering pass. Entities are keyed like the
/// validation simulation's write scopes: `m:<model id>`,
/// `b:<backend id>`, `g:<group name>`.
pub(super) fn item_entities(item: &PlanItem) -> (Vec<String>, Vec<String>, Vec<String>) {
    let target = item.target.clone().unwrap_or_default();
    let mut creates: Vec<String> = Vec::new();
    let mut requires: Vec<String> = Vec::new();
    let mut removes: Vec<String> = Vec::new();
    // Group member/fallback entries that name backends (a
    // `host:port` member is an external endpoint, and
    // "openrouter" is the cloud fallback — neither is a backend
    // this plan can create or delete).
    let backend_refs = |change: &Value, key: &str| -> Vec<String> {
        change
            .get(key)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| !s.contains(':') && *s != "openrouter")
                    .map(|s| format!("b:{s}"))
                    .collect()
            })
            .unwrap_or_default()
    };
    match item.kind {
        PlanKind::ModelAdd => creates.push(format!("m:{target}")),
        PlanKind::ModelDownload => requires.push(format!("m:{target}")),
        PlanKind::BackendCreate => {
            creates.push(format!("b:{target}"));
            if let Some(m) = item.change.get("model_id").and_then(|v| v.as_str()) {
                requires.push(format!("m:{m}"));
            }
        }
        PlanKind::BackendDelete => {
            requires.push(format!("b:{target}"));
            removes.push(format!("b:{target}"));
        }
        PlanKind::BackendSetModel => {
            requires.push(format!("b:{target}"));
            if let Some(m) = item.change.get("model_id").and_then(|v| v.as_str()) {
                requires.push(format!("m:{m}"));
            }
        }
        PlanKind::BackendSetContext
        | PlanKind::BackendSetFlags
        | PlanKind::BackendSetOffload
        | PlanKind::BackendSetPlacement
        | PlanKind::BackendSetEnabled => {
            requires.push(format!("b:{target}"));
        }
        PlanKind::GroupCreate => {
            creates.push(format!("g:{target}"));
            requires.extend(backend_refs(&item.change, "members"));
            requires.extend(backend_refs(&item.change, "fallbacks"));
        }
        PlanKind::GroupSetMembers => {
            requires.push(format!("g:{target}"));
            requires.extend(backend_refs(&item.change, "members"));
        }
        PlanKind::GroupSetFallbacks => {
            requires.push(format!("g:{target}"));
            requires.extend(backend_refs(&item.change, "fallbacks"));
        }
        PlanKind::GroupSetStrategy => {
            requires.push(format!("g:{target}"));
        }
        PlanKind::GroupDelete => {
            requires.push(format!("g:{target}"));
            removes.push(format!("g:{target}"));
        }
        PlanKind::Advice => {}
    }
    (creates, requires, removes)
}

/// Dependency ordering (plan-build pass): reorder items so a
/// prerequisite always precedes the item that needs it — a
/// model registration before any backend or download naming
/// the model, a backend creation before routing items naming
/// it, routing updates before a delete that removes what they
/// reference, deletes last among their dependents.
///
/// Edges derive from [`item_entities`]: creator → referencer,
/// and referencer → remover (an item that uses an entity runs
/// before the item that removes it). Placement is a depth-first
/// walk in emitted order: an item is emitted only after its
/// in-plan predecessors, and a predecessor is pulled earlier
/// only when an edge forces it — so where no edge applies, the
/// model's emitted order (its intent) is preserved exactly.
/// A dependency cycle cannot be ordered; it is broken at the
/// point of detection, keeping every item (validation reports
/// the genuine problem on the affected rows).
pub(super) fn order_items_by_dependency(items: Vec<PlanItem>) -> Vec<PlanItem> {
    let n = items.len();
    let mut creators: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut removers: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();
    let mut requires: Vec<Vec<String>> = Vec::with_capacity(n);
    for (i, item) in items.iter().enumerate() {
        let (creates, reqs, removes) = item_entities(item);
        for e in creates {
            creators.entry(e).or_insert(i);
        }
        for e in removes {
            removers.entry(e).or_default().push(i);
        }
        requires.push(reqs);
    }
    // predecessors[i]: in-plan items that must be emitted
    // before item i.
    let mut predecessors: Vec<Vec<usize>> = vec![Vec::new(); n];
    for i in 0..n {
        for e in &requires[i] {
            if let Some(&c) = creators.get(e) {
                if c != i && !predecessors[i].contains(&c) {
                    predecessors[i].push(c);
                }
            }
            if let Some(rs) = removers.get(e) {
                for &r in rs {
                    // Item i uses the entity; the remover runs
                    // after it.
                    if r != i && !predecessors[r].contains(&i) {
                        predecessors[r].push(i);
                    }
                }
            }
        }
    }
    for preds in predecessors.iter_mut() {
        preds.sort_unstable();
    }
    fn visit(i: usize, predecessors: &[Vec<usize>], state: &mut [u8], out: &mut Vec<usize>) {
        // state: 0 = unvisited, 1 = on the current path (cycle
        // guard), 2 = emitted.
        if state[i] != 0 {
            return;
        }
        state[i] = 1;
        for &p in &predecessors[i] {
            visit(p, predecessors, state, out);
        }
        state[i] = 2;
        out.push(i);
    }
    let mut state = vec![0u8; n];
    let mut order: Vec<usize> = Vec::with_capacity(n);
    for i in 0..n {
        visit(i, &predecessors, &mut state, &mut order);
    }
    let mut slots: Vec<Option<PlanItem>> = items.into_iter().map(Some).collect();
    order
        .into_iter()
        .map(|i| slots[i].take().expect("each item is emitted exactly once"))
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::parse::test_fixtures::*;
    use crate::parse::*;

    // ---- Dependency ordering (plan-build pass).

    #[test]
    fn dependency_ordering_puts_registration_and_creation_first_deletes_last() {
        // Emitted in the wrong order throughout: routing names
        // a backend that is only created later, the create
        // names a catalog model registered by an add emitted
        // last, and a delete removes the backend the routing
        // items still reference.
        let cfg = cfg_one_model();
        let r = parse_with(
            "{\"items\":[
              {\"kind\":\"group.set_members\",\"target\":\"tool-runner\",\"change\":{\"members\":[\"extra\"]},\"rationale\":\"r\"},
              {\"kind\":\"group.set_fallbacks\",\"target\":\"tool-runner\",\"change\":{\"fallbacks\":[\"extra\",\"openrouter\"]},\"rationale\":\"r\"},
              {\"kind\":\"backend.create\",\"target\":\"extra\",\"change\":{\"model_id\":\"qwen3-27b-planner\",\"port\":8090},\"rationale\":\"r\"},
              {\"kind\":\"backend.delete\",\"target\":\"tool-runner\",\"change\":{},\"rationale\":\"r\"},
              {\"kind\":\"model.add\",\"target\":\"qwen3-27b-planner\",\"change\":{\"source_url\":\"https://example.com/Qwen3-27B-Planner.gguf\"},\"rationale\":\"r\"}
            ]}",
            &cfg,
            json!({}),
        )
        .unwrap();
        let kinds: Vec<(PlanKind, &str)> = r
            .items
            .iter()
            .map(|i| (i.kind, i.target.as_deref().unwrap_or("")))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (PlanKind::ModelAdd, "qwen3-27b-planner"),
                (PlanKind::BackendCreate, "extra"),
                (PlanKind::GroupSetMembers, "tool-runner"),
                (PlanKind::GroupSetFallbacks, "tool-runner"),
                (PlanKind::BackendDelete, "tool-runner"),
            ]
        );
        // Ids follow the new order (the UI aligns validations
        // and decisions by id).
        assert!(r.items.iter().enumerate().all(|(i, item)| item.id == i + 1));
    }

    #[test]
    fn dependency_ordering_preserves_emitted_order_without_edges() {
        // Independent items keep the model's emitted order
        // exactly — the pass moves an item only when an edge
        // forces it.
        let r = parse(
            "{\"items\":[
              {\"kind\":\"backend.set_enabled\",\"target\":\"coder\",\"change\":{\"enabled\":true}},
              {\"kind\":\"backend.set_context\",\"target\":\"planner\",\"change\":{\"n_ctx\":65536}},
              {\"kind\":\"advice\",\"change\":{\"text\":\"note\"}}
            ]}",
        )
        .unwrap();
        let kinds: Vec<PlanKind> = r.items.iter().map(|i| i.kind).collect();
        assert_eq!(
            kinds,
            vec![
                PlanKind::BackendSetEnabled,
                PlanKind::BackendSetContext,
                PlanKind::Advice
            ]
        );
    }

    #[test]
    fn authored_model_add_after_its_create_is_relocated() {
        // (d) The consultant authored both items but in the wrong
        // order: the add moves ahead of the create, unchanged,
        // and no second add is chained.
        let r = parse(
            "{\"items\":[{\"kind\":\"backend.create\",\"target\":\"planner-alt\",\"change\":{\"model_id\":\"qwen3-27b-planner\",\"port\":8090},\"rationale\":\"r\"},{\"kind\":\"model.add\",\"target\":\"qwen3-27b-planner\",\"change\":{\"source_url\":\"https://example.com/Qwen3-27B-Planner.gguf\",\"quant\":\"Q4_K_M\"},\"rationale\":\"register it\"}]}",
        )
        .unwrap();
        assert_eq!(r.items.len(), 2);
        assert_eq!(r.items[0].kind, PlanKind::ModelAdd);
        assert_eq!(
            r.items[0].change["source_url"].as_str(),
            Some("https://example.com/Qwen3-27B-Planner.gguf"),
            "the authored add is the one relocated, not a chained copy"
        );
        assert_eq!(r.items[1].kind, PlanKind::BackendCreate);
        assert!(r.excluded_items.is_empty());
    }
}
