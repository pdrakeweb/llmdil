# Test plan: Windows services

Status: **not run yet**. The first attempt stopped at the Windows
administrator prompt: nobody was there to answer it, and Windows closes
it after about two minutes. This plan is for a session where the owner
sits at the machine.

Design: [windows-services-design.md](../windows-services-design.md).
General test rules: [integration-test-ui.md](integration-test-ui.md).

## 1. Who does what

| Prompt | Who | How often |
|---|---|---|
| Administrator approval (UAC) | Owner, at the machine, within two minutes | Once per register and once per unregister |
| Account password dialog ("LLMdil service registration") | Owner types it; the tester never does | Once per register |

The password is the **Windows account password**. A PIN or fingerprint
doesn't work; with a Microsoft account it is that account's password. An
account without a password can't run services.

The tester tells the owner before each prompt: which button was pressed,
which service it is for, and how many prompts follow. The tester waits up
to 100 seconds, then stops and asks, and never presses Register twice
while a prompt may be open.

**Owner time: one block of about 20 minutes** (6 registers, 6 unregisters
at most), in the order below. The rest runs without the owner.

## 2. Before the owner arrives

| Id | Step | Expected |
|---|---|---|
| V-1 | Run the smoke cases of the UI test | Pass |
| V-2 | Back up the data directory; note `services\*.json` | Backup verified |
| V-3 | `Get-Service llm-*` | None, or a list to leave alone. Service mode is global: an existing registration changes behaviour as soon as the mode switches |
| V-4 | Create `qa-svc-tcc` (small model, TCC GPU), `qa-svc-wddm` (small model, display GPU), `qa-svc-split` (both), all **enabled and stopped**; put them in group `qa-svc`; apply the plan | Each starts, answers and stops in Process mode (the baseline: pid, VRAM per GPU, tok/s) |
| V-5 | Choose ports no other program uses | Free |
| V-6 | Note "Log on as a service" holders: `secedit /export /areas USER_RIGHTS /cfg <file>` (administrator; optional) | Saved for the comparison in 7 |

## 3. Cases that need no approval

| Id | Step | Expected |
|---|---|---|
| V-10 | Settings → Supervision with nothing registered | Every row "not registered"; helper path shown; mode Process |
| V-11 | Register while the backend **runs** | Refused, "stop backend … first"; no Windows prompt |
| V-12 | Switch to Service mode, start an **unregistered** backend | Still starts as a normal process; the card says how it is supervised |
| V-13 | Helper file missing (test copy of the app, not the installed one) | Register disabled, with the reason |

## 4. Register (owner present)

Switch the mode to Service first; registration has no effect on start and
stop before that.

| Id | Step | Owner | Expected |
|---|---|---|---|
| V-20 | Register `qa-svc-tcc` | **Declines** UAC | Clear message; row unchanged; no service; no pending state |
| V-21 | Register again | Lets UAC time out | Same; the button works again |
| V-22 | Register again | Approves UAC, **cancels** the password dialog | Says cancelled; no service left |
| V-23 | Register again | Approves; types a **wrong** password | Refused at registration or at first start with "logon failure" wording; no half-registered service that the app treats as fine |
| V-24 | Register again | Approves; right password | `Get-Service llm-backend-qa-svc-tcc`: Stopped, Manual start; account is the owner's; row "registered" |
| V-25 | Register `qa-svc-wddm` and `qa-svc-split` | Approves twice | Both registered |

For each: `logs\service-manage.log` has one line per action and **no
password**; `services\<name>.json` holds no secret.

## 5. Run as services (owner not needed)

| Id | Step | Expected |
|---|---|---|
| V-30 | Start `qa-svc-tcc` from its card, the Dashboard, and `Start-Service` | Running each time; card shows service supervision and the child pid; **no** administrator prompt |
| V-31 | Prompt through the gateway and through Open WebUI | Answers |
| V-32 | `nvidia-smi`, process list | llama-server runs in session 0, weights on the TCC GPU |
| V-33 | **Display GPU:** start `qa-svc-wddm` | Either it works with weights on that GPU, or it fails with a reason. Record driver version and the log lines that mention CUDA devices. This decides whether services are usable for that GPU |
| V-34 | **Split:** start `qa-svc-split` | Weights on both GPUs, or failed with a reason; **not** a silent CPU run |
| V-35 | Logs page for the backend | Shows the service's output; previous run marked |
| V-36 | Edit the context, apply the plan | Definition file rewritten without a prompt; service restarts with the new value (`/props`) |
| V-37 | Stop from the card; `Stop-Service` | Stopped, code 0, shown as stopped; the app notices the outside stop within seconds |
| V-38 | Kill the llama-server child | Service stops with the child's exit code; card: failed with the reason. The restart policy for processes must **not** also start it (record what happens) |
| V-39 | Taken port; missing model file; not enough VRAM; invalid flag | Refused before the service starts, or failed with the exit-code reason; never "Running" without an answering server |
| V-40 | Stop right after Start | Ends stopped; no orphan llama-server |
| V-41 | Quit LLMdil while a service runs; reopen it | Service keeps answering; the app picks it up; exactly one process serves the port |
| V-42 | Switch the mode to Process while a service runs | Clear rule: refused, or the service stays in charge until stopped; never two servers on one port |
| V-43 | Delete a registered `qa-svc-*` backend from the app | Refused or explained; no orphan service |
| V-44 | Sign out and in again (owner's choice) | Service keeps running |
| V-45 | Over the Admin API: start, stop, restart of a registered backend | Same results. Register and unregister have no API route (reported as a gap) |

## 6. Gateway as a service (optional, separate agreement)

Clients lose the gateway for about a minute per restart. Stop the gateway
first; PostgreSQL stays a normal process.

| Id | Step | Expected |
|---|---|---|
| V-50 | Register "Gateway" (owner approves, types the password) | Registered, Manual |
| V-51 | Start through the app | Healthy; the master key comes from the owner's Credential Manager; no key in the definition file |
| V-52 | Apply a plan with a gateway change; restart | Works; config live |
| V-53 | Stop, unregister | Back to process mode, healthy |

## 7. Unregister and compare (owner present)

| Id | Step | Expected |
|---|---|---|
| V-60 | Unregister while the service runs | Refused, "stop … first"; no prompt |
| V-61 | Stop all; Unregister; **decline** UAC once | Still registered, clear message |
| V-62 | Unregister each (one approval per service) | `Get-Service llm-*` as in V-3 |
| V-63 | Mode back to Process; start each `qa-svc-*` | Baseline results of V-4 |
| V-64 | Compare config with the backup; `services\*.json` of the test backends remain by design: list them; "Log on as a service" right remains: list it | Nothing else changed |
| V-65 | Remove or keep `qa-svc-*` as agreed | Done |

If anything goes wrong and the app can't unregister, the owner runs this
in an administrator terminal, once per service:

```bash
sc.exe delete llm-backend-qa-svc-tcc
```

## 8. Stop conditions

Stop and tell the owner when: a prompt appears that the tester didn't
announce; a service that the test didn't create changes state; the
gateway stays down longer than three minutes; a password or key shows up
in any file or log.
