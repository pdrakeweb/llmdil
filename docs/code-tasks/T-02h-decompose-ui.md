# T-02h — Decompose `ui/src/views/static.ts` and `ui/src/api.ts`

**Task ID:** T-02h (subtask of T-02)
**Category:** decomposition
**Blast radius:** `ui/src/`
**Est. effort:** L

## 1. Objective

Reduce `views/static.ts` (8,816 lines) and `api.ts` (3,868 lines) to **≤ 2,000 lines each** by
moving their function/type definitions into cohesive modules, preserving all signatures, names,
and public API via re-exports.

## 2. Current state (measured, clean checkout 2026-10-22)

| File | Lines | Key contents |
|------|-------|--------------|
| `ui/src/views/static.ts` | 8,816 | All static view components (React functional components) |
| `ui/src/api.ts` | 3,868 | Hand-written TypeScript interfaces for all 130 ops |

## 3. Decomposition plan (concrete, module-by-module)

### 3.1 `views/static.ts` decomposition

Group by view domain:

| Module | Target lines | Content | Re-export from `static.ts` |
|--------|-------------|---------|---------------------------|
| `static.ts` (core) | ~1,500 | shared utilities (`formatNumber`, `formatBytes`, `formatDate`, `formatDuration`, `formatStatus`, `formatSize`, `formatTime`, `formatProgress`, `formatError`) | `export { formatNumber, formatBytes, formatDate, formatDuration, formatStatus, formatSize, formatTime, formatProgress, formatError };` |
| `views/DashboardView.tsx` | ~1,000 | `DashboardView`, `DashboardStats`, `DashboardChart`, `DashboardTable` | `export { DashboardView };` |
| `views/TasksView.tsx` | ~1,000 | `TasksView`, `TaskList`, `TaskItem`, `TaskDetail`, `TaskForm`, `TaskFormState`, `TaskFormError`, `TaskFormSuccess` | `export { TasksView };` |
| `views/DownloadsView.tsx` | ~1,000 | `DownloadsView`, `DownloadList`, `DownloadItem`, `DownloadForm`, `DownloadFormState`, `DownloadFormError`, `DownloadFormSuccess` | `export { DownloadsView };` |
| `views/ModelsView.tsx` | ~1,000 | `ModelsView`, `ModelList`, `ModelItem`, `ModelForm`, `ModelFormState`, `ModelFormError`, `ModelFormSuccess` | `export { ModelsView };` |
| `views/UsersView.tsx` | ~1,000 | `UsersView`, `UserList`, `UserItem`, `UserForm`, `UserFormState`, `UserFormError`, `UserFormSuccess` | `export { UsersView };` |
| `views/ConfigView.tsx` | ~1,000 | `ConfigView`, `ConfigForm`, `ConfigFormState`, `ConfigFormError`, `ConfigFormSuccess` | `export { ConfigView };` |
| `views/AdminView.tsx` | ~1,000 | `AdminView`, `AdminForm`, `AdminFormState`, `AdminFormError`, `AdminFormSuccess` | `export { AdminView };` |
| `views/WizardView.tsx` | ~1,000 | `WizardView`, `WizardStep`, `WizardStepState`, `WizardStepError`, `WizardStepSuccess` | `export { WizardView };` |

*Note:* Each `.tsx` file should export one main component. Keep all component props, state,
and helpers in the same file for cohesion.

### 3.2 `api.ts` decomposition

Group by domain:

| Module | Target lines | Content | Re-export from `api.ts` |
|--------|-------------|---------|------------------------|
| `api.ts` (core) | ~500 | shared types (`ApiError`, `ApiResult`, `ApiRequest`, `ApiResponse`) | `export { ApiError, ApiResult, ApiRequest, ApiResponse };` |
| `api/ops.ts` | ~3,000 | op-specific request/response types (one interface per op) | `export { GetTasksRequest, GetTasksResponse, StartTaskRequest, StartTaskResponse, … };` |
| `api/admin.ts` | ~300 | admin API types (`AdminAuthRequest`, `AdminAuthResponse`, `AdminSettingsRequest`, `AdminSettingsResponse`) | `export { AdminAuthRequest, AdminAuthResponse, AdminSettingsRequest, AdminSettingsResponse };` |

`api/ops.ts` should have one interface per op name (e.g. `GetTasksRequest`, `GetTasksResponse`,
`StartTaskRequest`, `StartTaskResponse`, …). Use the same names that the registry declares
for op arguments.

## 4. Implementation steps

1. Create `ui/src/views/` submodules (`DashboardView.tsx`, `TasksView.tsx`, …) as above.
2. Create `ui/src/api/` submodules (`ops.ts`, `admin.ts`).
3. Move component/function definitions into each file.
4. Update imports in `ui/src/main.ts` and other view files to use new paths.
5. Re-export from `static.ts` and `api.ts` (`export { DashboardView } from './views/DashboardView';`).
6. Run `npm run build` and `npm test` (or equivalent) to verify no breakage.
7. Run `npm run lint` to ensure no style violations.

## 5. Definition of done

- [ ] `views/static.ts` ≤ 2,000 lines.
- [ ] `api.ts` ≤ 2,000 lines.
- [ ] Every new module compiles cleanly (`npm run build`).
- [ ] `npm test` passes unchanged.
- [ ] Public API unchanged: all imports work as before.
- [ ] No test renamed, deleted, or weakened.

## 6. Risks & pitfalls

- **Component names.** Do not change component names (`DashboardView`, `TasksView`, …) — they
  are referenced in router config and tests.
- **API type names.** Do not change type names (`GetTasksRequest`, `StartTaskResponse`, …) —
  they are referenced in UI components.
- **TypeScript strictness.** Ensure `tsconfig.json` strict mode stays enabled; do not relax
  type rules to compensate for refactoring.
