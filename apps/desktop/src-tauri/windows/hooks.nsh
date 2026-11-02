; Stop the background reader before files are replaced or removed, so an
; update or uninstall never fails on a locked sleuth-worker.exe.
!macro NSIS_HOOK_PREINSTALL
  nsExec::Exec 'taskkill /F /IM sleuth-worker.exe'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::Exec 'taskkill /F /IM sleuth-worker.exe'
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Sleuth reader"
!macroend
