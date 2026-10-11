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
; The POSTINSTALL hook below adds a "Launch now?" prompt at the end of the
; install-files step, implementing the finish-page "Launch Local LLM Service
; Manager" offer.
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
!macro NSIS_HOOK_PREUNINSTALL
  IfSilent +5
  IfFileExists "$APPDATA\local-llm-service-manager\*.*" 0 +4
  MessageBox MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON2 "Remove user data (configuration, logs, downloaded models) in %APPDATA%\local-llm-service-manager?$\n$\nSelect No (recommended) to keep your data for a future reinstall." IDYES +2
  Goto +2
  RMDir /r "$APPDATA\local-llm-service-manager"
!macroend

; --- Install finish: offer to launch ------------------------------------------
; Skipped entirely in silent installs (/S): there is no user to answer the
; prompt, and a headless install must never block on a MessageBox.
!macro NSIS_HOOK_POSTINSTALL
  IfSilent +3
  MessageBox MB_YESNO|MB_ICONQUESTION "Launch Local LLM Service Manager now?" IDNO +2
  Exec '"$INSTDIR\local-llm-service-manager.exe"'
!macroend
