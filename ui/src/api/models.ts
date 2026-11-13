import { errOf, invoke, isTauri } from "./core";
import type { CallResult } from "./core";

/* ---------------- Models ---------------- */

export interface ModelRow {
  id: string;
  /** Short display name for compact controls (the curated catalog
   *  name when the model has one; the id otherwise). */
  name: string;
  gguf_path: string;
  quant: string;
  params_b: number;
  sha256: string;
  verified_at: string | null;
  /** Whether the setup wizard's download step includes this model. */
  enabled: boolean;
  on_disk: boolean;
  size_bytes: number | null;
  /** True while a download for this model is in flight (backend-tracked,
   *  so it is accurate on first paint, not just after a local click). */
  downloading: boolean;
  /** True after a download's transfer has finished while the backend
   *  watcher verifies the file against the pinned SHA-256. The file is
   *  not a plain complete state until this clears. */
  verifying: boolean;
  /** True while an in-flight download is paused (worker parked, partial
   *  file kept for resume). */
  paused: boolean;
  /** Size of the partial (.part) file when an incomplete download exists,
   *  else null. The final path only ever holds complete files, so a
   *  nonzero partial_bytes with on_disk=false means "partial download". */
  partial_bytes: number | null;
  /** Last terminal download failure for this model in this session
   *  (transport error, or a hash mismatch — which also removes the
   *  downloaded file), else null. */
  download_error: string | null;
  /** Curated catalog size for this model when known. An ESTIMATE, used
   *  only for display ("~X GB (estimate)"), never for verification. */
  catalog_size_bytes: number | null;
  /** The model's rated maximum context length in tokens, from the
   *  curated catalog (null for models outside it). The backend flags
   *  editor annotates context presets beyond this value. */
  ctx_max?: number | null;
  /** The operator's MoE declaration on the model record: the MoE
   *  layer count and the default offload layers declared in the
   *  Add/Edit model forms. Null when never declared. */
  moe_layers?: number | null;
  default_offload_layers?: number | null;
  /** The curated catalog's pins for the same figures (null for
   *  models outside the catalog, or when the catalog pins
   *  neither). A catalog pin wins over the record's declaration
   *  wherever both exist. */
  catalog_moe_layers?: number | null;
  catalog_default_offload_layers?: number | null;
  /** The operator's MTP capability declaration on the model
   *  record: whether the model's GGUF carries an MTP head.
   *  Null when never declared. */
  mtp?: boolean | null;
  /** The record's source URL. The Edit form's re-fetch
   *  (design Part 2, P2-D8) recovers the HF repo and file
   *  from it and is offered only for HF resolve URLs. */
  source_url: string;
  /** The on-disk file's parsed MoE block count (null when
   *  the file is absent or unparseable — an unknown, not a
   *  zero) and its in-file MTP signal. */
  file_moe_layers?: number | null;
  file_nextn?: boolean | null;
  /** The post-download reconciliation proposal (P2-D7):
   *  the stored MoE declaration vs the file's parsed count,
   *  present only when they disagree and no catalog pin
   *  governs the figure. */
  moe_reconcile?: { declared: number; file: number } | null;
  /** The curated catalog's MTP pin for the model (null when the
   *  catalog knows neither the id nor the source URL). A pin
   *  wins over the record's declaration wherever both exist —
   *  the EFFECTIVE capability is `catalog_mtp ?? mtp ?? false`. */
  catalog_mtp?: boolean | null;
  fit_check_available: boolean;
}

/** list_models — configured models with on-disk presence and size. */
export async function getModels(): Promise<CallResult<ModelRow[]>> {
  if (!isTauri()) return { data: [], live: false };
  try {
    const r = await invoke<{ models: (ModelRow & { verifying?: boolean })[] }>("list_models");
    const rows = Array.isArray(r.models) ? r.models : [];
    return { data: rows.map((m) => ({ ...m, verifying: Boolean(m.verifying) })), live: true };
  } catch (e) {
    return { data: [], live: true, error: errOf(e) };
  }
}

/** download_model — spawn a model download and return immediately with
 *  the destination path; the transfer runs in a background worker that
 *  resumes from any partial (.part) file. When the transfer completes, a
 *  backend watcher verifies the file against the pinned SHA-256 (when a
 *  real pin exists) and records verified_at; a mismatch removes the file
 *  and surfaces as the row's download_error. */
export async function downloadModel(id: string): Promise<CallResult<{ model_id: string; dest: string }>> {
  if (!isTauri()) return { data: { model_id: id, dest: "" }, live: false };
  try {
    const data = await invoke<{ model_id: string; dest: string }>("download_model", { id });
    return { data, live: true };
  } catch (e) {
    return { data: { model_id: id, dest: "" }, live: true, error: errOf(e) };
  }
}

/** cancel_download — abort an in-flight download (the partial file is
 *  discarded). Errors with "no download in flight" when the download has
 *  already finished. */
export async function cancelDownload(id: string): Promise<CallResult<{ model_id: string; cancelled: boolean }>> {
  if (!isTauri()) return { data: { model_id: id, cancelled: true }, live: false };
  try {
    const data = await invoke<{ model_id: string; cancelled: boolean }>("cancel_download", { id });
    return { data, live: true };
  } catch (e) {
    return { data: { model_id: id, cancelled: false }, live: true, error: errOf(e) };
  }
}

/** model_pause_download — pause an in-flight download; the worker parks
 *  and the partial (.part) file is kept for resume. */
export async function pauseModelDownload(id: string): Promise<CallResult<{ model_id: string; paused: boolean }>> {
  if (!isTauri()) return { data: { model_id: id, paused: false }, live: false, error: "model_pause_download is not available in sample mode" };
  try {
    const data = await invoke<{ model_id: string; paused: boolean }>("model_pause_download", { id });
    return { data, live: true };
  } catch (e) {
    return { data: { model_id: id, paused: false }, live: true, error: errOf(e) };
  }
}

/** model_resume_download — resume a paused download from its partial file. */
export async function resumeModelDownload(id: string): Promise<CallResult<{ model_id: string; resumed: boolean }>> {
  if (!isTauri()) return { data: { model_id: id, resumed: false }, live: false, error: "model_resume_download is not available in sample mode" };
  try {
    const data = await invoke<{ model_id: string; resumed: boolean }>("model_resume_download", { id });
    return { data, live: true };
  } catch (e) {
    return { data: { model_id: id, resumed: false }, live: true, error: errOf(e) };
  }
}

/** model_verify_checksum — SHA-256 the file on disk against the pinned hash. */
export async function verifyModel(id: string): Promise<CallResult<{ id: string; ok: boolean }>> {
  if (!isTauri()) return { data: { id, ok: false }, live: false };
  try {
    const data = await invoke<{ id: string; ok: boolean }>("model_verify_checksum", { id });
    return { data, live: true };
  } catch (e) {
    return { data: { id, ok: false }, live: true, error: errOf(e) };
  }
}

/** model_set_short_name — set the model's operator-chosen short
 *  display name (persisted override; wins over the catalog name in
 *  every short-name display). The model id never changes. */
export async function modelSetShortName(id: string, name: string): Promise<CallResult<{ id: string; name: string }>> {
  if (!isTauri()) return { data: { id, name }, live: false };
  try {
    const data = await invoke<{ id: string; name: string }>("model_set_short_name", { id, name });
    return { data, live: true };
  } catch (e) {
    return { data: { id, name }, live: true, error: errOf(e) };
  }
}

/** model_set_enabled — include/exclude a model from wizard downloads.
 *  The model's file and configuration are kept either way. */
export async function modelSetEnabled(id: string, enabled: boolean): Promise<CallResult<{ model_id: string; enabled: boolean }>> {
  if (!isTauri()) return { data: { model_id: id, enabled }, live: false };
  try {
    const data = await invoke<{ model_id: string; enabled: boolean }>("model_set_enabled", { id, enabled });
    return { data, live: true };
  } catch (e) {
    return { data: { model_id: id, enabled: !enabled }, live: true, error: errOf(e) };
  }
}

/** model_add — register a new model by id and GGUF source URL. File
 *  name, quant, and paramsB are optional (the file name defaults to
 *  the URL's last path segment). paramsB is the total parameter count
 *  in billions: the command resolves it from the curated catalog
 *  when omitted and refuses the add when no source yields a count.
 *  moeLayers/defaultOffloadLayers are the operator MoE declaration
 *  (both or neither): they give a model outside the curated catalog
 *  expert-offload controls. mtp is the operator MTP capability
 *  declaration: it gives a model outside the catalog the MTP
 *  draft control. sha256 is the optional hash pin (design
 *  Part 2, P2-D6): an HF-picked single-file add passes the
 *  file's lfs.sha256 and the record starts pinned; without it
 *  the model starts enabled with no pinned hash. */
export async function modelAdd(id: string, sourceUrl: string, fileName?: string, quant?: string, paramsB?: number, moeLayers?: number, defaultOffloadLayers?: number, mtp?: boolean, sha256?: string): Promise<CallResult<{ model_id: string }>> {
  if (!isTauri()) return { data: { model_id: id }, live: false };
  try {
    const data = await invoke<{ model_id: string }>("model_add", {
      id,
      sourceUrl,
      fileName: fileName ?? null,
      quant: quant ?? null,
      paramsB: paramsB ?? null,
      moeLayers: moeLayers ?? null,
      defaultOffloadLayers: defaultOffloadLayers ?? null,
      mtp: mtp ?? null,
      sha256: sha256 ?? null,
    });
    return { data, live: true };
  } catch (e) {
    return { data: { model_id: id }, live: true, error: errOf(e) };
  }
}

/** One quant row of an HF lookup: the file, its exact size,
 *  the fit verdict token against the live free-VRAM snapshot,
 *  and the prefill derivations (id slug, quant token, and —
 *  for single-file quants — the LFS hash that pins the add). */
export interface HfLookupQuant {
  file: string;
  size_bytes: number;
  shard_count: number;
  sha256: string | null;
  fit: string;
  id_slug: string;
  quant: string | null;
  /** The constructed resolve URL a pick prefills as the
   *  model's source URL (§P2.5). */
  source_url: string;
}

/** One repo row of an HF lookup (design Part 2, §P2.4). */
export interface HfLookupRepo {
  repo_id: string;
  downloads: number;
  likes: number;
  architecture: string | null;
  params_b: number | null;
  ctx_max: number | null;
  /** The curated-catalog model whose file lives in this
   *  repo, when one does — the picker's Catalog chip. */
  catalog_model_id: string | null;
  quants: HfLookupQuant[];
}

/** model_hf_lookup — the Add Model form's shallow HF lookup
 *  (P2-D2): repo rows with per-repo facts and quant files.
 *  A failed search is an error; the manual form never
 *  depends on it. */
export async function modelHfLookup(query: string): Promise<CallResult<{ repos: HfLookupRepo[] }>> {
  if (!isTauri()) return { data: { repos: [] }, live: false };
  try {
    const data = await invoke<{ suggest: boolean; repos: HfLookupRepo[] }>("model_hf_lookup", {
      query,
      suggest: null,
    });
    return { data: { repos: data.repos }, live: true };
  } catch (e) {
    return { data: { repos: [] }, live: true, error: errOf(e) };
  }
}

/** model_hf_lookup (suggest) — the name field's type-ahead:
 *  repo names + download counts from the same shallow
 *  search, with no record fetches. */
export async function modelHfSuggest(query: string): Promise<CallResult<{ repos: { repo_id: string; downloads: number }[] }>> {
  if (!isTauri()) return { data: { repos: [] }, live: false };
  try {
    const data = await invoke<{ suggest: boolean; repos: { repo_id: string; downloads: number }[] }>("model_hf_lookup", {
      query,
      suggest: true,
    });
    return { data: { repos: data.repos }, live: true };
  } catch (e) {
    return { data: { repos: [] }, live: true, error: errOf(e) };
  }
}

/** The parsed-header facts a pick-time read returns. */
export interface HfHeaderFacts {
  moe_layers: number | null;
  block_count: number | null;
  expert_bytes: number;
  total_bytes: number;
  mtp: boolean;
  suggested_default_offload_layers: number | null;
  needs_split: boolean;
}

/** model_hf_header's payload. `header` is null when the
 *  range read failed for any reason — the form proceeds
 *  without geometry prefill; the add is never blocked. */
export interface HfHeaderResult {
  repo_id: string;
  file: string;
  /** This exact file is a curated-catalog file: its pins
   *  govern and no fetched value is prefilled over them. */
  catalog: boolean;
  /** The record's current lfs.sha256 for the file, when the
   *  record reports one (the re-fetch's changed-hash check). */
  sha256: string | null;
  header: HfHeaderFacts | null;
}

/** model_hf_header — the pick-time GGUF header range read
 *  (P2-D4) plus the record's current hash for the file. */
export async function modelHfHeader(repoId: string, file: string): Promise<CallResult<HfHeaderResult>> {
  const empty: HfHeaderResult = { repo_id: repoId, file, catalog: false, sha256: null, header: null };
  if (!isTauri()) return { data: empty, live: false };
  try {
    const data = await invoke<HfHeaderResult>("model_hf_header", { repoId, file });
    return { data, live: true };
  } catch (e) {
    return { data: empty, live: true, error: errOf(e) };
  }
}

/** model_set_offload_spec — set or clear a model's operator MoE
 *  declaration (the MoE layer count + default offload layers).
 *  Pass both null to clear the declaration. */
export async function modelSetOffloadSpec(id: string, moeLayers: number | null, defaultOffloadLayers: number | null): Promise<CallResult<{ id: string; moe_layers: number | null; default_offload_layers: number | null }>> {
  if (!isTauri()) return { data: { id, moe_layers: moeLayers, default_offload_layers: defaultOffloadLayers }, live: false };
  try {
    const data = await invoke<{ id: string; moe_layers: number | null; default_offload_layers: number | null }>("model_set_offload_spec", {
      id,
      moeLayers,
      defaultOffloadLayers,
    });
    return { data, live: true };
  } catch (e) {
    return { data: { id, moe_layers: moeLayers, default_offload_layers: defaultOffloadLayers }, live: true, error: errOf(e) };
  }
}

/** model_set_mtp — set or clear a model's operator MTP
 *  capability declaration. Pass null to clear the declaration. */
export async function modelSetMtp(id: string, mtp: boolean | null): Promise<CallResult<{ id: string; mtp: boolean | null }>> {
  if (!isTauri()) return { data: { id, mtp }, live: false };
  try {
    const data = await invoke<{ id: string; mtp: boolean | null }>("model_set_mtp", {
      id,
      mtp,
    });
    return { data, live: true };
  } catch (e) {
    return { data: { id, mtp }, live: true, error: errOf(e) };
  }
}

/** model_delete — remove a model's file (and any partial .part file) and
 *  its config entry; the removal is persisted. Refused while a download
 *  is in flight for the model or while any backend references it. */
export async function deleteModel(id: string): Promise<CallResult<{ model_id: string; deleted: boolean; bytes_freed: number }>> {
  if (!isTauri()) return { data: { model_id: id, deleted: true, bytes_freed: 0 }, live: false };
  try {
    const data = await invoke<{ model_id: string; deleted: boolean; bytes_freed: number }>("model_delete", { id });
    return { data, live: true };
  } catch (e) {
    return { data: { model_id: id, deleted: false, bytes_freed: 0 }, live: true, error: errOf(e) };
  }
}

/* ---------------- Placement ---------------- */

export interface ModelAssignResult {
  task_id: string;
  /** True only when the assignment was saved to the configuration file. */
  persisted: boolean;
}

/** model_assign — assign a model to a backend: sets the backend's
 *  model_file to the model's gguf_path, applies the tensor split, and
 *  persists. This is the one command that aligns a backend with a
 *  model; the Placement flow calls it when the selected backend does
 *  not already resolve to the selected model. The split is validated
 *  against the discovered GPU count by the command. */
export async function modelAssign(backendId: string, modelId: string, split: number[]): Promise<CallResult<ModelAssignResult>> {
  const fallback: ModelAssignResult = { task_id: "", persisted: false };
  if (!isTauri()) return { data: fallback, live: false, error: "model_assign is not available in sample mode" };
  try {
    const data = await invoke<ModelAssignResult>("model_assign", { id: modelId, backendId, split });
    return { data, live: true };
  } catch (e) {
    return { data: fallback, live: true, error: errOf(e) };
  }
}
