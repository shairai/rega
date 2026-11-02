; Windows installer. Build with:  makensis /DVERSION=0.1.0 scripts/installer.nsi
; Expects ${EXE} (default target\release\snapquill.exe) and dist\snapquill.ico to exist.
Unicode true
!include "MUI2.nsh"

!ifndef EXE
  !define EXE "..\target\release\snapquill.exe"
!endif
!ifndef VERSION
  !define VERSION "0.0.0"
!endif

Name "Snapquill"
OutFile "..\dist\Snapquill-${VERSION}-windows-x64-setup.exe"
InstallDir "$LOCALAPPDATA\Programs\Snapquill"
RequestExecutionLevel user
SetCompressor /SOLID lzma

!define MUI_ICON "..\dist\snapquill.ico"
!define MUI_UNICON "..\dist\snapquill.ico"
!define MUI_FINISHPAGE_RUN "$INSTDIR\Snapquill.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Launch Snapquill"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Section "Install"
  SetOutPath "$INSTDIR"
  File /oname=Snapquill.exe "${EXE}"
  File "..\LICENSE"
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  CreateShortcut "$SMPROGRAMS\Snapquill.lnk" "$INSTDIR\Snapquill.exe"

  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Snapquill" "DisplayName" "Snapquill"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Snapquill" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Snapquill" "DisplayIcon" "$INSTDIR\Snapquill.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Snapquill" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Snapquill" "NoModify" 1
SectionEnd

Section "Uninstall"
  ; Also remove the launch-at-login entry the app registers.
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Snapquill"
  Delete "$INSTDIR\Snapquill.exe"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\Snapquill.lnk"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Snapquill"
SectionEnd
