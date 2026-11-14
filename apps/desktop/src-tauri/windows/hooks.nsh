; Stop the background reader before files are replaced or removed, so an
; update or uninstall never fails on a locked sleuth-worker.exe.
!macro NSIS_HOOK_PREINSTALL
  nsExec::Exec 'taskkill /F /IM sleuth-worker.exe'
!macroend

; sleuth://search?q=<tag> opens Sleuth on that search (Dexterity Connect turns line and
; drawing numbers in chat into these links).
!macro NSIS_HOOK_POSTINSTALL
  WriteRegStr HKCU "Software\Classes\sleuth" "" "URL:Sleuth"
  WriteRegStr HKCU "Software\Classes\sleuth" "URL Protocol" ""
  WriteRegStr HKCU "Software\Classes\sleuth\DefaultIcon" "" "$INSTDIR\${MAINBINARYNAME}.exe,0"
  WriteRegStr HKCU "Software\Classes\sleuth\shell\open\command" "" '"$INSTDIR\${MAINBINARYNAME}.exe" "%1"'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::Exec 'taskkill /F /IM sleuth-worker.exe'
  DeleteRegKey HKCU "Software\Classes\sleuth"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Sleuth reader"
!macroend
