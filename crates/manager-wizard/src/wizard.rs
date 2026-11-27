//! The `Wizard` step state machine and per-step time budgets.

use super::*;

/// Why a start-gateway Done is not (or not fully) trusted after the
/// saved configuration moved on.
const GATEWAY_DRIFT_NOTE: &str = "gateway configuration changed since the gateway was started";

/// Ordered auto-runner for the wizard steps.
///
/// Semantics:
/// - `run_all` runs `Queued` steps in declaration order, stopping at the
///   first failure or the first prerequisite-blocked step. Call it again
///   after `retry`/`skip`/`mark_manual` to resume.
/// - `run_step` runs one step on demand (still prerequisite-gated).
/// - In [`RunMode::Audit`], steps where `is_check()` is false are pre-marked
///   `Skipped`; the rest re-verify without reinstalling.
pub struct Wizard {
    steps: Vec<Box<dyn WizardStep>>,
    states: HashMap<String, StepState>,
    reports: HashMap<String, StepReport>,
    pub(crate) notes: HashMap<String, String>,
    mode: RunMode,
    started_at: Option<Instant>,
    /// Config edits returned by steps run under
    /// [`ConfigEditMode::ReturnToCaller`], in run order, keyed by step
    /// id; drained by [`Wizard::take_config_edits`].
    pending_config_edits: Vec<(String, Vec<ConfigEdit>)>,
}

impl Wizard {
    /// Build a runner. Duplicate step ids are rejected — silently dropping a
    /// step would be worse than failing to construct.
    pub fn new(steps: Vec<Box<dyn WizardStep>>, mode: RunMode) -> Result<Self, WizardError> {
        let mut seen = std::collections::HashSet::new();
        for s in &steps {
            if !seen.insert(s.id().to_string()) {
                return Err(WizardError::DuplicateStepId(s.id().to_string()));
            }
        }
        let mut states: HashMap<String, StepState> = HashMap::new();
        let mut notes: HashMap<String, String> = HashMap::new();
        for s in &steps {
            let id = s.id().to_string();
            if mode == RunMode::Audit && !s.is_check() {
                states.insert(id.clone(), StepState::Skipped);
                notes.insert(id, "install-only step; not run in audit mode".to_string());
            } else {
                states.insert(id, StepState::Queued);
            }
        }
        Ok(Self {
            steps,
            states,
            reports: HashMap::new(),
            notes,
            mode,
            started_at: None,
            pending_config_edits: Vec::new(),
        })
    }

    /// Drain the config edits that steps run since the last call
    /// returned under [`ConfigEditMode::ReturnToCaller`], as
    /// `(step id, edits)` in run order. The caller applies them under
    /// its config lock and reports a failed application with
    /// [`Wizard::fail_step`].
    pub fn take_config_edits(&mut self) -> Vec<(String, Vec<ConfigEdit>)> {
        std::mem::take(&mut self.pending_config_edits)
    }

    /// Record a step as failed after it ran — the caller could not
    /// persist the step's config edits. The step's report keeps its
    /// log, gains `log_line`, and carries `message`.
    pub fn fail_step(&mut self, id: &str, message: String, log_line: String) {
        if self.find(id).is_none() {
            return;
        }
        self.states.insert(id.to_string(), StepState::Failed);
        if let Some(report) = self.reports.get_mut(id) {
            report.state = StepState::Failed;
            report.message = message;
            report.log_tail.push(log_line);
            if report.log_tail.len() > LOG_TAIL_LINES {
                let drop = report.log_tail.len() - LOG_TAIL_LINES;
                report.log_tail.drain(..drop);
            }
        }
    }

    /// Step ids in declaration order.
    pub fn step_ids(&self) -> Vec<String> {
        self.steps.iter().map(|s| s.id().to_string()).collect()
    }

    pub fn mode(&self) -> RunMode {
        self.mode
    }

    pub fn state(&self, id: &str) -> Option<StepState> {
        self.states.get(id).copied()
    }

    pub fn note(&self, id: &str) -> Option<&str> {
        self.notes.get(id).map(String::as_str)
    }

    /// Remediation hint for a step (spec §4.1: offered, not forced, on audit
    /// failure).
    pub fn remediation_hint(&self, id: &str) -> Option<&str> {
        self.find(id)?.remediation_hint()
    }

    /// Help URL for a step, if the step offers one.
    pub fn help_url(&self, id: &str) -> Option<&str> {
        self.find(id)?.help_url()
    }

    pub(crate) fn find(&self, id: &str) -> Option<&dyn WizardStep> {
        self.steps.iter().find(|s| s.id() == id).map(|s| s.as_ref())
    }

    /// Public step lookup by id (spec §14 `setup_get_commands`: exposes
    /// `planned_commands()` for the "Show commands" UI).
    pub fn get_step(&self, id: &str) -> Option<&dyn WizardStep> {
        self.find(id)
    }

    fn touch_started(&mut self) {
        if self.started_at.is_none() {
            self.started_at = Some(Instant::now());
        }
    }

    /// Elapsed wall time since the first step started.
    pub fn elapsed(&self) -> Duration {
        self.started_at.map(|t| t.elapsed()).unwrap_or_default()
    }

    /// Rough ETA from mean completed-step duration × remaining steps.
    /// `None` before any step completes; `Some(ZERO)` when nothing remains.
    pub fn eta(&self) -> Option<Duration> {
        let done_ms: Vec<u64> = self
            .reports
            .values()
            .filter(|r| matches!(r.state, StepState::Done | StepState::Manual))
            .map(|r| r.duration_ms)
            .collect();
        if done_ms.is_empty() {
            return None;
        }
        let remaining = self
            .states
            .values()
            .filter(|s| matches!(s, StepState::Queued | StepState::Running))
            .count();
        if remaining == 0 {
            return Some(Duration::ZERO);
        }
        let avg = done_ms.iter().sum::<u64>() / done_ms.len() as u64;
        Some(Duration::from_millis(avg.saturating_mul(remaining as u64)))
    }

    /// Adopt check outcomes from another wizard's report (produced by
    /// an Audit-mode `run_all`) as this wizard's initial state, so the
    /// checklist reflects the machine across app restarts instead of
    /// resetting to all-Queued on every launch. Only steps whose check
    /// passed (Done/Manual) are adopted, and only into steps that are
    /// still Queued here — a step run in this session is never
    /// overwritten. A failing check is not adopted: an unsatisfied
    /// check means "not in place yet", which the checklist correctly
    /// shows as Queued rather than as a failed install attempt.
    ///
    /// One exception, gated on `gateway_config_current` (see
    /// [`gateway_config_is_current`]): the start-gateway audit check
    /// passes whenever *any* gateway answers the health probe,
    /// including a stale one started from an older configuration.
    /// Adopting that Done would mark the step complete without the
    /// config being regenerated or the gateway restarted, so when the
    /// on-disk gateway config has drifted from the current
    /// configuration the audit result for start-gateway is not
    /// adopted and the step stays Queued until it actually runs.
    pub fn hydrate(&mut self, report: &WizardReport, gateway_config_current: bool) {
        for step in &report.steps {
            if !matches!(step.state, StepState::Done | StepState::Manual) {
                continue;
            }
            let current = self.states.get(&step.id).copied();
            if step.id == steps::STEP_START_GATEWAY && !gateway_config_current {
                // The drift note explains why the step is not
                // adopted — but only when it is not adopted. If this
                // instance already holds Done for the step, the note
                // would contradict the displayed state.
                if !matches!(current, Some(StepState::Done | StepState::Manual)) {
                    self.notes
                        .insert(step.id.clone(), GATEWAY_DRIFT_NOTE.to_string());
                }
                continue;
            }
            if matches!(current, None | Some(StepState::Queued)) {
                self.states.insert(step.id.clone(), step.state);
                self.reports.insert(step.id.clone(), step.clone());
            }
        }
    }

    /// Adopt a previously persisted report (this wizard's own
    /// `report()` output, saved by the app layer) as this wizard's
    /// state, so setup progress survives an app restart: the user
    /// picks back up where they left off instead of facing an
    /// untouched checklist. Unlike [`Self::hydrate`] — which adopts
    /// only passing checks from a fresh audit — restore adopts every
    /// recorded outcome, including failures and skips: they are the
    /// user's progress, not machine checks. A step recorded as
    /// Running was interrupted by the app closing; it comes back
    /// Queued with a note saying so. Only steps still Queued here
    /// are adopted, unknown ids are ignored, and a report from the
    /// other mode is rejected wholesale (audit outcomes must go
    /// through `hydrate`, never restore). The one outcome restore
    /// does not adopt verbatim is a completed start-gateway when
    /// `gateway_config_current` is false. With the gateway stopped the
    /// step has to run again, so it comes back Queued with a note.
    /// While the gateway is running (`gateway_running`) the step did
    /// complete — pending changes are the activation plan's concern —
    /// so it stays Done with the note: a second reader of the saved
    /// state (another instance, or one that cannot read the gateway's
    /// credentials) must not turn a running gateway back into an
    /// unfinished step.
    pub fn restore(
        &mut self,
        report: &WizardReport,
        gateway_config_current: bool,
        gateway_running: bool,
    ) {
        if report.mode != self.mode {
            return;
        }
        for step in &report.steps {
            if self.find(&step.id).is_none() {
                continue;
            }
            let current = self.states.get(&step.id).copied();
            if !matches!(current, None | Some(StepState::Queued)) {
                continue;
            }
            if step.id == steps::STEP_START_GATEWAY
                && matches!(step.state, StepState::Done | StepState::Manual)
            {
                if !gateway_config_current && !gateway_running {
                    self.notes
                        .insert(step.id.clone(), GATEWAY_DRIFT_NOTE.to_string());
                    continue;
                }
                let mut r = step.clone();
                if !gateway_config_current {
                    r.note = Some(GATEWAY_DRIFT_NOTE.to_string());
                } else if r.note.as_deref() == Some(GATEWAY_DRIFT_NOTE) {
                    r.note = None;
                }
                self.states.insert(step.id.clone(), r.state);
                self.reports.insert(step.id.clone(), r);
                continue;
            }
            if step.state == StepState::Running {
                let mut r = step.clone();
                r.state = StepState::Queued;
                r.message = String::new();
                r.duration_ms = 0;
                r.log_tail = Vec::new();
                r.manual_command = None;
                r.note =
                    Some("interrupted — the app closed while this step was running".to_string());
                self.states.insert(step.id.clone(), StepState::Queued);
                self.reports.insert(step.id.clone(), r);
            } else {
                self.states.insert(step.id.clone(), step.state);
                self.reports.insert(step.id.clone(), step.clone());
            }
        }
    }

    /// (done, total) for the overall completion bar.
    pub fn progress(&self) -> (usize, usize) {
        self.report().progress()
    }

    /// Current report; steps never run are synthesized as `Queued`.
    pub fn report(&self) -> WizardReport {
        let steps = self
            .steps
            .iter()
            .map(|s| {
                if let Some(r) = self.reports.get(s.id()) {
                    r.clone()
                } else {
                    StepReport {
                        id: s.id().to_string(),
                        name: s.name().to_string(),
                        state: self
                            .states
                            .get(s.id())
                            .copied()
                            .unwrap_or(StepState::Queued),
                        message: String::new(),
                        duration_ms: 0,
                        log_tail: Vec::new(),
                        note: self.notes.get(s.id()).cloned(),
                        manual_command: None,
                    }
                }
            })
            .collect();
        WizardReport {
            mode: self.mode,
            steps,
        }
    }

    /// Run all `Queued` steps in order, honoring prerequisites. Stops at the
    /// first failure or the first blocked step; call again to resume after
    /// `retry` / `skip` / `mark_manual`.
    pub async fn run_all(&mut self, ctx: &WizardContext) -> WizardReport {
        self.run_all_inner(ctx, None).await
    }

    /// [`Self::run_all`] that also stops when `cancel` resolves: the
    /// running step is stopped exactly as a budget expiry stops it
    /// (its command's process tree killed, blocking work asked to
    /// stop) and recorded `Failed` with an operator-cancelled message,
    /// and no further step starts. The task registry's abort drives
    /// this (cooperative task cancellation).
    pub async fn run_all_until(
        &mut self,
        ctx: &WizardContext,
        cancel: Pin<&mut (dyn Future<Output = ()> + Send + '_)>,
    ) -> WizardReport {
        self.run_all_inner(ctx, Some(cancel)).await
    }

    async fn run_all_inner(
        &mut self,
        ctx: &WizardContext,
        mut cancel: Option<Pin<&mut (dyn Future<Output = ()> + Send + '_)>>,
    ) -> WizardReport {
        self.touch_started();
        let ids = self.step_ids();
        for id in ids {
            if self.states.get(&id).copied() != Some(StepState::Queued) {
                continue;
            }
            let prereqs: Vec<String> = self
                .find(&id)
                .map(|s| s.prerequisites())
                .unwrap_or_default();
            let blocked = prereqs.iter().find(|pre| {
                !self
                    .states
                    .get(pre.as_str())
                    .copied()
                    .map(StepState::satisfies_prerequisite)
                    .unwrap_or(false)
            });
            if blocked.is_some() {
                break; // leave Queued; UI shows "waiting on"
            }
            let (outcome, cancelled) = self
                .execute_step_inner(
                    ctx,
                    &id,
                    step_budget(&id),
                    cancel.as_mut().map(|c| c.as_mut()),
                )
                .await;
            if cancelled || outcome.state == StepState::Failed {
                break;
            }
        }
        self.report()
    }

    /// Run (or re-run) one step by id. Prerequisites still enforced.
    pub async fn run_step(
        &mut self,
        ctx: &WizardContext,
        id: &str,
    ) -> Result<StepOutcome, WizardError> {
        let prereqs: Vec<String> = self
            .find(id)
            .ok_or_else(|| WizardError::UnknownStep(id.to_string()))?
            .prerequisites();
        for pre in &prereqs {
            let ok = self
                .states
                .get(pre.as_str())
                .copied()
                .map(StepState::satisfies_prerequisite)
                .unwrap_or(false);
            if !ok {
                return Err(WizardError::PrerequisiteFailed {
                    step: id.to_string(),
                    waiting_on: pre.clone(),
                });
            }
        }
        self.touch_started();
        Ok(self.execute_step(ctx, id).await)
    }

    /// [`Self::run_step`] that also stops when `cancel` resolves (see
    /// [`Self::run_all_until`]). Returns the outcome and whether the
    /// step was stopped by `cancel`.
    pub async fn run_step_until(
        &mut self,
        ctx: &WizardContext,
        id: &str,
        cancel: Pin<&mut (dyn Future<Output = ()> + Send + '_)>,
    ) -> Result<(StepOutcome, bool), WizardError> {
        let prereqs: Vec<String> = self
            .find(id)
            .ok_or_else(|| WizardError::UnknownStep(id.to_string()))?
            .prerequisites();
        for pre in &prereqs {
            let ok = self
                .states
                .get(pre.as_str())
                .copied()
                .map(StepState::satisfies_prerequisite)
                .unwrap_or(false);
            if !ok {
                return Err(WizardError::PrerequisiteFailed {
                    step: id.to_string(),
                    waiting_on: pre.clone(),
                });
            }
        }
        self.touch_started();
        Ok(self
            .execute_step_inner(ctx, id, step_budget(id), Some(cancel))
            .await)
    }

    /// Reset a failed step to `Queued` so it can run again. No-op otherwise.
    pub fn retry(&mut self, id: &str) {
        if self.states.get(id).copied() == Some(StepState::Failed) {
            self.states.insert(id.to_string(), StepState::Queued);
            self.notes.remove(id);
        }
    }

    /// Mark a step skipped with a user note. Note: per spec §4.1,
    /// prerequisites require `Done` or `Manual`, so skipping a step blocks
    /// its dependents until they are re-pointed or the step is retried.
    pub fn skip(&mut self, id: &str, note: String) {
        if self.find(id).is_some() {
            self.states.insert(id.to_string(), StepState::Skipped);
            self.notes.insert(id.to_string(), note.clone());
            // Synthesize a report entry so the checklist shows the note.
            let name = self
                .find(id)
                .map(|s| s.name().to_string())
                .unwrap_or_default();
            self.reports.insert(
                id.to_string(),
                StepReport {
                    id: id.to_string(),
                    name,
                    state: StepState::Skipped,
                    message: "skipped by user".to_string(),
                    duration_ms: 0,
                    log_tail: Vec::new(),
                    note: Some(note),
                    manual_command: None,
                },
            );
        }
    }

    /// Mark a step as manually completed (user installed it themselves).
    /// `Manual` satisfies prerequisites.
    pub fn mark_manual(&mut self, id: &str, path: String, version: String) {
        if self.find(id).is_some() {
            self.states.insert(id.to_string(), StepState::Manual);
            let note = format!("{path} @ {version}");
            self.notes.insert(id.to_string(), note.clone());
            let name = self
                .find(id)
                .map(|s| s.name().to_string())
                .unwrap_or_default();
            self.reports.insert(
                id.to_string(),
                StepReport {
                    id: id.to_string(),
                    name,
                    state: StepState::Manual,
                    message: "marked as manually installed".to_string(),
                    duration_ms: 0,
                    log_tail: Vec::new(),
                    note: Some(note),
                    manual_command: None,
                },
            );
        }
    }

    async fn execute_step(&mut self, ctx: &WizardContext, id: &str) -> StepOutcome {
        self.execute_step_inner(ctx, id, step_budget(id), None)
            .await
            .0
    }

    #[cfg(test)]
    pub(crate) async fn execute_step_with_budget(
        &mut self,
        ctx: &WizardContext,
        id: &str,
        budget: Duration,
    ) -> StepOutcome {
        self.execute_step_inner(ctx, id, budget, None).await.0
    }

    /// Run one step under `budget`, also stopping when `cancel`
    /// resolves. Returns the outcome and whether `cancel` stopped it.
    async fn execute_step_inner(
        &mut self,
        ctx: &WizardContext,
        id: &str,
        budget: Duration,
        cancel: Option<Pin<&mut (dyn Future<Output = ()> + Send + '_)>>,
    ) -> (StepOutcome, bool) {
        // Defensive: both callers resolve the step via find() first, so this
        // is unreachable in practice — but an unknown id must fail closed,
        // not panic.
        if self.find(id).is_none() {
            return (
                StepOutcome::failed(
                    format!("unknown step: {id}"),
                    vec!["execute_step called for an unregistered step".to_string()],
                ),
                false,
            );
        }
        let step_name = self
            .find(id)
            .map(|s| s.name().to_string())
            .unwrap_or_default();
        // A note explains why a step is sitting Queued (hydration
        // drift, an interruption, a user skip). Once the step
        // actually runs, the note's purpose is spent and its text
        // must not ride along on the fresh outcome: a start-gateway
        // run that failed inside PostgreSQL still displayed the
        // pre-run "gateway configuration changed" note next to the
        // failure, pointing at config drift as though it were the
        // cause (field report 2026-10-18). Notes set by skip /
        // mark_manual are unaffected — those paths synthesize their
        // reports directly and never pass through here.
        self.notes.remove(id);
        self.states.insert(id.to_string(), StepState::Running);
        ctx.ops.note_step_started();
        let started = Instant::now();
        // The borrow from the finds above has ended; re-resolve for the run.
        // The None arm is unreachable (checked above) but handled anyway.
        // The timeout drops the step's future on expiry: any command
        // it was awaiting is dropped with it (and its process tree
        // killed — see `run_command`), and blocking work that a
        // dropped future cannot cancel (archive extraction) is asked
        // to stop via request_cancellation, so a timed-out step frees
        // the wizard instead of wedging it for the rest of the session.
        // An operator cancellation (`cancel`) stops the step the same
        // way: the step future is dropped and blocking work is asked
        // to stop.
        let mut cancelled = false;
        let raw = match self.find(id) {
            Some(step) => {
                let timed = tokio::time::timeout(budget, step.run(ctx));
                let stopped = async {
                    match cancel {
                        Some(c) => c.await,
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    biased;
                    _ = stopped => {
                        cancelled = true;
                        ctx.ops.request_cancellation();
                        Ok(StepOutcome::failed(
                            format!("step '{step_name}' was cancelled by the operator"),
                            vec![
                                "the step was cancelled and stopped; re-run the step to retry"
                                    .to_string(),
                            ],
                        ))
                    }
                    res = timed => match res {
                        Ok(r) => r,
                        Err(_) => {
                            ctx.ops.request_cancellation();
                            Ok(StepOutcome::failed(
                                format!(
                                    "step '{step_name}' timed out after {} and was stopped",
                                    budget_label(budget)
                                ),
                                vec![format!(
                                    "the step exceeded its {} budget and was aborted; \
                                     re-run the step to retry",
                                    budget_label(budget)
                                )],
                            ))
                        }
                    },
                }
            }
            None => {
                return (
                    StepOutcome::failed(
                        format!("unknown step: {id}"),
                        vec!["step unregistered during execution".to_string()],
                    ),
                    false,
                )
            }
        };
        let mut outcome = match raw {
            Ok(o) => o,
            Err(e) => StepOutcome::failed(format!("step error: {e}"), vec![format!("{e:?}")]),
        };
        // Runner-measured wall time is authoritative.
        outcome.duration_ms = started.elapsed().as_millis() as u64;
        if ctx.config_edit_mode == ConfigEditMode::ReturnToCaller
            && !outcome.config_edits.is_empty()
        {
            self.pending_config_edits
                .push((id.to_string(), outcome.config_edits.clone()));
        }
        // The report (UI display, persisted wizard state) carries
        // only the display tail; the outcome keeps the full log for
        // the caller's log persistence.
        let report_tail = if outcome.log_tail.len() > LOG_TAIL_LINES {
            let drop = outcome.log_tail.len() - LOG_TAIL_LINES;
            outcome.log_tail.iter().skip(drop).cloned().collect()
        } else {
            outcome.log_tail.clone()
        };
        self.states.insert(id.to_string(), outcome.state);
        self.reports.insert(
            id.to_string(),
            StepReport {
                id: id.to_string(),
                name: step_name,
                state: outcome.state,
                message: outcome.message.clone(),
                duration_ms: outcome.duration_ms,
                log_tail: report_tail,
                note: self.notes.get(id).cloned(),
                manual_command: outcome.manual_command.clone(),
            },
        );
        (outcome, cancelled)
    }
}

/// Wall-clock budget for one step execution. Before this bound
/// existed, a single stalled child process (a hung `uv pip install`,
/// a Prisma pre-warm that never returned) held the wizard lock for
/// the rest of the session, and every later step run reported
/// "busy" with no recovery short of restarting the app.
///
/// Coherence rule: a step's budget must exceed the worst-case
/// sequential sum of its sub-budgets — a budget smaller than the
/// work it bounds kills legitimate runs mid-flight and misreports
/// them as stalls. v0.1.32 violated this for start-gateway (600 s
/// total against 900 s sub-budgets for the pin install, the
/// generate, and the package setup, each of which the command
/// runner also capped at 600 s), so a legitimate cold repair chain
/// could not finish in one run (field evidence 2026-10-16: a run
/// died at exactly 600003 ms mid-repair). The start-gateway figure
/// covers the cold chain end to end: venv create (900) + package
/// install (900) + Prisma pin install (900) + generate (900) +
/// probes and the health window (~600).
fn step_budget(id: &str) -> Duration {
    match id {
        steps::STEP_START_GATEWAY => Duration::from_secs(4200),
        _ => Duration::from_secs(1200),
    }
}

/// "10 minutes" / "45 seconds" for timeout messages.
fn budget_label(budget: Duration) -> String {
    let secs = budget.as_secs();
    if secs != 0 && secs.is_multiple_of(60) {
        let mins = secs / 60;
        if mins == 1 {
            "1 minute".to_string()
        } else {
            format!("{mins} minutes")
        }
    } else {
        format!("{secs} seconds")
    }
}
