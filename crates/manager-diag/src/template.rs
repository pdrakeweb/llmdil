//! Template resolution: `{name}` placeholders filled from LLMdil
//! state and user parameters.
//!
//! The app builds a [`TemplateContext`] with `llamacpp_bin`,
//! `data_dir`, `models_dir`, `backend.<id>.model_path` /
//! `.raw_flags` / `.port` / `.server_args`, `gpu.<n>.name` / `.uuid`,
//! and `env.<NAME>` from the process environment; recipe parameters
//! are layered on top with [`TemplateContext::with_params`].
//!
//! Resolution only ever produces strings for an argv array: a list
//! variable used as a whole token splices into several arguments; it
//! is never joined into a shell command. `{{` and `}}` escape braces.

use std::collections::BTreeMap;

use crate::recipe::Param;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TemplateContext {
    pub vars: BTreeMap<String, String>,
    /// List variables (`backend.<id>.raw_flags`): spliced when a token
    /// is exactly `{name}`, space-joined inside a longer string.
    pub lists: BTreeMap<String, Vec<String>>,
    /// The process environment, served as `{env.NAME}`.
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateError {
    Unknown(String),
    Unclosed(String),
    MissingParam(String),
}

impl std::fmt::Display for TemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TemplateError::Unknown(n) => write!(f, "unknown template variable {{{n}}}"),
            TemplateError::Unclosed(s) => write!(f, "unclosed `{{` in {s:?}"),
            TemplateError::MissingParam(n) => {
                write!(f, "parameter `{n}` has no default and was not supplied")
            }
        }
    }
}

impl std::error::Error for TemplateError {}

enum Piece<'a> {
    Lit(String),
    Var(&'a str),
}

fn pieces(s: &str) -> Result<Vec<Piece<'_>>, TemplateError> {
    let mut out = Vec::new();
    let mut lit = String::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'{' {
            if b.get(i + 1) == Some(&b'{') {
                lit.push('{');
                i += 2;
                continue;
            }
            let Some(end) = s[i + 1..].find('}') else {
                return Err(TemplateError::Unclosed(s.to_string()));
            };
            let name = &s[i + 1..i + 1 + end];
            if !lit.is_empty() {
                out.push(Piece::Lit(std::mem::take(&mut lit)));
            }
            out.push(Piece::Var(name.trim()));
            i += end + 2;
        } else if c == b'}' && b.get(i + 1) == Some(&b'}') {
            lit.push('}');
            i += 2;
        } else {
            // Push the whole UTF-8 char.
            let ch = s[i..].chars().next().unwrap_or('\u{fffd}');
            lit.push(ch);
            i += ch.len_utf8();
        }
    }
    if !lit.is_empty() {
        out.push(Piece::Lit(lit));
    }
    Ok(out)
}

/// The variable names `s` references (deduplicated, in order).
pub fn referenced(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Ok(ps) = pieces(s) {
        for p in ps {
            if let Piece::Var(n) = p {
                if !out.iter().any(|o| o == n) {
                    out.push(n.to_string());
                }
            }
        }
    }
    out
}

impl TemplateContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, k: impl Into<String>, v: impl Into<String>) {
        self.vars.insert(k.into(), v.into());
    }

    pub fn set_list(&mut self, k: impl Into<String>, v: Vec<String>) {
        self.lists.insert(k.into(), v);
    }

    fn lookup(&self, name: &str) -> Result<String, TemplateError> {
        if let Some(v) = self.vars.get(name) {
            return Ok(v.clone());
        }
        if let Some(l) = self.lists.get(name) {
            return Ok(l.join(" "));
        }
        if let Some(env) = name.strip_prefix("env.") {
            // Windows environment names are case-insensitive.
            if let Some(v) = self
                .env
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(env))
                .map(|(_, v)| v.clone())
            {
                return Ok(v);
            }
        }
        Err(TemplateError::Unknown(name.to_string()))
    }

    /// Resolve every placeholder in `s`.
    pub fn resolve_str(&self, s: &str) -> Result<String, TemplateError> {
        let mut out = String::new();
        for p in pieces(s)? {
            match p {
                Piece::Lit(l) => out.push_str(&l),
                Piece::Var(n) => out.push_str(&self.lookup(n)?),
            }
        }
        Ok(out)
    }

    /// Resolve an argument list; a token that is exactly one list
    /// variable splices into its elements.
    pub fn resolve_args(&self, args: &[String]) -> Result<Vec<String>, TemplateError> {
        let mut out = Vec::new();
        for a in args {
            let ps = pieces(a)?;
            if let [Piece::Var(n)] = ps.as_slice() {
                if let Some(list) = self.lists.get(*n) {
                    out.extend(list.iter().cloned());
                    continue;
                }
            }
            out.push(self.resolve_str(a)?);
        }
        Ok(out)
    }

    /// Resolve string leaves of a JSON value.
    pub fn resolve_json(&self, v: &serde_json::Value) -> Result<serde_json::Value, TemplateError> {
        Ok(match v {
            serde_json::Value::String(s) => serde_json::Value::String(self.resolve_str(s)?),
            serde_json::Value::Array(a) => serde_json::Value::Array(
                a.iter()
                    .map(|x| self.resolve_json(x))
                    .collect::<Result<_, _>>()?,
            ),
            serde_json::Value::Object(o) => {
                let mut m = serde_json::Map::new();
                for (k, x) in o {
                    m.insert(k.clone(), self.resolve_json(x)?);
                }
                serde_json::Value::Object(m)
            }
            other => other.clone(),
        })
    }

    /// Layer recipe parameters over this context: a supplied value
    /// wins, else the default; values may reference the base context
    /// and earlier parameters. Supplied names the recipe does not
    /// declare are accepted as ad-hoc parameters.
    pub fn with_params(
        &self,
        params: &[Param],
        supplied: &BTreeMap<String, String>,
    ) -> Result<TemplateContext, TemplateError> {
        let mut ctx = self.clone();
        for p in params {
            let raw = match supplied.get(&p.name) {
                Some(v) => v.clone(),
                None => p
                    .default
                    .clone()
                    .ok_or_else(|| TemplateError::MissingParam(p.name.clone()))?,
            };
            let v = ctx.resolve_str(&raw)?;
            ctx.vars.insert(p.name.clone(), v);
        }
        for (k, v) in supplied {
            if !params.iter().any(|p| &p.name == k) {
                let v = ctx.resolve_str(v)?;
                ctx.vars.insert(k.clone(), v);
            }
        }
        ctx.alias_backend_params();
        Ok(ctx)
    }

    /// For every scalar variable whose value names a backend
    /// (`backend.<value>.port` exists), expose that backend's
    /// variables under the variable's own name: a parameter
    /// `backend = planner-moe` makes `{backend.model_path}` mean
    /// `{backend.planner-moe.model_path}`.
    pub fn alias_backend_params(&mut self) {
        let names: Vec<(String, String)> = self
            .vars
            .iter()
            .filter(|(k, _)| !k.contains('.'))
            .filter(|(_, v)| self.vars.contains_key(&format!("backend.{v}.port")))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (alias, id) in names {
            let prefix = format!("backend.{id}.");
            let scalars: Vec<(String, String)> = self
                .vars
                .iter()
                .filter_map(|(k, v)| {
                    k.strip_prefix(&prefix)
                        .map(|rest| (format!("{alias}.{rest}"), v.clone()))
                })
                .collect();
            let lists: Vec<(String, Vec<String>)> = self
                .lists
                .iter()
                .filter_map(|(k, v)| {
                    k.strip_prefix(&prefix)
                        .map(|rest| (format!("{alias}.{rest}"), v.clone()))
                })
                .collect();
            self.vars.extend(scalars);
            self.lists.extend(lists);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> TemplateContext {
        let mut c = TemplateContext::new();
        c.set("models_dir", "C:/data/models");
        c.set("backend.planner-moe.port", "8081");
        c.set_list(
            "backend.planner-moe.raw_flags",
            vec!["-ot".into(), "exps=CPU".into()],
        );
        c.env
            .insert("APPDATA".into(), "C:/Users/p/AppData/Roaming".into());
        c
    }

    #[test]
    fn resolves_scalars_lists_env_and_escapes() {
        let c = ctx();
        assert_eq!(
            c.resolve_str("{models_dir}/x.gguf").unwrap(),
            "C:/data/models/x.gguf"
        );
        assert_eq!(
            c.resolve_str("{env.appdata}\\a").unwrap(),
            "C:/Users/p/AppData/Roaming\\a"
        );
        assert_eq!(c.resolve_str("{{literal}}").unwrap(), "{literal}");
        let args = c
            .resolve_args(&[
                "--port".into(),
                "{backend.planner-moe.port}".into(),
                "{backend.planner-moe.raw_flags}".into(),
            ])
            .unwrap();
        assert_eq!(args, vec!["--port", "8081", "-ot", "exps=CPU"]);
        assert_eq!(
            c.resolve_str("{nope}").unwrap_err(),
            TemplateError::Unknown("nope".into())
        );
        assert!(c.resolve_str("{open").is_err());
        assert_eq!(referenced("{a} {b} {a}"), vec!["a", "b"]);
    }

    #[test]
    fn params_layer_with_defaults_and_overrides() {
        let c = ctx();
        let params = vec![
            Param {
                name: "m".into(),
                default: Some("{models_dir}/w.gguf".into()),
                description: String::new(),
            },
            Param {
                name: "depth".into(),
                default: None,
                description: String::new(),
            },
        ];
        assert_eq!(
            c.with_params(&params, &BTreeMap::new()).unwrap_err(),
            TemplateError::MissingParam("depth".into())
        );
        let supplied: BTreeMap<String, String> = [("depth".to_string(), "8192".to_string())].into();
        let p = c.with_params(&params, &supplied).unwrap();
        assert_eq!(p.vars["m"], "C:/data/models/w.gguf");
        assert_eq!(p.vars["depth"], "8192");
    }
}
