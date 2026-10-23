//! Comparability keys (design §3.3): a hash over everything held
//! constant in a sweep — model identity, runner kind, prompt hash,
//! temperature, llama build. Arms are compared freely within one key;
//! across sweeps, only when keys match on everything except the
//! swept axes, and [`differing_fields`] names what differs so the UI
//! can refuse a cross-sweep delta with the field named.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The one runner kind since the D2 amendment: server-driven.
pub const RUNNER_KIND: &str = "server";

/// Identity of the measured model file: the field set the Optimize
/// fingerprint already uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ModelIdentity {
    pub path: String,
    pub size_bytes: u64,
    /// Hex sha256 of the file, when it has been stamped.
    pub sha256: Option<String>,
}

/// Hex sha256 of a prompt body — the hash a sweep pins its prompt by.
pub fn prompt_sha256(body: &str) -> String {
    hex_sha256(body.as_bytes())
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest.iter() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// The inputs of a comparability key, held together so callers can
/// both compute a key and diff two conditions field by field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct KeyInputs {
    pub model: ModelIdentity,
    pub prompt_sha: String,
    pub temperature: f32,
    pub llama_build: String,
}

impl KeyInputs {
    /// The comparability key for these inputs; identical to calling
    /// [`comparability_key`] with the fields unpacked.
    pub fn key(&self) -> String {
        comparability_key(&self.model, &self.prompt_sha, self.temperature, &self.llama_build)
    }

    /// The canonical multi-line string the key hashes. One
    /// `name=value` line per held-constant field, in a fixed order;
    /// an unstamped model sha256 hashes as the empty string.
    fn canonical(&self) -> String {
        format!(
            "runner_kind={RUNNER_KIND}\nmodel_path={}\nmodel_size_bytes={}\nmodel_sha256={}\nprompt_sha256={}\ntemperature={}\nllama_build={}",
            self.model.path,
            self.model.size_bytes,
            self.model.sha256.as_deref().unwrap_or(""),
            self.prompt_sha,
            self.temperature,
            self.llama_build,
        )
    }
}

/// Compute an arm's comparability key: hex sha256 over the canonical
/// string of held-constant inputs (see [`KeyInputs`]).
pub fn comparability_key(
    model: &ModelIdentity,
    prompt_sha: &str,
    temperature: f32,
    llama_build: &str,
) -> String {
    let inputs = KeyInputs {
        model: model.clone(),
        prompt_sha: prompt_sha.to_string(),
        temperature,
        llama_build: llama_build.to_string(),
    };
    hex_sha256(inputs.canonical().as_bytes())
}

/// Name the held-constant fields on which `a` and `b` differ, for
/// the UI's cross-sweep refusal message (§3.3: the differing field
/// is named, not footnoted). Model identity is reported per field
/// (`model_path`, `model_size_bytes`, `model_sha256`) so the message
/// can be exact. Empty when the two are comparable.
pub fn differing_fields(a: &KeyInputs, b: &KeyInputs) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    if a.model.path != b.model.path {
        out.push("model_path");
    }
    if a.model.size_bytes != b.model.size_bytes {
        out.push("model_size_bytes");
    }
    if a.model.sha256 != b.model.sha256 {
        out.push("model_sha256");
    }
    if a.prompt_sha != b.prompt_sha {
        out.push("prompt_sha256");
    }
    if a.temperature != b.temperature {
        out.push("temperature");
    }
    if a.llama_build != b.llama_build {
        out.push("llama_build");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> KeyInputs {
        KeyInputs {
            model: ModelIdentity {
                path: "models/coder.gguf".to_string(),
                size_bytes: 49_301_055_488,
                sha256: Some("ab12".to_string()),
            },
            prompt_sha: prompt_sha256("prompt body"),
            temperature: 0.0,
            llama_build: "b11658".to_string(),
        }
    }

    #[test]
    fn key_is_stable_for_identical_inputs() {
        let a = inputs();
        assert_eq!(a.key(), a.clone().key());
        assert_eq!(
            comparability_key(&a.model, &a.prompt_sha, a.temperature, &a.llama_build),
            a.key()
        );
        assert_eq!(a.key().len(), 64);
    }

    #[test]
    fn key_is_sensitive_to_every_input() {
        let a = inputs();
        let mut b = a.clone();
        b.temperature = 0.6;
        assert_ne!(a.key(), b.key());
        let mut b = a.clone();
        b.prompt_sha = prompt_sha256("other prompt");
        assert_ne!(a.key(), b.key());
        let mut b = a.clone();
        b.llama_build = "b11659".to_string();
        assert_ne!(a.key(), b.key());
        let mut b = a.clone();
        b.model.size_bytes += 1;
        assert_ne!(a.key(), b.key());
        let mut b = a.clone();
        b.model.sha256 = None;
        assert_ne!(a.key(), b.key());
    }

    #[test]
    fn differing_fields_names_what_changed() {
        let a = inputs();
        assert!(differing_fields(&a, &a.clone()).is_empty());
        let mut b = a.clone();
        b.temperature = 0.6;
        assert_eq!(differing_fields(&a, &b), vec!["temperature"]);
        let mut b = a.clone();
        b.model.path = "models/other.gguf".to_string();
        b.prompt_sha = prompt_sha256("changed");
        assert_eq!(
            differing_fields(&a, &b),
            vec!["model_path", "prompt_sha256"]
        );
    }

    #[test]
    fn prompt_hash_is_hex_sha256() {
        // sha256 of the empty string, a known constant.
        assert_eq!(
            prompt_sha256(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_ne!(prompt_sha256("a"), prompt_sha256("b"));
    }
}
