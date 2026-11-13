//! Setup-wizard subsystem state: the install and audit wizards
//! (lazily built), the last checklist payload, and the step that is
//! currently executing.
//!
//! Lock order (see the [`crate::state`] module docs): a wizard guard
//! is taken BEFORE the config lock (wizard → config, PS-4) and
//! before `steps_cache` (wizard → steps cache). `step_running` is a
//! std lock that is only set/cleared/read synchronously.

use manager_wizard::{steps::default_steps, RunMode, Wizard};
use serde_json::Value;
use tokio::sync::{Mutex, OnceCell, RwLock};

/// The setup wizard's state.
#[derive(Default)]
pub(crate) struct WizardState {
    install: OnceCell<Mutex<Wizard>>,
    audit: OnceCell<Mutex<Wizard>>,
    /// Last setup_get_steps payload, refreshed whenever the install
    /// wizard's state is read or mutated under its lock. Served when
    /// the wizard lock is held by a running step, so the checklist
    /// never sits blank for the duration of a step.
    pub(crate) steps_cache: RwLock<Option<Value>>,
    /// The install-wizard step currently executing (id + start
    /// instant), set by `run_wizard_step` just before the step runs
    /// and cleared when it returns. A caller that finds the wizard
    /// lock held reads this to report *what* is running and for how
    /// long instead of a bare "busy" — and the rejection is written
    /// to the wizard log, so the state is diagnosable afterwards.
    /// std Mutex: only synchronous set/clear/read.
    pub(crate) step_running: std::sync::Mutex<Option<(String, std::time::Instant)>>,
}

impl WizardState {
    /// The wizard for `mode`, built on first use.
    pub(crate) async fn get(&self, mode: RunMode) -> &Mutex<Wizard> {
        let cell = match mode {
            RunMode::Install => &self.install,
            RunMode::Audit => &self.audit,
        };
        cell.get_or_init(|| async {
            Mutex::new(
                Wizard::new(default_steps(), mode)
                    .expect("default_steps() has unique step ids by construction"),
            )
        })
        .await
    }
}
