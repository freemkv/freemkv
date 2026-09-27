; Per-user installer for the Windows app. Built by release.yml:
;   iscc /DAppVersion=X.Y.Z /DBinDir=<dir with freemkv.exe + freemkv.com> /O<out> freemkv.iss
; freemkv.exe is the windowed image, freemkv.com the console one; `freemkv` on
; PATH resolves to the .com first (PATHEXT).

#ifndef AppVersion
  #error AppVersion must be defined (/DAppVersion=X.Y.Z)
#endif
#ifndef BinDir
  #error BinDir must be defined (/DBinDir=...)
#endif

[Setup]
; Never change AppId: it is how upgrades and the uninstaller find this install.
AppId={{1CF5699A-98F1-450E-9DD3-85FE5DF79A3B}
AppName=freemkv
AppVersion={#AppVersion}
AppVerName=freemkv {#AppVersion}
AppPublisher=freemkv
AppPublisherURL=https://github.com/freemkv/freemkv
VersionInfoVersion={#AppVersion}
PrivilegesRequired=lowest
DefaultDirName={localappdata}\Programs\freemkv
DisableDirPage=yes
DisableProgramGroupPage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
ChangesEnvironment=yes
SetupIconFile=..\..\res\freemkv.ico
UninstallDisplayIcon={app}\freemkv.exe
UninstallDisplayName=freemkv
OutputBaseFilename=freemkv-x86_64-windows-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#BinDir}\freemkv.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\freemkv.com"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
; Same AppUserModelID the process sets (windows.rs APP_ID), so a pinned
; shortcut and the running window group as one app.
Name: "{autoprograms}\freemkv"; Filename: "{app}\freemkv.exe"; AppUserModelID: "org.freemkv.FreeMKV"
Name: "{autodesktop}\freemkv"; Filename: "{app}\freemkv.exe"; AppUserModelID: "org.freemkv.FreeMKV"; Tasks: desktopicon

[Registry]
; The toast identity the app registers on its first rip-finished toast;
; declared here so the uninstaller removes it.
Root: HKCU; Subkey: "Software\Classes\AppUserModelId\org.freemkv.FreeMKV"; ValueType: string; ValueName: "DisplayName"; ValueData: "freemkv"; Flags: uninsdeletekey

[Run]
Filename: "{app}\freemkv.exe"; Description: "{cm:LaunchProgram,freemkv}"; Flags: nowait postinstall skipifsilent

[Code]
const
  EnvKey = 'Environment';

function PathIndex(Paths, Dir: string): Integer;
begin
  Result := Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Paths) + ';');
end;

procedure AddToPath(Dir: string);
var
  Paths: string;
begin
  if not RegQueryStringValue(HKCU, EnvKey, 'Path', Paths) then
    Paths := '';
  if PathIndex(Paths, Dir) > 0 then
    exit;
  if (Paths <> '') and (Copy(Paths, Length(Paths), 1) <> ';') then
    Paths := Paths + ';';
  if not RegWriteExpandStringValue(HKCU, EnvKey, 'Path', Paths + Dir) then
    SuppressibleMsgBox('Could not add ' + Dir + ' to your PATH. Add it by hand to run freemkv from a console.', mbError, MB_OK, IDOK);
end;

procedure RemoveFromPath(Dir: string);
var
  Paths: string;
  P: Integer;
begin
  if not RegQueryStringValue(HKCU, EnvKey, 'Path', Paths) then
    exit;
  P := PathIndex(Paths, Dir);
  if P = 0 then
    exit;
  Paths := ';' + Paths + ';';
  Delete(Paths, P, Length(Dir) + 1);
  Paths := Copy(Paths, 2, Length(Paths) - 2);
  if not RegWriteExpandStringValue(HKCU, EnvKey, 'Path', Paths) then
    SuppressibleMsgBox('Could not remove ' + Dir + ' from your PATH. Remove it by hand.', mbError, MB_OK, IDOK);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    AddToPath(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    RemoveFromPath(ExpandConstant('{app}'));
end;
