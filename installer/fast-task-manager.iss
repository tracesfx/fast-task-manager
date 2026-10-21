; Inno Setup script. Built by .github/workflows/release.yml:
;   iscc /DAppVersion=0.1.0 /DExePath=..\target\release\fast-task-manager.exe installer\fast-task-manager.iss
#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef ExePath
  #define ExePath "..\target\release\fast-task-manager.exe"
#endif

[Setup]
AppId={{DA1C7D89-FAE0-4770-BB9B-8B3B0C220B36}
AppName=Fast Task Manager
AppVersion={#AppVersion}
DefaultDirName={autopf}\Fast Task Manager
DefaultGroupName=Fast Task Manager
UninstallDisplayIcon={app}\fast-task-manager.exe
OutputDir=..\dist
OutputBaseFilename=fast-task-manager-{#AppVersion}-setup
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Per-user by default (no admin prompt); the dialog offers an all-users install.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
WizardStyle=modern
; Lets the installer replace the exe while the app is running.
CloseApplications=yes

[Files]
Source: "{#ExePath}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Fast Task Manager"; Filename: "{app}\fast-task-manager.exe"
Name: "{autodesktop}\Fast Task Manager"; Filename: "{app}\fast-task-manager.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; Flags: unchecked

[Run]
Filename: "{app}\fast-task-manager.exe"; Description: "Launch Fast Task Manager"; Flags: nowait postinstall skipifsilent
