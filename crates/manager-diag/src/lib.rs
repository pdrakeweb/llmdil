//! Pure logic for the LLMdil debug console (`docs/debug-console.md`).
//!
//! The console turns a pasted block of diagnostic commands into a
//! reviewable, parameterized [`Recipe`], runs it under the app's
//! control (manager-app's `console` module owns processes, GPUs,
//! backends and persistence), and reduces the output to structured
//! results. Everything in this crate is pure over its inputs so it is
//! unit-tested on every platform:
//!
//! - [`recipe`]: the recipe document, variants, validation, expansion;
//! - [`argv`]: the flag model variants override (`-d 8192`, `--no-mmap`);
//! - [`template`]: `{llamacpp_bin}` / `{backend.<id>.model_path}` /
//!   user-parameter resolution into argv arrays (never a shell string);
//! - [`paste`]: PowerShell / cmd block import, including variant folding;
//! - [`command`]: the console line grammar (`run`, `logs`, `gpu`, …);
//! - [`parsers`]: llama-bench (JSON and markdown), llama-server timing
//!   lines, CUDA/ggml error blocks, stderr excerpts;
//! - [`ntstatus`]: exit-code decoding (`0xc0000409` → abort/fast-fail);
//! - [`gpus`]: which GPUs a resolved command line needs;
//! - [`gguf`]: a GGUF header metadata reader;
//! - [`builtins`]: the shipped recipes;
//! - [`summary`]: the results table.

pub mod argv;
pub mod builtins;
pub mod command;
pub mod gguf;
pub mod gpus;
pub mod ntstatus;
pub mod parsers;
pub mod paste;
pub mod recipe;
pub mod summary;
pub mod template;

pub use recipe::{
    Expect, GgufSpec, HttpSpec, Param, ParserKind, PlannedStep, Recipe, RecipeError, Requires,
    Step, StepKind, Variant,
};
pub use template::{TemplateContext, TemplateError};
