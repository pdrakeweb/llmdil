//! Shared helpers for the discover submodules' unit tests.

pub(super) const MIB64: u64 = 1024 * 1024;

/// Trimmed shape of the real Qwen/Qwen3-8B-GGUF record
/// (verified against the live API, §A.6): gguf block with
/// total / architecture / context_length / totalFileSize,
/// siblings with size + lfs.
pub(super) fn record_body() -> String {
    serde_json::json!({
        "id": "Qwen/Qwen3-8B-GGUF",
        "downloads": 594664,
        "likes": 330,
        "gguf": {
            "total": 8190735360u64,
            "architecture": "qwen3",
            "context_length": 40960,
            "totalFileSize": 5027783488u64,
        },
        "siblings": [
            { "rfilename": ".gitattributes", "size": 1798 },
            { "rfilename": "Qwen3-8B-Q4_K_M.gguf", "size": 5027783488u64,
              "lfs": { "sha256": "d98c", "size": 5027783488u64, "pointerSize": 135 } },
            { "rfilename": "Qwen3-8B-Q8_0.gguf", "size": 8709518112u64,
              "lfs": { "sha256": "408b", "size": 8709518112u64, "pointerSize": 135 } },
            { "rfilename": "mmproj-Qwen3-8B.gguf", "size": 640000000u64 },
            { "rfilename": "README.md", "size": 7577 },
        ],
    })
    .to_string()
}
