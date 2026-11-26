//! The console command line: built-in commands, parsed into
//! [`Command`] values. The same grammar serves the desktop console
//! prompt and agents (`POST /api/v1/console/exec`). Parsing never
//! executes anything; a line is split into words with simple quoting
//! (`"..."`, `'...'`; backslashes are literal, for Windows paths).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LifecycleAction {
    Start,
    Stop,
    Restart,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    Help {
        topic: Option<String>,
    },
    Recipes,
    Recipe {
        name: String,
    },
    Plan {
        recipe: String,
        variant: Option<usize>,
        params: BTreeMap<String, String>,
        set: BTreeMap<String, String>,
    },
    Run {
        recipe: String,
        variant: Option<usize>,
        params: BTreeMap<String, String>,
        /// Flag overrides applied to every exec step (`--set -fa=0`).
        set: BTreeMap<String, String>,
        /// Backends the caller confirms may be stopped.
        confirm_stop: Vec<String>,
        /// Confirm every conflicting backend.
        yes: bool,
        no_restore: bool,
    },
    Cancel {
        run_id: Option<String>,
    },
    Import,
    Lifecycle {
        action: LifecycleAction,
        backend: String,
    },
    Logs {
        backend: String,
        prev: bool,
        grep: Option<String>,
        tail: usize,
    },
    Gpu,
    Flags {
        backend: String,
    },
    SetFlags {
        backend: String,
        flags: String,
    },
    Bench {
        backend: String,
        depth: String,
        set: BTreeMap<String, String>,
        confirm_stop: Vec<String>,
        yes: bool,
    },
    Gguf {
        target: String,
        keys: Vec<String>,
    },
    History {
        limit: usize,
    },
    Show {
        run_id: String,
        full: bool,
    },
    Export {
        run_id: String,
    },
}

impl Command {
    /// The canonical command word (for audit and class checks).
    pub fn word(&self) -> &'static str {
        match self {
            Command::Help { .. } => "help",
            Command::Recipes => "recipes",
            Command::Recipe { .. } => "recipe",
            Command::Plan { .. } => "plan",
            Command::Run { .. } => "run",
            Command::Cancel { .. } => "cancel",
            Command::Import => "import",
            Command::Lifecycle { action, .. } => match action {
                LifecycleAction::Start => "start",
                LifecycleAction::Stop => "stop",
                LifecycleAction::Restart => "restart",
            },
            Command::Logs { .. } => "logs",
            Command::Gpu => "gpu",
            Command::Flags { .. } => "flags",
            Command::SetFlags { .. } => "set-flags",
            Command::Bench { .. } => "bench",
            Command::Gguf { .. } => "gguf",
            Command::History { .. } => "history",
            Command::Show { .. } => "show",
            Command::Export { .. } => "export",
        }
    }

    /// Does the command start processes or change backend state?
    /// (Refused on non-loopback requests unless allowed.)
    pub fn runs_processes(&self) -> bool {
        matches!(
            self,
            Command::Run { .. }
                | Command::Bench { .. }
                | Command::Lifecycle { .. }
                | Command::SetFlags { .. }
        )
    }
}

/// Split a console line into words.
pub fn split_words(line: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' | '\'' => {
                have = true;
                let q = c;
                let mut closed = false;
                for d in chars.by_ref() {
                    if d == q {
                        closed = true;
                        break;
                    }
                    cur.push(d);
                }
                if !closed {
                    return Err(format!("unterminated {q} quote"));
                }
            }
            c if c.is_whitespace() => {
                if have {
                    out.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            _ => {
                cur.push(c);
                have = true;
            }
        }
    }
    if have {
        out.push(cur);
    }
    Ok(out)
}

pub const HELP: &str = "\
Built-in commands (agents: POST /api/v1/console/exec {\"line\": ...}):
  recipes                         list recipes (built-in and saved)
  recipe <name>                   show one recipe
  plan <recipe> [opts]            resolve commands, GPUs and backends to stop, without running
  run <recipe> [opts]             run a recipe (refuses until conflicts are confirmed)
      --variant N | -v N          run only variant N
      --param k=v | -p k=v        set a parameter (repeatable; bare k=v also works)
      --set FLAG=VALUE            override a flag on every exec step (repeatable)
      --stop a,b | --yes          confirm stopping these / all conflicting backends
      --no-restore                leave stopped backends stopped afterwards
  cancel [run-id]                 cancel the active run (backends are still restored)
  import                          paste a command block (UI: opens the import panel)
  start|stop|restart <backend>    backend lifecycle (graceful drain on stop)
  logs <backend> [--prev] [--grep X] [--tail N]
  gpu                             GPU telemetry snapshot
  flags <backend>                 a backend's launch flags
  set-flags <backend> <flags...>  replace raw flags (journaled; applies on next start)
  bench <backend> --depth 0,4096,8192 [--set -fa=0] [--yes]
  gguf <model-id|backend|path> [--key K ...]
  history [N]                     recent runs
  show <run-id> [--full]          a run's results (compact unless --full)
  export <run-id>                 the run's full JSON export
";

fn take_value(words: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    *i += 1;
    words
        .get(*i)
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn kv(s: &str, flag: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.trim().to_string(), v.to_string()))
        .filter(|(k, _)| !k.is_empty())
        .ok_or_else(|| format!("{flag} expects KEY=VALUE, got {s:?}"))
}

fn need<'a>(words: &'a [String], i: usize, what: &str) -> Result<&'a String, String> {
    words.get(i).ok_or_else(|| format!("missing {what}"))
}

/// Parse a console line.
pub fn parse(line: &str) -> Result<Command, String> {
    let words = split_words(line.trim())?;
    let Some(word) = words.first() else {
        return Err("empty command".into());
    };
    let w = word.to_ascii_lowercase();
    match w.as_str() {
        "help" | "?" => Ok(Command::Help {
            topic: words.get(1).cloned(),
        }),
        "recipes" | "ls" => Ok(Command::Recipes),
        "recipe" => Ok(Command::Recipe {
            name: need(&words, 1, "recipe name")?.clone(),
        }),
        "run" | "plan" => {
            let recipe = need(&words, 1, "recipe name")?.clone();
            let mut variant = None;
            let mut params = BTreeMap::new();
            let mut set = BTreeMap::new();
            let mut confirm_stop = Vec::new();
            let mut yes = false;
            let mut no_restore = false;
            let mut i = 2;
            while i < words.len() {
                let a = words[i].as_str();
                match a {
                    "--variant" | "-v" => {
                        let v = take_value(&words, &mut i, a)?;
                        variant = Some(
                            v.parse::<usize>()
                                .map_err(|_| format!("--variant expects a number, got {v:?}"))?,
                        );
                    }
                    "--param" | "-p" => {
                        let (k, v) = kv(&take_value(&words, &mut i, a)?, a)?;
                        params.insert(k, v);
                    }
                    "--set" => {
                        let (k, v) = kv(&take_value(&words, &mut i, a)?, a)?;
                        set.insert(k, v);
                    }
                    "--stop" => {
                        confirm_stop.extend(
                            take_value(&words, &mut i, a)?
                                .split(',')
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty()),
                        );
                    }
                    "--yes" | "-y" => yes = true,
                    "--no-restore" => no_restore = true,
                    other if other.contains('=') && !other.starts_with('-') => {
                        let (k, v) = kv(other, "parameter")?;
                        params.insert(k, v);
                    }
                    other => return Err(format!("unknown option {other:?} for {w}")),
                }
                i += 1;
            }
            Ok(if w == "plan" {
                Command::Plan {
                    recipe,
                    variant,
                    params,
                    set,
                }
            } else {
                Command::Run {
                    recipe,
                    variant,
                    params,
                    set,
                    confirm_stop,
                    yes,
                    no_restore,
                }
            })
        }
        "cancel" => Ok(Command::Cancel {
            run_id: words.get(1).cloned(),
        }),
        "import" | "paste" => Ok(Command::Import),
        "start" | "stop" | "restart" => Ok(Command::Lifecycle {
            action: match w.as_str() {
                "start" => LifecycleAction::Start,
                "stop" => LifecycleAction::Stop,
                _ => LifecycleAction::Restart,
            },
            backend: need(&words, 1, "backend id")?.clone(),
        }),
        "logs" | "log" => {
            let backend = need(&words, 1, "backend id")?.clone();
            let mut prev = false;
            let mut grep = None;
            let mut tail = 60usize;
            let mut i = 2;
            while i < words.len() {
                let a = words[i].as_str();
                match a {
                    "--prev" => prev = true,
                    "--grep" | "-g" => grep = Some(take_value(&words, &mut i, a)?),
                    "--tail" | "-n" => {
                        let v = take_value(&words, &mut i, a)?;
                        tail = v
                            .parse()
                            .map_err(|_| format!("--tail expects a number, got {v:?}"))?;
                    }
                    other => return Err(format!("unknown option {other:?} for logs")),
                }
                i += 1;
            }
            Ok(Command::Logs {
                backend,
                prev,
                grep,
                tail: tail.clamp(1, 5000),
            })
        }
        "gpu" | "gpus" => Ok(Command::Gpu),
        "flags" => Ok(Command::Flags {
            backend: need(&words, 1, "backend id")?.clone(),
        }),
        "set-flags" | "setflags" => {
            let backend = need(&words, 1, "backend id")?.clone();
            // Everything after the backend id, re-quoted only where a
            // word contains whitespace; stored as the raw-flags text.
            let flags = words[2..]
                .iter()
                .map(|s| {
                    if s.contains(char::is_whitespace) {
                        format!("\"{s}\"")
                    } else {
                        s.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            Ok(Command::SetFlags { backend, flags })
        }
        "bench" => {
            let backend = need(&words, 1, "backend id")?.clone();
            let mut depth = "0,4096,8192".to_string();
            let mut set = BTreeMap::new();
            let mut confirm_stop = Vec::new();
            let mut yes = false;
            let mut i = 2;
            while i < words.len() {
                let a = words[i].as_str();
                match a {
                    "--depth" | "-d" => depth = take_value(&words, &mut i, a)?,
                    "--set" => {
                        let (k, v) = kv(&take_value(&words, &mut i, a)?, a)?;
                        set.insert(k, v);
                    }
                    "--fa" | "--ctk" | "--ctv" | "-fa" | "-ctk" | "-ctv" | "-p" | "-n" | "-ub"
                    | "-b" | "-r" => {
                        let flag = match a {
                            "--fa" => "-fa",
                            "--ctk" => "-ctk",
                            "--ctv" => "-ctv",
                            other => other,
                        }
                        .to_string();
                        set.insert(flag, take_value(&words, &mut i, a)?);
                    }
                    "--stop" => confirm_stop.extend(
                        take_value(&words, &mut i, a)?
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty()),
                    ),
                    "--yes" | "-y" => yes = true,
                    other => return Err(format!("unknown option {other:?} for bench")),
                }
                i += 1;
            }
            Ok(Command::Bench {
                backend,
                depth,
                set,
                confirm_stop,
                yes,
            })
        }
        "gguf" => {
            let target = need(&words, 1, "model id, backend id or path")?.clone();
            let mut keys = Vec::new();
            let mut i = 2;
            while i < words.len() {
                let a = words[i].as_str();
                match a {
                    "--key" | "-k" => keys.push(take_value(&words, &mut i, a)?),
                    other => keys.push(other.to_string()),
                }
                i += 1;
            }
            Ok(Command::Gguf { target, keys })
        }
        "history" => Ok(Command::History {
            limit: words
                .get(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(20usize)
                .clamp(1, 500),
        }),
        "show" => Ok(Command::Show {
            run_id: need(&words, 1, "run id")?.clone(),
            full: words.iter().skip(2).any(|w| w == "--full"),
        }),
        "export" => Ok(Command::Export {
            run_id: need(&words, 1, "run id")?.clone(),
        }),
        other => Err(format!("unknown command {other:?} (try `help`)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_run_with_every_option() {
        let c = parse("run fa-ab -v 1 -p m=C:\\x.gguf depth=8192 --set --parallel=1 --stop planner-moe,chat --no-restore").unwrap();
        match c {
            Command::Run {
                recipe,
                variant,
                params,
                set,
                confirm_stop,
                yes,
                no_restore,
            } => {
                assert_eq!(recipe, "fa-ab");
                assert_eq!(variant, Some(1));
                assert_eq!(params["m"], "C:\\x.gguf");
                assert_eq!(params["depth"], "8192");
                assert_eq!(set["--parallel"], "1");
                assert_eq!(confirm_stop, vec!["planner-moe", "chat"]);
                assert!(!yes && no_restore);
            }
            other => panic!("{other:?}"),
        }
        assert!(parse("run").is_err());
        assert!(parse("run x --bogus").is_err());
        assert!(parse("frobnicate").is_err());
    }

    #[test]
    fn parses_the_other_builtins() {
        assert_eq!(
            parse("logs planner-moe --prev --grep \"CUDA error\" -n 20").unwrap(),
            Command::Logs {
                backend: "planner-moe".into(),
                prev: true,
                grep: Some("CUDA error".into()),
                tail: 20
            }
        );
        assert!(parse("stop planner-moe").unwrap().runs_processes());
        assert!(!parse("gpu").unwrap().runs_processes());
        match parse("set-flags planner-moe --cache-type-k f16 --parallel 1").unwrap() {
            Command::SetFlags { flags, .. } => assert_eq!(flags, "--cache-type-k f16 --parallel 1"),
            other => panic!("{other:?}"),
        }
        match parse("bench planner-moe --depth 8192 --fa 0 --ctv f16 -y").unwrap() {
            Command::Bench {
                depth, set, yes, ..
            } => {
                assert_eq!(depth, "8192");
                assert_eq!(set["-fa"], "0");
                assert_eq!(set["-ctv"], "f16");
                assert!(yes);
            }
            other => panic!("{other:?}"),
        }
        match parse("gguf planner-moe attention.key_length --key head_count_kv").unwrap() {
            Command::Gguf { keys, .. } => {
                assert_eq!(keys, vec!["attention.key_length", "head_count_kv"])
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(parse("history 5").unwrap(), Command::History { limit: 5 });
        assert!(split_words("a \"b").is_err());
    }
}
