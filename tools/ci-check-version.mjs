#!/usr/bin/env node
// Version-drift gate (T-04). Every place the release version is stamped
// must agree; CI fails on the first mismatch so a release can never ship
// with, say, a bumped tauri.conf.json but a stale Cargo manifest.
//
// Sources checked (all relative to the repo root):
//   - crates/*/Cargo.toml            [package] version (workspace members)
//   - src-tauri/Cargo.toml           [package] version
//   - src-tauri/tauri.conf.json      top-level "version"
//   - ui/package.json                "version"
//   - ui/package-lock.json           root "version" and packages[""].version
//   - README.md                      "Current release: **vX.Y.Z**"
//   - Cargo.lock, src-tauri/Cargo.lock
//                                    every local (source-less) [[package]]
//
// When run on a tag build (GITHUB_REF_TYPE=tag), the tag must also be
// v<version>. An explicit expected version may be passed as argv[2]
// (with or without a leading "v").
//
// Dependency-free Node (>= 18) so it runs unchanged on the ubuntu and
// windows runners and locally: `node tools/ci-check-version.mjs`.

import { readFileSync, readdirSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const found = []; // { source, version }
const errors = [];

const read = (rel) => readFileSync(join(root, rel), "utf8").replace(/\r\n/g, "\n");
const record = (source, version) => {
  if (!version) errors.push(`${source}: no version found`);
  else found.push({ source, version });
};

// [package] version from a Cargo manifest (literal string only; a
// `version.workspace = true` would need the workspace version, which
// this repo does not use — flag it rather than guess).
function cargoPackageVersion(rel) {
  const text = read(rel);
  let inPackage = false;
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (line.startsWith("[")) {
      inPackage = line === "[package]";
      continue;
    }
    if (!inPackage) continue;
    const m = line.match(/^version\s*=\s*"([^"]+)"/);
    if (m) return m[1];
    if (/^version(\.workspace|\s*=\s*\{)/.test(line)) {
      errors.push(`${rel}: inherited version is not supported by this check`);
      return "";
    }
  }
  return "";
}

// Local packages in a Cargo.lock are the ones without a `source` line.
function lockLocalPackages(rel) {
  const out = [];
  for (const block of read(rel).split("[[package]]").slice(1)) {
    const name = block.match(/^name = "([^"]+)"/m)?.[1];
    const version = block.match(/^version = "([^"]+)"/m)?.[1];
    if (!/^source = /m.test(block)) out.push({ name, version });
  }
  return out;
}

// --- Cargo manifests -------------------------------------------------------
const cratesDir = join(root, "crates");
for (const name of readdirSync(cratesDir).sort()) {
  const manifest = join("crates", name, "Cargo.toml");
  if (existsSync(join(root, manifest))) {
    record(manifest.replace(/\\/g, "/"), cargoPackageVersion(manifest));
  }
}
record("src-tauri/Cargo.toml", cargoPackageVersion("src-tauri/Cargo.toml"));

// --- JSON manifests --------------------------------------------------------
record("src-tauri/tauri.conf.json", JSON.parse(read("src-tauri/tauri.conf.json")).version);
record("ui/package.json", JSON.parse(read("ui/package.json")).version);
const lock = JSON.parse(read("ui/package-lock.json"));
record("ui/package-lock.json (version)", lock.version);
record('ui/package-lock.json (packages[""])', lock.packages?.[""]?.version);

// --- README ----------------------------------------------------------------
record(
  "README.md (Current release)",
  read("README.md").match(/Current release:\s*\*\*v(\d+\.\d+\.\d+[^*\s]*)\*\*/)?.[1],
);

// --- Lockfiles -------------------------------------------------------------
for (const lockRel of ["Cargo.lock", "src-tauri/Cargo.lock"]) {
  const locals = lockLocalPackages(lockRel);
  if (locals.length === 0) errors.push(`${lockRel}: no local packages found`);
  for (const { name, version } of locals) record(`${lockRel} (${name})`, version);
}

// --- Expected version: argv, else the workspace's first manifest ----------
let expected = (process.argv[2] ?? "").replace(/^v/, "");
if (process.env.GITHUB_REF_TYPE === "tag" && process.env.GITHUB_REF_NAME) {
  const tagVersion = process.env.GITHUB_REF_NAME.replace(/^v/, "");
  record(`git tag ${process.env.GITHUB_REF_NAME}`, tagVersion);
}
if (!expected) expected = found.find((f) => f.source === "ui/package.json")?.version ?? "";

const mismatches = found.filter((f) => f.version !== expected);
for (const f of found) {
  const mark = f.version === expected ? "ok  " : "DIFF";
  console.log(`${mark} ${f.version.padEnd(10)} ${f.source}`);
}
for (const e of errors) console.error(`error: ${e}`);
if (mismatches.length || errors.length || !expected) {
  console.error(
    `\nversion drift: expected ${expected || "(unknown)"}; ` +
      `${mismatches.length} mismatching source(s), ${errors.length} error(s).`,
  );
  process.exit(1);
}
console.log(`\nall ${found.length} version sources agree on ${expected}`);
