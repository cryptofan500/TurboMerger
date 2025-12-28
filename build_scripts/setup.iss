; TurboMerger v4.0 Inno Setup Script
; Requires Inno Setup 6.6.1 or later

#define MyAppName "TurboMerger"
#define MyAppVersion "4.0.0"
#define MyAppPublisher "Pawel"
#define MyAppURL "https://github.com/cryptofan500/turbomerger"
#define MyAppExeName "turbomerger.exe"

[Setup]
; Unique ID for this application
AppId={{8F2E4A7B-3C1D-4E5F-9A6B-7C8D9E0F1A2B}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
OutputBaseFilename=TurboMerger-{#MyAppVersion}-Setup
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.18362
ChangesAssociations=yes
DisableProgramGroupPage=yes
LicenseFile=..\LICENSE
OutputDir=..\dist

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Files]
; Main executable - assumes onefile build
Source: "..\dist\turbomerger.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
; Desktop shortcut - CAN BE PINNED TO TASKBAR
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\Uninstall {#MyAppName}"; Filename: "{uninstallexe}"

[Tasks]
Name: "desktopicon"; Description: "Create desktop shortcut (can pin to taskbar)"; Flags: checkedonce
Name: "contextmenu"; Description: "Add to right-click context menu"; Flags: checkedonce

[Registry]
; Context menu for FOLDERS
Root: HKCR; Subkey: "Directory\shell\TurboMerger"; ValueType: string; ValueData: "Merge with TurboMerger"; Flags: uninsdeletekey; Tasks: contextmenu
Root: HKCR; Subkey: "Directory\shell\TurboMerger"; ValueName: "Icon"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"""; Tasks: contextmenu
Root: HKCR; Subkey: "Directory\shell\TurboMerger\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""; Tasks: contextmenu

; Context menu for FOLDER BACKGROUND (right-click in folder)
Root: HKCR; Subkey: "Directory\Background\shell\TurboMerger"; ValueType: string; ValueData: "Merge with TurboMerger"; Flags: uninsdeletekey; Tasks: contextmenu
Root: HKCR; Subkey: "Directory\Background\shell\TurboMerger"; ValueName: "Icon"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"""; Tasks: contextmenu
Root: HKCR; Subkey: "Directory\Background\shell\TurboMerger\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%V"""; Tasks: contextmenu

[Code]
const
  SHCNE_ASSOCCHANGED = $08000000;
  SHCNF_IDLIST = $0000;

procedure SHChangeNotify(wEventId: Integer; uFlags: Cardinal; dwItem1, dwItem2: Integer);
  external 'SHChangeNotify@shell32.dll stdcall';

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, 0, 0);
end;

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "Launch TurboMerger"; Flags: nowait postinstall skipifsilent
