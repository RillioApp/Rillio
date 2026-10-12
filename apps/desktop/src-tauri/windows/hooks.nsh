; Rillio NSIS installer hooks (wired through bundle.windows.nsis.installerHooks
; in tauri.conf.json; the bundler includes this file near the top of its
; installer.nsi, after utils.nsh and Win\RestartManager.nsh).
;
; NSIS_HOOK_PREINSTALL: on an update, wait for the old app to finish exiting
; before the template checks whether it is running.
;
; Why. Since tauri CLI 2.12.0 the template's CheckIfAppIsRunning (utils.nsh)
; asks Windows Restart Manager which processes have $INSTDIR\<main binary> in
; use and, in silent mode, calls RmShutdown(force) on them. Any nonzero result
; makes the silent installer Abort with no message. On an update the app starts
; the installer and exits at once (tauri-plugin-updater: ShellExecute, then
; std::process::exit(0)), so the installer can reach that check while the old
; process is still being torn down by the kernel. It is still listed, and
; RmShutdown gives a process that is already exiting only about half a second
; to disappear (measured: a teardown that outlasts that returns
; ERROR_FAIL_SHUTDOWN 351 with Application event 10006, status 17), so nothing
; gets installed. How long a teardown takes depends on how much memory the app
; had committed and on what the machine is doing, which is why it is
; intermittent. The pre-2.12 macro killed by process name and slept 500 ms,
; which hid the race. Seen for real on the 0.1.46 -> 0.1.47 update.
;
; What. Only when /UPDATE was passed ($UpdateMode = 1, set by the template's
; .onInit) and the main binary already exists: poll until Restart Manager
; lists nobody using it, bounded by RILLIO_EXIT_WAIT_BOUND_MS. Then fall
; through to the template's own check in every case. If the bound runs out
; (a second instance that really is running, not exiting) the template still
; performs its normal shutdown, exactly as it would without this hook.
;
; Why Restart Manager for the "in use" test, and not opening the file for
; write: the thing that must become false is precisely the condition the
; template tests next, so the hook asks the same question with the same calls
; (start session, register the file, RmGetList with a null buffer) and simply
; never calls RmShutdown. A write-open probe tests a neighbouring property
; (share modes) that can disagree with it in both directions, for example an
; antivirus scan holding the file without being a process RM would list.
; Each poll needs a FRESH session: inside one session RmGetList keeps
; answering ERROR_MORE_DATA after the process is gone (measured 2026-10-11 on
; Windows 11 26200), and a fresh session is also what the template's check
; uses. A poll costs about 60 ms and writes two informational Restart Manager
; events (10000, 10001) to the Application log, hence the unhurried period.
;
; Fresh installs and manual (non-update) installs never enter the loop.
;
; When bumping the tauri CLI (pinned in .github/workflows): re-read the
; template's CheckIfAppIsRunning and keep the three calls below in step.

!define RILLIO_EXIT_WAIT_BOUND_MS 15000
!define RILLIO_EXIT_WAIT_POLL_MS 100
; RmGetList's answer when at least one process uses a registered resource
; (utils.nsh defines the same value the same way).
!define /ifndef ERROR_MORE_DATA 234

!macro NSIS_HOOK_PREINSTALL
  ${If} $UpdateMode = 1
  ${AndIf} ${FileExists} "$INSTDIR\${MAINBINARYNAME}.exe"
    ; The template's sections use these registers freely; leave them as found.
    Push $0
    Push $1
    Push $2
    Push $3
    Push $R0
    Push $R4
    Push $R5

    ; $R4: tick count at the start of the wait, $R5: milliseconds waited.
    System::Call 'kernel32::GetTickCount() i .R4'
    ${Do}
      ; $0 ends each poll as the RmGetList result, or 0 ("stop waiting") when
      ; Restart Manager cannot answer: the template then decides on its own.
      StrCpy $0 0
      !insertmacro RestartManager_StartSession $R0
      ${If} $R0 != ""
        !insertmacro RestartManager_RegisterFile $R0 "$INSTDIR\${MAINBINARYNAME}.exe"
        ${If} $0 = 0
          ; Same call as the template: null buffer, so ERROR_SUCCESS means
          ; nobody uses the file and ERROR_MORE_DATA means somebody does.
          System::Call 'RSTRTMGR::RmGetList(p R0, *i .r1, *i .r2, p 0, *i .r3) i .r0'
        ${Else}
          StrCpy $0 0
        ${EndIf}
        !insertmacro RestartManager_EndSession $R0
      ${EndIf}
      ${If} $0 <> ${ERROR_MORE_DATA}
        ${ExitDo}
      ${EndIf}

      System::Call 'kernel32::GetTickCount() i .R5'
      IntOp $R5 $R5 - $R4
      ${If} $R5 >= ${RILLIO_EXIT_WAIT_BOUND_MS}
        DetailPrint "${PRODUCTNAME} was still running after ${RILLIO_EXIT_WAIT_BOUND_MS} ms, asking it to close"
        ${ExitDo}
      ${EndIf}
      Sleep ${RILLIO_EXIT_WAIT_POLL_MS}
    ${Loop}

    Pop $R5
    Pop $R4
    Pop $R0
    Pop $3
    Pop $2
    Pop $1
    Pop $0
  ${EndIf}
!macroend
