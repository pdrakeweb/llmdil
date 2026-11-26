//! The flag model recipe variants operate on.
//!
//! A command line is an argv array (never a shell string). For
//! variant overrides it is read as a sequence of items: a flag
//! (`-d`, `--flash-attn`, `--x=y`) with an optional value — the next
//! token when that token is not itself a flag — or a positional
//! token. Negative numbers (`-1`) are values, not flags.

use std::collections::BTreeMap;

/// One argv item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgItem {
    /// A flag and its value. `inline` records the `--flag=value`
    /// spelling so re-emission keeps the operator's form.
    Flag {
        name: String,
        value: Option<String>,
        inline: bool,
    },
    Positional(String),
}

impl ArgItem {
    pub fn flag_name(&self) -> Option<&str> {
        match self {
            ArgItem::Flag { name, .. } => Some(name),
            ArgItem::Positional(_) => None,
        }
    }
}

/// Is `tok` a flag token (`-x`, `--xyz`), as opposed to a value?
/// Negative numbers and a bare `-` are values.
pub fn is_flag(tok: &str) -> bool {
    if !tok.starts_with('-') || tok.len() < 2 {
        return false;
    }
    let rest = &tok[1..];
    // -1, -0.5, -1e3: numeric values.
    if tok.parse::<f64>().is_ok() {
        return false;
    }
    // A template placeholder such as -{x} is still a flag; anything
    // else starting with a dash is.
    !rest.is_empty()
}

/// Group an argv (without the program) into items.
pub fn group(args: &[String]) -> Vec<ArgItem> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let t = &args[i];
        if is_flag(t) {
            if t.starts_with("--") {
                if let Some((n, v)) = t.split_once('=') {
                    out.push(ArgItem::Flag {
                        name: n.to_string(),
                        value: Some(v.to_string()),
                        inline: true,
                    });
                    i += 1;
                    continue;
                }
            }
            let value = args.get(i + 1).filter(|n| !is_flag(n)).cloned();
            let consumed = if value.is_some() { 2 } else { 1 };
            out.push(ArgItem::Flag {
                name: t.clone(),
                value,
                inline: false,
            });
            i += consumed;
        } else {
            out.push(ArgItem::Positional(t.clone()));
            i += 1;
        }
    }
    out
}

/// Re-emit grouped items as an argv.
pub fn ungroup(items: &[ArgItem]) -> Vec<String> {
    let mut out = Vec::new();
    for it in items {
        match it {
            ArgItem::Positional(p) => out.push(p.clone()),
            ArgItem::Flag {
                name,
                value,
                inline,
            } => match (value, inline) {
                (Some(v), true) => out.push(format!("{name}={v}")),
                (Some(v), false) => {
                    out.push(name.clone());
                    out.push(v.clone());
                }
                (None, _) => out.push(name.clone()),
            },
        }
    }
    out
}

/// Apply a variant's overrides to `args`: `set` replaces the value of
/// the first occurrence of each flag (an empty value makes it a bare
/// flag) or appends the flag when absent; `unset` removes every
/// occurrence. Deterministic: appended flags follow `set`'s order.
pub fn apply_overrides(
    args: &[String],
    set: &BTreeMap<String, String>,
    unset: &[String],
) -> Vec<String> {
    let mut items = group(args);
    for (flag, val) in set {
        let value = if val.is_empty() {
            None
        } else {
            Some(val.clone())
        };
        match items
            .iter_mut()
            .find(|it| it.flag_name() == Some(flag.as_str()))
        {
            Some(ArgItem::Flag {
                value: v, inline, ..
            }) => {
                if value.is_none() {
                    *inline = false;
                }
                *v = value;
            }
            _ => items.push(ArgItem::Flag {
                name: flag.clone(),
                value,
                inline: false,
            }),
        }
    }
    items.retain(|it| match it.flag_name() {
        Some(n) => !unset.iter().any(|u| u == n),
        None => true,
    });
    ungroup(&items)
}

/// The value of the last occurrence of any of `names` (`Some("")`
/// for a bare flag).
pub fn flag_value(args: &[String], names: &[&str]) -> Option<String> {
    let mut found = None;
    for it in group(args) {
        if let ArgItem::Flag { name, value, .. } = it {
            if names.contains(&name.as_str()) {
                found = Some(value.unwrap_or_default());
            }
        }
    }
    found
}

/// Whether any of `names` is present.
pub fn has_flag(args: &[String], names: &[&str]) -> bool {
    flag_value(args, names).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn groups_flags_values_and_negative_numbers() {
        let g = group(&v("-m x.gguf -ngl 99 --no-mmap -s -1 --ctx=4096 pos"));
        assert_eq!(g.len(), 6);
        assert_eq!(
            g[3],
            ArgItem::Flag {
                name: "-s".into(),
                value: Some("-1".into()),
                inline: false
            }
        );
        assert_eq!(
            g[2],
            ArgItem::Flag {
                name: "--no-mmap".into(),
                value: None,
                inline: false
            }
        );
        assert_eq!(
            ungroup(&g),
            v("-m x.gguf -ngl 99 --no-mmap -s -1 --ctx=4096 pos")
        );
    }

    #[test]
    fn overrides_replace_append_and_remove() {
        let mut set = BTreeMap::new();
        set.insert("-d".to_string(), "8192".to_string());
        set.insert("--parallel".to_string(), "1".to_string());
        let out = apply_overrides(&v("-p 512 -d 0,4096 -fa 1"), &set, &["-fa".to_string()]);
        assert_eq!(out, v("-p 512 -d 8192 --parallel 1"));
        assert_eq!(flag_value(&out, &["-d"]).as_deref(), Some("8192"));
        assert!(!has_flag(&out, &["-fa"]));
    }
}
