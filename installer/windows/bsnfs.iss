; Inno Setup script for the bsnfs Windows installer. Built by
; scripts/dist/package-windows.sh, which passes these defines:
;   MyAppVersion  version string, e.g. 1.2.3 or nightly.2026.10.26
;   FileVersion   the same as four numbers, each at most 65535
;   TargetArch    x86, x64 or arm64
;   SourceDir     folder holding bsnfs.exe
;   DocDir        folder holding README.md, GOAL.md and the licenses
;
; Installs bsnfs.exe to Program Files and, by default, registers it as a
; Windows service (`bsnfs service install`), which also opens it in the
; Windows Firewall. Its config lives in %ProgramData%\bsnfs\bsnfs.conf.

#ifndef MyAppVersion
  #define MyAppVersion "0.0.0"
#endif
#ifndef FileVersion
  #define FileVersion "0.0.0.0"
#endif
#ifndef TargetArch
  #define TargetArch "x64"
#endif

[Setup]
AppId={{6F4D2B1E-8C3A-4E5F-9B7D-2A1C0E9F8B64}
AppName=bsnfs
AppVersion={#MyAppVersion}
AppVerName=bsnfs {#MyAppVersion}
VersionInfoVersion={#FileVersion}
AppPublisher=bsnfs
DefaultDirName={autopf}\bsnfs
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ChangesEnvironment=yes
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\bsnfs.exe
LicenseFile={#DocDir}\LICENSE
; Windows 10 and later; the ARM64 build is for Windows 11 on ARM.
#if TargetArch == "arm64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
MinVersion=10.0.22000
#elif TargetArch == "x64"
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
#else
ArchitecturesAllowed=x86compatible
MinVersion=10.0
#endif

[Tasks]
Name: "service"; Description: "Run bsnfs as a service that starts with Windows"
Name: "path"; Description: "Add bsnfs to the PATH (for use from a terminal)"

[Files]
Source: "{#SourceDir}\bsnfs.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#DocDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#DocDir}\GOAL.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#DocDir}\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "{#DocDir}\LICENSE-iris.txt"; DestDir: "{app}"; Flags: ignoreversion

[Run]
Filename: "{app}\bsnfs.exe"; Parameters: "service install"; Tasks: service; \
  Flags: runhidden waituntilterminated; StatusMsg: "Starting the bsnfs service..."
Filename: "http://127.0.0.1:8080/"; Description: "Open the bsnfs admin page to add folders"; \
  Tasks: service; Flags: shellexec postinstall skipifsilent nowait

[UninstallRun]
Filename: "{app}\bsnfs.exe"; Parameters: "service uninstall"; Flags: runhidden waituntilterminated; \
  RunOnceId: "RemoveService"

[Code]
const
  EnvKey = 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment';

{ Stop a running service before files are replaced on upgrade. }
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
begin
  Exec(ExpandConstant('{sys}\sc.exe'), 'stop bsnfs', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Sleep(1500);
  Result := '';
end;

function PathHas(Dir: String): Boolean;
var
  Path: String;
begin
  Result := RegQueryStringValue(HKLM, EnvKey, 'Path', Path)
    and (Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Path) + ';') > 0);
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Path, Dir: String;
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('path') then begin
    Dir := ExpandConstant('{app}');
    if not PathHas(Dir) and RegQueryStringValue(HKLM, EnvKey, 'Path', Path) then
      RegWriteExpandStringValue(HKLM, EnvKey, 'Path', Path + ';' + Dir);
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Path, Dir: String;
  P: Integer;
begin
  if CurUninstallStep <> usPostUninstall then exit;
  Dir := ExpandConstant('{app}');
  if not RegQueryStringValue(HKLM, EnvKey, 'Path', Path) then exit;
  P := Pos(';' + Uppercase(Dir), Uppercase(Path));
  if P > 0 then begin
    Delete(Path, P, Length(Dir) + 1);
    RegWriteExpandStringValue(HKLM, EnvKey, 'Path', Path);
  end;
end;
