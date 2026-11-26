; web-stream host installer (Inno Setup 6).
#ifndef AppVersion
  #define AppVersion "0.0.0-dev"
#endif
#ifndef AppNumericVersion
  #define AppNumericVersion "0.0.0"
#endif
#define AppName "web-stream host"
#define AppExe "ws-host-gui.exe"
#define ServiceExe "ws-host.exe"
#define ServiceName "web-stream"
#define FirewallRule "web-stream host"
#define RunValue "web-stream host"
#define ViGEm "ViGEmBus_1.22.0_x64_x86_arm64.exe"
#define Cloudflared "cloudflared-windows-amd64.exe"
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
CloseApplications=no
RestartIfNeededByRun=no

[Messages]
SelectTasksLabel2=Choose what to set up, then click Install.
WizardSelectTasks=Set up web-stream host

[CustomMessages]
AppStillRunning=web-stream host is still running. Quit it from its tray icon (right-click, Quit), then click Retry.

[Tasks]
Name: "gamepad"; Description: "Gamepad driver (ViGEmBus), so controllers in the browser work in games"
Name: "mic"; Description: "Microphone driver (Steam Streaming Microphone), so the browser's mic reaches Windows"; Check: SteamMicAvailable
Name: "service"; Description: "Allow use at the login screen and with admin programs (installs a Windows service)"; Flags: unchecked
Name: "autostart"; Description: "Start with Windows (in the tray)"

[Files]
Source: "..\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "redist\{#Cloudflared}"; DestDir: "{app}"; DestName: "cloudflared.exe"; Flags: ignoreversion
Source: "third-party\*"; DestDir: "{app}\licenses"; Flags: ignoreversion
Source: "redist\{#ViGEm}"; DestDir: "{tmp}"; Flags: deleteafterinstall; Tasks: gamepad; Check: not ViGEmInstalled

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"; Comment: "Stream this PC to a browser"

[Run]
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""{#FirewallRule}"""; Flags: runhidden; StatusMsg: "Adding firewall rules..."
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""{#FirewallRule}"" dir=in action=allow program=""{app}\{#AppExe}"" enable=yes profile=any"; Flags: runhidden
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""{#FirewallRule}"" dir=in action=allow program=""{app}\{#ServiceExe}"" enable=yes profile=any"; Flags: runhidden
Filename: "{tmp}\{#ViGEm}"; Parameters: "/exenoui /qn /norestart"; Tasks: gamepad; Check: not ViGEmInstalled; StatusMsg: "Installing the gamepad driver..."
Filename: "{app}\{#AppExe}"; Parameters: "setup mic-driver"; Tasks: mic; Flags: runhidden; StatusMsg: "Installing the microphone driver..."
Filename: "{app}\{#AppExe}"; Parameters: "service update --quiet"; Check: ServiceInstalled; Flags: runhidden; StatusMsg: "Updating the service..."
Filename: "{app}\{#AppExe}"; Parameters: "service install --quiet"; Tasks: service; Check: not ServiceInstalled; Flags: runhidden; StatusMsg: "Installing the service..."
Filename: "{app}\{#AppExe}"; Description: "Open web-stream host"; Flags: postinstall nowait runasoriginaluser skipifsilent; Check: not HasParam('/RELAUNCH') and not AppWasRunning
Filename: "{app}\{#AppExe}"; Parameters: "--hidden"; Flags: nowait runasoriginaluser; Check: AppWasRunning and not HasParam('/RELAUNCH')
Filename: "{app}\{#AppExe}"; Flags: nowait runasoriginaluser; Check: HasParam('/RELAUNCH')

[UninstallRun]
Filename: "{app}\{#AppExe}"; Parameters: "service uninstall --quiet"; Check: ServiceInstalled; Flags: runhidden; RunOnceId: "service"
Filename: "{app}\{#AppExe}"; Parameters: "setup upnp-remove"; Flags: runhidden; RunOnceId: "upnp"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""{#FirewallRule}"""; Flags: runhidden; RunOnceId: "firewall"

[UninstallDelete]
Type: files; Name: "{app}\{#ServiceExe}"
Type: files; Name: "{app}\{#ServiceExe}.new"
Type: dirifempty; Name: "{app}\licenses"
Type: dirifempty; Name: "{app}"

[Code]
const
  RunKey = 'Software\Microsoft\Windows\CurrentVersion\Run';
  UninstallKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{918435C9-8423-4488-9252-A4724B1CB18E}_is1';
  InstanceMutex = 'Local\web-stream-host-gui';
  QuitEvent = 'Local\web-stream-host-gui-quit';
  EVENT_MODIFY_STATE = $0002;
  QuitTimeoutMs = 20000;

var
  WasInstalled: Boolean;
  AppRunningAtStart: Boolean;

function OpenEvent(DesiredAccess: DWORD; InheritHandle: BOOL; Name: String): THandle;
  external 'OpenEventW@kernel32.dll stdcall';
function SetEvent(Event: THandle): BOOL;
  external 'SetEvent@kernel32.dll stdcall';
function CloseHandle(Handle: THandle): BOOL;
  external 'CloseHandle@kernel32.dll stdcall';

function AppWasRunning: Boolean;
begin
  Result := AppRunningAtStart;
end;

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

function IsSilentRun: Boolean;
begin
  Result := HasParam('/SILENT') or HasParam('/VERYSILENT');
end;

function QuitRunningApp: Boolean;
var
  Event: THandle;
  Waited: Integer;
begin
  Result := not CheckForMutexes(InstanceMutex);
  if Result then
    Exit;
  AppRunningAtStart := True;
  Event := OpenEvent(EVENT_MODIFY_STATE, False, QuitEvent);
  if Event <> 0 then
  begin
    SetEvent(Event);
    CloseHandle(Event);
  end;
  Waited := 0;
  while CheckForMutexes(InstanceMutex) and (Waited < QuitTimeoutMs) do
  begin
    Sleep(250);
    Waited := Waited + 250;
  end;
  Result := not CheckForMutexes(InstanceMutex);
end;

function EnsureAppClosed(Silent: Boolean): Boolean;
begin
  Result := QuitRunningApp;
  while not Result do
  begin
    if Silent then
    begin
      Log('web-stream host did not quit');
      Exit;
    end;
    if MsgBox(CustomMessage('AppStillRunning'), mbError, MB_RETRYCANCEL) = IDCANCEL then
      Exit;
    Result := QuitRunningApp;
  end;
end;

function InitializeSetup: Boolean;
begin
  WasInstalled := RegValueExists(HKLM, UninstallKey, 'UninstallString');
  Result := EnsureAppClosed(IsSilentRun);
end;

function InitializeUninstall: Boolean;
begin
  Result := EnsureAppClosed(UninstallSilent);
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := WasInstalled and ((PageID = wpSelectTasks) or ((PageID = wpFinished) and AppRunningAtStart));
end;

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

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
begin
  Result := '';
  if ServiceInstalled then
    if not Exec(ExpandConstant('{sys}\net.exe'), 'stop {#ServiceName}', '', SW_HIDE,
        ewWaitUntilTerminated, Code) then
      Log('net stop {#ServiceName} could not run');
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
      DelTree(ExpandConstant('{win}\System32\config\systemprofile\AppData\Local\web-stream'), True, True, True);
    end;
  end;
end;
