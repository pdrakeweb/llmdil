//! Exit-code decoding.
//!
//! A Windows process killed by an unhandled exception or a fast-fail
//! exits with the NTSTATUS code as its exit code (`0xc0000409`
//! appears as `-1073740791` through `ExitStatus::code()`). The table
//! names the common ones with what they usually mean for llama.cpp
//! tools; anything else is reported raw.

use serde::{Deserialize, Serialize};

/// A decoded exit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecodedExit {
    /// The raw code as the OS reported it (`i32`), or `None` for a
    /// signal exit / unknown.
    pub code: Option<i64>,
    /// `0xc0000409` for NTSTATUS-range codes, the decimal otherwise.
    pub display: String,
    /// The symbolic name (`STATUS_STACK_BUFFER_OVERRUN`), when known.
    pub name: Option<String>,
    /// Operator-facing meaning.
    pub meaning: String,
    /// True for exits that are crashes (exceptions, fast-fail,
    /// signals), as opposed to an orderly non-zero exit.
    pub crash: bool,
}

const TABLE: &[(u32, &str, &str)] = &[
    (
        0xC000_0409,
        "STATUS_STACK_BUFFER_OVERRUN",
        "abort() — GGML_ABORT/fast-fail, not memory corruption (the MSVC CRT reports abort() and __fastfail as this code)",
    ),
    (
        0xC000_0005,
        "STATUS_ACCESS_VIOLATION",
        "access violation — invalid memory read/write (a real crash: null/dangling pointer, out-of-bounds)",
    ),
    (
        0xC000_001D,
        "STATUS_ILLEGAL_INSTRUCTION",
        "illegal instruction — the binary uses CPU instructions this machine lacks (AVX/AVX2/AVX-512 build mismatch)",
    ),
    (
        0xC000_0096,
        "STATUS_PRIVILEGED_INSTRUCTION",
        "privileged instruction — usually a corrupted or mismatched binary",
    ),
    (
        0xC000_0094,
        "STATUS_INTEGER_DIVIDE_BY_ZERO",
        "integer divide by zero",
    ),
    (
        0xC000_00FD,
        "STATUS_STACK_OVERFLOW",
        "stack overflow — runaway recursion or a too-large stack allocation",
    ),
    (
        0xC000_0374,
        "STATUS_HEAP_CORRUPTION",
        "heap corruption detected by the Windows heap — real memory corruption",
    ),
    (
        0xC000_0017,
        "STATUS_NO_MEMORY",
        "out of (system) memory / commit limit reached",
    ),
    (
        0xC000_012D,
        "STATUS_COMMITMENT_LIMIT",
        "commit limit reached — page file too small for the allocation",
    ),
    (
        0xC000_0135,
        "STATUS_DLL_NOT_FOUND",
        "a required DLL was not found (CUDA runtime / cuBLAS / ggml DLL missing from the install or PATH)",
    ),
    (
        0xC000_0139,
        "STATUS_ENTRYPOINT_NOT_FOUND",
        "DLL entry point not found — mismatched DLL versions (e.g. CUDA runtime vs build)",
    ),
    (
        0xC000_0142,
        "STATUS_DLL_INIT_FAILED",
        "a DLL failed to initialize (often the CUDA driver/runtime)",
    ),
    (
        0xC000_007B,
        "STATUS_INVALID_IMAGE_FORMAT",
        "invalid image format — 32/64-bit or corrupted binary/DLL",
    ),
    (
        0xC000_013A,
        "STATUS_CONTROL_C_EXIT",
        "terminated by Ctrl+C / console close",
    ),
    (
        0xC000_0420,
        "STATUS_ASSERTION_FAILURE",
        "assertion failure (a debug-build assert)",
    ),
    (
        0xC000_041D,
        "STATUS_FATAL_USER_CALLBACK_EXCEPTION",
        "an exception escaped a user callback",
    ),
    (
        0x8000_0003,
        "STATUS_BREAKPOINT",
        "breakpoint hit (__debugbreak) outside a debugger",
    ),
    (
        0xE06D_7363,
        "MSVC_CPP_EXCEPTION",
        "an uncaught C++ exception (std::runtime_error etc.) — check stderr for the message",
    ),
    (
        0x4001_0004,
        "DBG_TERMINATE_PROCESS",
        "terminated by a debugger / TerminateProcess",
    ),
];

/// Decode a raw OS exit code (`ExitStatus::code()` on Windows).
pub fn decode(code: i64) -> DecodedExit {
    let as_u32 = code as i32 as u32;
    if let Some((_, name, meaning)) = TABLE.iter().find(|(c, _, _)| *c == as_u32) {
        return DecodedExit {
            code: Some(code),
            display: format!("0x{as_u32:08x}"),
            name: Some((*name).to_string()),
            meaning: (*meaning).to_string(),
            crash: !matches!(as_u32, 0xC000_013A | 0x4001_0004),
        };
    }
    // Any other 0xC....... is an NTSTATUS error: still a crash.
    if as_u32 & 0xC000_0000 == 0xC000_0000 {
        return DecodedExit {
            code: Some(code),
            display: format!("0x{as_u32:08x}"),
            name: None,
            meaning: "unrecognized NTSTATUS error code (an abnormal termination)".to_string(),
            crash: true,
        };
    }
    let meaning = match code {
        0 => "success".to_string(),
        1 => "exit 1 — a generic error (also what a forced kill / taskkill /F reports on Windows)"
            .to_string(),
        _ => format!("exit {code} — the program reported an error"),
    };
    DecodedExit {
        code: Some(code),
        display: code.to_string(),
        name: None,
        meaning,
        crash: false,
    }
}

/// Decode a Unix signal termination.
pub fn decode_signal(sig: i32) -> DecodedExit {
    let (name, meaning) = match sig {
        6 => (
            "SIGABRT",
            "abort() — GGML_ABORT / assert, not memory corruption",
        ),
        11 => ("SIGSEGV", "segmentation fault — invalid memory access"),
        4 => ("SIGILL", "illegal instruction — CPU feature mismatch"),
        9 => ("SIGKILL", "killed (OOM killer or a forced kill)"),
        15 => ("SIGTERM", "terminated"),
        8 => ("SIGFPE", "floating point / divide-by-zero exception"),
        7 => ("SIGBUS", "bus error"),
        _ => ("signal", "killed by a signal"),
    };
    DecodedExit {
        code: None,
        display: format!("signal {sig}"),
        name: Some(name.to_string()),
        meaning: meaning.to_string(),
        crash: !matches!(sig, 15),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_fast_fail_from_either_sign() {
        let d = decode(-1073740791);
        assert_eq!(d.display, "0xc0000409");
        assert_eq!(d.name.as_deref(), Some("STATUS_STACK_BUFFER_OVERRUN"));
        assert!(d
            .meaning
            .starts_with("abort() — GGML_ABORT/fast-fail, not memory corruption"));
        assert!(d.crash);
        assert_eq!(decode(0xC000_0409_i64).display, "0xc0000409");
    }

    #[test]
    fn decodes_the_common_table_and_falls_back() {
        assert_eq!(
            decode(0xC000_0005_u32 as i32 as i64).name.as_deref(),
            Some("STATUS_ACCESS_VIOLATION")
        );
        assert!(decode(0xC000_001D_u32 as i32 as i64)
            .meaning
            .contains("illegal instruction"));
        let unknown = decode(0xC0DE_0001_u32 as i32 as i64);
        assert!(unknown.crash && unknown.name.is_none());
        let ok = decode(0);
        assert!(!ok.crash && ok.meaning == "success");
        assert!(!decode(2).crash);
        assert!(!decode(0xC000_013A_u32 as i32 as i64).crash);
        assert_eq!(decode_signal(6).name.as_deref(), Some("SIGABRT"));
    }
}
