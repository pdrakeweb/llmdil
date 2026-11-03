; installer-hooks.nsi — NSIS hooks for the Local LLM Service Manager installer.
; Referenced from tauri.conf.json > bundle.windows.nsis.installerHooks.
;
; Uses only core NSIS instructions (no LogicLib dependency) via relative jumps.
;
; INSTALLER WIZARD PAGES (stock tauri NSIS template, MUI):
;   1. Welcome page
;   2. License page (LICENSE file, Apache-2.0)
;   3. Directory page (per-user install dir, no UAC elevation)
;   4. Install files page
;   5. Finish page
; There is no post-install launch prompt in this file: the finish page's
; "Run LLMdil" checkbox (the template's MUI_FINISHPAGE_RUN, in
; installer-template.nsi) is the installer's only launch offer.
;
; SILENT INSTALL FLAGS (built into NSIS, work with this installer):
;   installer.exe /S                — silent install (no wizard pages)
;   installer.exe /S /D=C:\My\Dir   — silent install to a custom directory
;                                     (/D= must be the LAST argument, no quotes
;                                      even if the path contains spaces)

; --- Uninstall: ask before removing user data --------------------------------
; App binaries are ALWAYS removed by the uninstaller. User data in
; %APPDATA%\local-llm-service-manager (config.json, logs, downloaded GGUF
; models, managed Python env) is KEPT BY DEFAULT and deleted only when the
; user explicitly answers "Yes". The prompt defaults to "No" (MB_DEFBUTTON2).
; In silent uninstalls (/S) the prompt is skipped and data is kept.
;
; The removal itself is a single `rd /s /q` run through nsExec (hidden,
; no console window) with ONE DetailPrint line. NSIS's own RMDir /r
; deletes and logs every file individually — a managed Python venv is
; ~15,000 files, which took minutes and flooded the uninstall log
; (real hardware, 2026-10-15). The RMDir /r remains only as a fallback
; for whatever rd could not remove (e.g. a file locked by a process
; that is still running).
!macro NSIS_HOOK_PREUNINSTALL
  IfSilent +9
  IfFileExists "$APPDATA\local-llm-service-manager\*.*" 0 +8
  MessageBox MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON2 "Remove user data (configuration, logs, downloaded models) in %APPDATA%\local-llm-service-manager?$\n$\nSelect No (recommended) to keep your data for a future reinstall." IDYES +2
  Goto +6
  DetailPrint "Removing user data in %APPDATA%\local-llm-service-manager ..."
  nsExec::Exec '"$SYSDIR\cmd.exe" /c rd /s /q "$APPDATA\local-llm-service-manager"'
  Pop $0
  IfFileExists "$APPDATA\local-llm-service-manager\*.*" 0 +2
  RMDir /r "$APPDATA\local-llm-service-manager"
  ; --- Uninstall: remove the log receiver service ------------------
  ; The Prometheus-local log receiver (Settings → Supervision → Log
  ; receiver) is an auto-start Windows service registered against
  ; the bundled llmdil-log-receiver.exe and llm-service-host.exe in
  ; $INSTDIR; leaving the registration behind would strand a broken
  ; service pointing at files this uninstall removes. Service
  ; removal needs elevation the per-user uninstaller does not have,
  ; so when the service is present this block runs the receiver's
  ; own `remove` subcommand elevated (one UAC prompt, via
  ; PowerShell's Start-Process -Verb RunAs) and re-checks. A
  ; declined prompt leaves the registration in place with an
  ; explicit warning rather than failing the uninstall. Silent
  ; uninstalls cannot elevate: they skip the attempt and log the
  ; same instruction. The receiver's data (store + tokens in
  ; %APPDATA%\local-llm-service-manager\log-receiver) is user data,
  ; governed by the prompt above like everything else there.
  ; Relative jumps only, per this file's convention. Instruction
  ; map: +14 from the IfFileExists lands past the block; the first
  ; IntCmp (+11) and the second (+4) land at the same place; the
  ; IfSilent (+9) lands on the silent DetailPrint; the Goto (+2)
  ; skips it.
  IfFileExists "$INSTDIR\llmdil-log-receiver.exe" 0 +14
  nsExec::Exec '"$SYSDIR\sc.exe" query "llm-log-receiver"'
  Pop $0
  IntCmp $0 1060 +11
  IfSilent +9
  DetailPrint "Removing the LLMdil Log Receiver service ..."
  nsExec::Exec '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -Command "Start-Process -FilePath $\'$INSTDIR\llmdil-log-receiver.exe$\' -ArgumentList $\'remove$\' -Verb RunAs -Wait"'
  Pop $0
  nsExec::Exec '"$SYSDIR\sc.exe" query "llm-log-receiver"'
  Pop $0
  IntCmp $0 1060 +4
  MessageBox MB_OK|MB_ICONEXCLAMATION "The LLMdil Log Receiver service could not be removed (the elevation prompt may have been declined).$\n$\nIt is registered against files this uninstall is removing. Remove it from an elevated command prompt with:$\n$\nsc.exe delete llm-log-receiver"
  Goto +2
  DetailPrint "Log receiver service is registered: a silent uninstall cannot elevate to remove it, and it points at files being removed. Remove it from an elevated command prompt with: sc.exe delete llm-log-receiver"
!macroend

