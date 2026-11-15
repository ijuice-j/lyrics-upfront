; Inno Setup script for Lyrics UpFront.
;
; Produces a single Setup .exe: the user downloads one file, double-clicks it,
; and the app is installed with Desktop and Start Menu shortcuts.
;
; The app is one self-contained exe of about 1 MB,
; so the installer is tiny too.
;
; Lyrics UpFront replaces its earlier Python version. It keeps that
; version's AppId, so running this setup on a machine that has it upgrades
; it in place: the old exe, runtime folder, shortcuts and start-with-Windows
; entry are replaced by the new ones, and Settings > Apps lists one app.
;
; Build (from the repository root):
;   cargo build --release
;   target\release\lyrics-upfront.exe --write-icon app.ico
;   "%LOCALAPPDATA%\Programs\Inno Setup 6\ISCC.exe" installer.iss
; Output: installer_output\LyricsUpFront-Setup.exe

#define AppName        "Lyrics UpFront"
#define AppShortName   "LyricsUpFront"
#define AppVersion     "2.0.0"
#define AppPublisher   "viraj-rgb"
#define AppURL         "https://github.com/ijuice-j/lyrics-upfront"
#define AppExeName     "LyricsUpFront.exe"
; Registry value shared with the app's own "Start automatically" setting.
#define RunValueName   "LyricsUpFront"

; Names used by the earlier version, cleaned up on upgrade.
#define OldAppName      "Spotify Lyrics Overlay"
#define OldExeName      "SpotifyLyricsOverlay.exe"
#define OldRunValueName "LyricOverlay"

[Setup]
; Stable GUID, inherited from the earlier version. Never change it -- it is
; how Windows recognises an existing install and upgrades it in place rather
; than stacking a second copy.
AppId={{8F3C2A41-6D9E-4B27-9C55-1E7A0D4B8F62}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}/issues
AppUpdatesURL={#AppURL}/releases
VersionInfoVersion={#AppVersion}
VersionInfoDescription={#AppName} installer

; Install per-user under LocalAppData so no administrator prompt appears.
; An upgrade stays in the folder the old app was installed to.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
DefaultDirName={localappdata}\Programs\{#AppShortName}
DefaultGroupName={#AppName}
; Use the new Start Menu folder name even when upgrading (the old folder is
; removed below).
UsePreviousGroup=no
DisableProgramGroupPage=yes
; The app has no options to choose at install time, so the directory page is
; just friction. Advanced users can still pass /DIR= on the command line.
DisableDirPage=yes
DisableReadyPage=no

OutputDir=installer_output
OutputBaseFilename={#AppShortName}-Setup
SetupIconFile=app.ico
UninstallDisplayIcon={app}\{#AppExeName}
UninstallDisplayName={#AppName}

Compression=lzma2/max
SolidCompression=yes

WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a &Desktop shortcut"; GroupDescription: "Shortcuts:"
Name: "startupicon"; Description: "Start {#AppName} when I sign in to Windows"; GroupDescription: "Startup:"; Flags: unchecked

[InstallDelete]
; Upgrading from the earlier version: remove its ~200 MB runtime folder
; and its exe, which the new exe replaces.
Type: filesandordirs; Name: "{app}\_internal"
Type: files; Name: "{app}\{#OldExeName}"
; Its shortcuts, which pointed at the old exe.
Type: filesandordirs; Name: "{autoprograms}\{#OldAppName}"
Type: files; Name: "{autodesktop}\{#OldAppName}.lnk"
; Python releases used a Startup-folder shortcut for start-with-Windows. The
; Run value below replaces it; if that option was chosen before, Inno
; re-selects the task, so the choice carries over.
Type: files; Name: "{userstartup}\{#OldAppName}.lnk"

[Files]
Source: "target\release\lyrics-upfront.exe"; DestDir: "{app}"; DestName: "{#AppExeName}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Registry]
; The old app's start-with-Windows value would launch an exe that no longer
; exists; the task below writes the new one instead.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "{#OldRunValueName}"; Flags: deletevalue
; Start-with-Windows uses the per-user Run key -- the same value the app's
; Settings checkbox reads and writes, so the two always agree.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#RunValueName}"; ValueData: """{app}\{#AppExeName}"""; Flags: uninsdeletevalue; Tasks: startupicon

[Run]
Filename: "{app}\{#AppExeName}"; Description: "Launch {#AppName}"; Flags: nowait postinstall skipifsilent

; The user's own settings live in %USERPROFILE%\.lyrics-upfront and are
; deliberately left alone on uninstall, so reinstalling keeps their position
; and preferences. On its first run the app copies the earlier version's
; settings and cached lyrics from %USERPROFILE%\.lyric-overlay.

[Code]
{ Whether a process with this image name is running. }
function IsRunning(ExeName: String): Boolean;
var
  ResultCode: Integer;
begin
  Result := Exec('cmd.exe', '/C tasklist /FI "IMAGENAME eq ' + ExeName + '" | find /I "' + ExeName + '"',
                 '', SW_HIDE, ewWaitUntilTerminated, ResultCode) and (ResultCode = 0);
end;

{ Refuse to install over a copy that is currently running -- the exe would be
  locked and the upgrade would half-apply. The old app counts too, since its
  files are removed. }
function InitializeSetup(): Boolean;
var
  ResultCode: Integer;
begin
  Result := True;
  if IsRunning('{#AppExeName}') or IsRunning('{#OldExeName}') then
  begin
    if MsgBox('{#AppName} (or its earlier version) is currently running.' + #13#10#13#10 +
              'Quit it from the tray icon, then click OK to continue.',
              mbConfirmation, MB_OKCANCEL) = IDOK then
    begin
      Exec('taskkill.exe', '/F /IM "{#AppExeName}"', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
      Exec('taskkill.exe', '/F /IM "{#OldExeName}"', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    end
    else
      Result := False;
  end;
end;

{ The app's Settings checkbox can create the Run value even when the installer
  task was not chosen, so remove it on uninstall regardless; otherwise Windows
  would try to start a program that no longer exists at every sign-in. }
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
  begin
    RegDeleteValue(HKEY_CURRENT_USER, 'Software\Microsoft\Windows\CurrentVersion\Run', '{#RunValueName}');
    RegDeleteValue(HKEY_CURRENT_USER, 'Software\Microsoft\Windows\CurrentVersion\Run', '{#OldRunValueName}');
  end;
end;
