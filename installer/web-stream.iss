; The web-stream host installer (Inno Setup 6). One screen of checkboxes,
; then the tray app opens on its first-run setup.
;
; Built by CI (.github/workflows/release.yml and ci.yml):
;   powershell -File installer\fetch-redist.ps1
;   cargo build --release --locked -p ws-host-gui
;   iscc /DAppVersion=0.2.0 installer\web-stream.iss
; Output: target\installer\web-stream-setup-<version>.exe
;
; Installs per machine into Program Files (only administrators can change
; what may later run as SYSTEM). Settings, paired browsers and logs stay in
; each user's %LOCALAPPDATA%\web-stream, as before.
;
; Silent install, as CI tests it:
;   web-stream-setup.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /TASKS="autostart"
; Silent uninstall keeps settings unless /PURGE is given:
;   "%ProgramFiles%\web-stream\unins000.exe" /VERYSILENT /SUPPRESSMSGBOXES /PURGE

#ifndef AppVersion
  #define AppVersion "0.0.0-dev"
#endif
; Windows wants four numbers in the file's version resource.
#ifndef AppNumericVersion
  #define AppNumericVersion "0.0.0"
#endif
#define AppName "web-stream host"
#define AppExe "ws-host-gui.exe"
; The service's copy of the host (crates/host/src/service/scm.rs).
#define ServiceExe "ws-host.exe"
#define ServiceName "web-stream"
#define FirewallRule "web-stream host"
; HKCU Run value the tray app's "Start with Windows" uses (crates/gui/src/win.rs).
#define RunValue "web-stream host"
#define ViGEm "ViGEmBus_1.22.0_x64_x86_arm64.exe"
#define SteamMicInf "Steam\drivers\Windows10\x64\SteamStreamingMicrophone.inf"

[Setup]
AppId={{918435C9-8423-4488-9252-A4724B1CB18E}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=web-stream
AppPublisherURL=https://github.com/Gogan999/web-stream
AppSupportURL=https://github.com/Gogan999/web-stream/blob/main/docs/guide/getting-started.md
VersionInfoVersion={#AppNumericVersion}
DefaultDirName={autopf}\web-stream
; Fixed: the service copy, the firewall rules and the updater expect it here.
DisableDirPage=yes
DisableProgramGroupPage=yes
DisableWelcomePage=yes
DisableReadyPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
WizardStyle=modern
OutputDir=..\target\installer
OutputBaseFilename=web-stream-setup-{#AppVersion}
Compression=lzma2/max
SolidCompression=yes
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
SetupLogging=yes
; The tray app's single-instance mutex: setup and uninstall ask to quit it
; from the tray first, so it can put back a changed resolution and remove
; its router forwards.
AppMutex=Local\web-stream-host-gui
CloseApplications=no
RestartIfNeededByRun=no

[Messages]
SelectTasksLabel2=Choose what to set up, then click Install.
WizardSelectTasks=Set up web-stream host
SetupAppRunningError=web-stream host is running. Quit it from its tray icon (right-click, Quit), then click OK.
UninstallAppRunningError=web-stream host is running. Quit it from its tray icon (right-click, Quit), then click OK.

[Tasks]
Name: "gamepad"; Description: "Gamepad driver (ViGEmBus), so controllers in the browser work in games"
Name: "mic"; Description: "Microphone driver (Steam Streaming Microphone), so the browser's mic reaches Windows"; Check: SteamMicAvailable
Name: "service"; Description: "Allow use at the login screen and with admin programs (installs a Windows service)"; Flags: unchecked
Name: "autostart"; Description: "Start with Windows (in the tray)"

[Files]
Source: "..\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "third-party\*"; DestDir: "{app}\licenses"; Flags: ignoreversion
Source: "redist\{#ViGEm}"; DestDir: "{tmp}"; Flags: deleteafterinstall; Tasks: gamepad; Check: not ViGEmInstalled

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"; Comment: "Stream this PC to a browser"

[Run]
; Inbound rules for the two programs that listen (the tray app, and the
; service's copy), on any port they are set to, on every network profile:
; a friend's home network is often marked Public. Removed on uninstall.
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""{#FirewallRule}"""; Flags: runhidden; StatusMsg: "Adding firewall rules..."
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""{#FirewallRule}"" dir=in action=allow program=""{app}\{#AppExe}"" enable=yes profile=any"; Flags: runhidden
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""{#FirewallRule}"" dir=in action=allow program=""{app}\{#ServiceExe}"" enable=yes profile=any"; Flags: runhidden
Filename: "{tmp}\{#ViGEm}"; Parameters: "/exenoui /qn /norestart"; Tasks: gamepad; Check: not ViGEmInstalled; StatusMsg: "Installing the gamepad driver..."
Filename: "{app}\{#AppExe}"; Parameters: "setup mic-driver"; Tasks: mic; Flags: runhidden; StatusMsg: "Installing the microphone driver..."
; The service: updated in place if it is already installed (keeping its
; settings), installed with the defaults if newly asked for.
Filename: "{app}\{#AppExe}"; Parameters: "service update --quiet"; Check: ServiceInstalled; Flags: runhidden; StatusMsg: "Updating the service..."
Filename: "{app}\{#AppExe}"; Parameters: "service install --quiet"; Tasks: service; Check: not ServiceInstalled; Flags: runhidden; StatusMsg: "Installing the service..."
Filename: "{app}\{#AppExe}"; Description: "Open web-stream host"; Flags: postinstall nowait runasoriginaluser skipifsilent

[UninstallRun]
Filename: "{app}\{#AppExe}"; Parameters: "service uninstall --quiet"; Check: ServiceInstalled; Flags: runhidden; RunOnceId: "service"
Filename: "{app}\{#AppExe}"; Parameters: "setup upnp-remove"; Flags: runhidden; RunOnceId: "upnp"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""{#FirewallRule}"""; Flags: runhidden; RunOnceId: "firewall"

[UninstallDelete]
; The service's copy and its leftovers from an interrupted update.
Type: files; Name: "{app}\{#ServiceExe}"
Type: files; Name: "{app}\{#ServiceExe}.new"
Type: dirifempty; Name: "{app}\licenses"
Type: dirifempty; Name: "{app}"

[Code]
const
  RunKey = 'Software\Microsoft\Windows\CurrentVersion\Run';

function SteamMicAvailable: Boolean;
begin
  Result := FileExists(ExpandConstant('{commoncf32}\{#SteamMicInf}'));
end;

function ViGEmInstalled: Boolean;
begin
  Result := RegKeyExists(HKLM, 'SYSTEM\CurrentControlSet\Services\ViGEmBus');
end;

function ServiceInstalled: Boolean;
begin
  Result := RegKeyExists(HKLM, 'SYSTEM\CurrentControlSet\Services\{#ServiceName}');
end;

function HasParam(const Name: String): Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if CompareText(ParamStr(I), Name) = 0 then
      Result := True;
end;

// "Start with Windows" belongs to the user who ran setup, not to the
// administrator account UAC elevated to, so the tray app sets it as them.
procedure SetAutostart(const On: Boolean);
var
  Arg: String;
  Code: Integer;
begin
  if On then Arg := 'on' else Arg := 'off';
  if not ExecAsOriginalUser(ExpandConstant('{app}\{#AppExe}'), 'setup autostart ' + Arg, '',
      SW_HIDE, ewWaitUntilTerminated, Code) or (Code <> 0) then
    Log(Format('setup autostart %s failed (%d)', [Arg, Code]));
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    SetAutostart(WizardIsTaskSelected('autostart'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Purge: Boolean;
begin
  if CurUninstallStep = usUninstall then
    // The uninstaller runs as the elevated user, who is almost always the
    // one who installed it.
    RegDeleteValue(HKCU, RunKey, '{#RunValue}');
  if CurUninstallStep = usPostUninstall then
  begin
    if UninstallSilent then
      Purge := HasParam('/PURGE')
    else
      Purge := MsgBox('Also delete the settings, paired browsers, certificate and logs?' + #13#10#13#10 +
        'Choose No to keep them for a later install.', mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES;
    if Purge then
    begin
      DelTree(ExpandConstant('{localappdata}\web-stream'), True, True, True);
      // The service's own data, in SYSTEM's profile (scm.rs data_dir).
      DelTree(ExpandConstant('{win}\System32\config\systemprofile\AppData\Local\web-stream'), True, True, True);
    end;
  end;
end;
