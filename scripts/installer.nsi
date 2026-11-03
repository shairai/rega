; Windows installer. Build with:  makensis /DVERSION=0.1.0 scripts/installer.nsi
; Expects ${EXE} (default target\release\rega.exe), dist\rega.ico and the
; dist\installer-*.bmp artwork (rega --write-branding dist) to exist.
Unicode true
!include "MUI2.nsh"

!ifndef EXE
  !define EXE "..\target\release\rega.exe"
!endif
!ifndef VERSION
  !define VERSION "0.0.0"
!endif

Name "Rega"
OutFile "..\dist\Rega-${VERSION}-windows-x64-setup.exe"
InstallDir "$LOCALAPPDATA\Programs\Rega"
RequestExecutionLevel user
SetCompressor /SOLID lzma

!define MUI_ICON "..\dist\rega.ico"
!define MUI_UNICON "..\dist\rega.ico"
!define MUI_WELCOMEFINISHPAGE_BITMAP "..\dist\installer-welcome.bmp"
!define MUI_UNWELCOMEFINISHPAGE_BITMAP "..\dist\installer-welcome.bmp"
!define MUI_HEADERIMAGE
!define MUI_HEADERIMAGE_RIGHT
!define MUI_HEADERIMAGE_BITMAP "..\dist\installer-header.bmp"
!define MUI_HEADERIMAGE_UNBITMAP "..\dist\installer-header.bmp"
!define MUI_WELCOMEPAGE_TITLE "Welcome to Rega"
!define MUI_WELCOMEPAGE_TEXT "Capture anything on your screen, mark it up, and share it — in seconds.$\r$\n$\r$\nClick Next to continue."
!define MUI_FINISHPAGE_RUN "$INSTDIR\Rega.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Launch Rega"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Section "Install"
  SetOutPath "$INSTDIR"
  File /oname=Rega.exe "${EXE}"
  File "..\LICENSE"
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  CreateShortcut "$SMPROGRAMS\Rega.lnk" "$INSTDIR\Rega.exe"

  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Rega" "DisplayName" "Rega"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Rega" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Rega" "DisplayIcon" "$INSTDIR\Rega.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Rega" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Rega" "NoModify" 1
SectionEnd

Section "Uninstall"
  ; Also remove the launch-at-login entry the app registers.
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Rega"
  Delete "$INSTDIR\Rega.exe"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\Rega.lnk"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Rega"
SectionEnd
