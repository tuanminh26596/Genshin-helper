; Genshin Helper Off-Store Public Installer (Inno Setup 6)
#define MyAppName "Genshin Helper"
#define MyAppVersion "5.8.0"
#define MyAppPublisher "Genshin-Helper Contributors"
#define MyAppExeName "GenshinHelper.exe"
#define MyHostExeName "GenshinHelperHost.exe"
#define FixedExtensionId "nfkijdoblmphmdljfogooajhbldkeojh"
#define NativeHostName "com.genshinhelper.host"
#define ElevatedTaskName "GenshinHelper_Elevated"

[Setup]
AppId={{8F6B7A43-4D7E-4C19-9A2B-1E4500B45000}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
UsePreviousAppDir=yes
UsePreviousGroup=yes
UsePreviousTasks=yes
AllowNoIcons=yes
OutputDir=..\dist\installer
OutputBaseFilename=GenshinHelper-Setup-v{#MyAppVersion}
SetupIconFile=..\assets\icons\app.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
Compression=lzma2/normal
SolidCompression=yes
WizardStyle=modern
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
CloseApplications=yes
CloseApplicationsFilter=*.exe
RestartApplications=no
ShowLanguageDialog=yes

[Languages]
Name: "vietnamese"; MessagesFile: "compiler:Default.isl,Vietnamese.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[CustomMessages]
vietnamese.TaskGroup=Tùy chọn bổ sung:
vietnamese.TaskDesktopIcon=Tạo lối tắt trên màn hình Desktop
vietnamese.TaskAutoStart=Tự chạy nền trong khay hệ thống khi mở Windows
vietnamese.IconGuide=Hướng dẫn cài đặt Extension trình duyệt
vietnamese.IconUninstall=Gỡ cài đặt {#MyAppName}
vietnamese.RunLaunchTray=Khởi động {#MyAppName} trong khay hệ thống ngay bây giờ
vietnamese.RunOpenGuide=Mở hướng dẫn Extension trên trình duyệt (Chrome / Edge / Brave)

english.TaskGroup=Additional options:
english.TaskDesktopIcon=Create a Desktop shortcut
english.TaskAutoStart=Launch quietly in System Tray on Windows logon
english.IconGuide=Browser Extension Setup Guide
english.IconUninstall=Uninstall {#MyAppName}
english.RunLaunchTray=Launch {#MyAppName} in System Tray now
english.RunOpenGuide=Open Browser Extension setup guide (Chrome / Edge / Brave)

[Tasks]
Name: "desktopicon"; Description: "{cm:TaskDesktopIcon}"; GroupDescription: "{cm:TaskGroup}"
Name: "autostart"; Description: "{cm:TaskAutoStart}"; GroupDescription: "{cm:TaskGroup}"

[Files]
Source: "..\dist\GenshinHelper\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "..\extension\*"; DestDir: "{app}\extension"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "install_extension.html"; DestDir: "{app}"; Flags: ignoreversion
Source: "native_host_manifest.json"; DestDir: "{app}"; DestName: "{#NativeHostName}.json"; Flags: ignoreversion

[InstallDelete]
Type: filesandordirs; Name: "{app}\_internal"
Type: files; Name: "{app}\config.json"

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\{#MyAppExeName}"
Name: "{group}\{cm:IconGuide}"; Filename: "{app}\install_extension.html"
Name: "{group}\{cm:IconUninstall}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Registry]
; Native Messaging Host manifest pointers for Chrome, Edge, and Brave (HKLM + HKCU)
Root: HKLM; Subkey: "Software\Google\Chrome\NativeMessagingHosts\{#NativeHostName}"; ValueType: string; ValueName: ""; ValueData: "{app}\{#NativeHostName}.json"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Google\Chrome\NativeMessagingHosts\{#NativeHostName}"; ValueType: string; ValueName: ""; ValueData: "{app}\{#NativeHostName}.json"; Flags: uninsdeletekey
Root: HKLM; Subkey: "Software\Microsoft\Edge\NativeMessagingHosts\{#NativeHostName}"; ValueType: string; ValueName: ""; ValueData: "{app}\{#NativeHostName}.json"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Microsoft\Edge\NativeMessagingHosts\{#NativeHostName}"; ValueType: string; ValueName: ""; ValueData: "{app}\{#NativeHostName}.json"; Flags: uninsdeletekey
Root: HKLM; Subkey: "Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\{#NativeHostName}"; ValueType: string; ValueName: ""; ValueData: "{app}\{#NativeHostName}.json"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\{#NativeHostName}"; ValueType: string; ValueName: ""; ValueData: "{app}\{#NativeHostName}.json"; Flags: uninsdeletekey

[Run]
; Elevated Scheduled Task allows Extension Native Host to wake/start GenshinHelper silently with zero UAC prompt
Filename: "schtasks.exe"; Parameters: "/create /f /rl HIGHEST /sc ONLOGON /tn ""{#ElevatedTaskName}"" /tr ""\""{app}\{#MyAppExeName}\"" --minimized"""; Tasks: autostart; Flags: runhidden waituntilterminated
Filename: "schtasks.exe"; Parameters: "/create /f /rl HIGHEST /sc ONCE /sd 01/01/2000 /st 00:00 /tn ""{#ElevatedTaskName}"" /tr ""\""{app}\{#MyAppExeName}\"" --minimized"""; Tasks: not autostart; Flags: runhidden waituntilterminated

; Post-install actions on Finish page (interactive installs)
Filename: "schtasks.exe"; Parameters: "/run /tn ""{#ElevatedTaskName}"""; Description: "{cm:RunLaunchTray}"; Flags: runhidden nowait postinstall skipifsilent
Filename: "{app}\install_extension.html"; Description: "{cm:RunOpenGuide}"; Flags: shellexec postinstall skipifsilent
; Auto-relaunch elevated app after 1-click silent in-app update (skipped when /AUTOUPDATE=exit applies on user Quit)
Filename: "schtasks.exe"; Parameters: "/run /tn ""{#ElevatedTaskName}"""; Flags: runhidden nowait skipifnotsilent; Check: ShouldRelaunchAfterSilentUpdate

[UninstallRun]
Filename: "taskkill.exe"; Parameters: "/f /im {#MyAppExeName}"; Flags: runhidden; RunOnceId: "KillMainApp"
Filename: "taskkill.exe"; Parameters: "/f /im {#MyHostExeName}"; Flags: runhidden; RunOnceId: "KillHostApp"
Filename: "schtasks.exe"; Parameters: "/delete /f /tn ""{#ElevatedTaskName}"""; Flags: runhidden; RunOnceId: "DeleteElevTask"

[UninstallDelete]
Type: files; Name: "{app}\extension_path.js"

[Code]
function PostMessageW(hWnd: HWND; Msg: Cardinal; wParam, lParam: LongInt): Integer;
  external 'PostMessageW@user32.dll stdcall';

var
  GExistingInstall: Boolean;
  GPrevVersion: string;
  GPrevInstallDir: string;
  ModePage: TInputOptionWizardPage;
  GPurgeUserData: Boolean;
  GExtUninstallSignaled: Boolean;

function IsVietnameseUI(): Boolean;
var
  Lang: string;
begin
  Lang := Lowercase(ActiveLanguage());
  if Lang <> '' then
    Result := (Lang = 'vietnamese')
  else
    Result := (GetUILanguage and $03FF) = $2A;
end;

function ShouldRelaunchAfterSilentUpdate(): Boolean;
begin
  Result := WizardSilent() and (Lowercase(ExpandConstant('{param:AUTOUPDATE|1}')) <> 'exit');
end;

procedure DetectExistingInstall();
var
  UninstKey, Ver, Dir, DefaultDir: string;
begin
  GExistingInstall := False;
  GPrevVersion := '';
  GPrevInstallDir := '';
  UninstKey := 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{8F6B7A43-4D7E-4C19-9A2B-1E4500B45000}_is1';

  if RegQueryStringValue(HKLM, UninstKey, 'DisplayVersion', Ver) or
     RegQueryStringValue(HKCU, UninstKey, 'DisplayVersion', Ver) then
  begin
    GExistingInstall := True;
    GPrevVersion := Trim(Ver);
  end;

  if RegQueryStringValue(HKLM, UninstKey, 'InstallLocation', Dir) or
     RegQueryStringValue(HKCU, UninstKey, 'InstallLocation', Dir) then
  begin
    GPrevInstallDir := RemoveBackslashUnlessRoot(Trim(Dir));
    if GPrevInstallDir <> '' then
      GExistingInstall := True;
  end;

  if not GExistingInstall then
  begin
    DefaultDir := ExpandConstant('{autopf}\{#MyAppName}');
    if FileExists(DefaultDir + '\{#MyAppExeName}') then
    begin
      GExistingInstall := True;
      GPrevInstallDir := DefaultDir;
    end;
  end;
end;

function IsQuickUpdateSelected(): Boolean;
begin
  Result := GExistingInstall and (ModePage <> nil) and (ModePage.SelectedValueIndex = 0);
end;

procedure InitializeWizard();
var
  PageTitle, PageSub, PageDesc, OptUpdate, OptCustom: string;
begin
  DetectExistingInstall();
  if not GExistingInstall then
    Exit;

  if IsVietnameseUI() then
  begin
    WizardForm.WelcomeLabel1.Caption := 'Cập nhật {#MyAppName} {#MyAppVersion}';
    PageTitle := 'Phát hiện bản {#MyAppName} đã cài sẵn';
    if (GPrevVersion <> '') and (GPrevVersion <> '{#MyAppVersion}') then
    begin
      PageSub := 'Phiên bản hiện tại: v' + GPrevVersion + ' → Phiên bản mới: v{#MyAppVersion}';
      OptUpdate := 'Cập nhật {#MyAppName} (v' + GPrevVersion + ' → v{#MyAppVersion}) — giữ nguyên thư mục & cài đặt cũ';
    end
    else
    begin
      PageSub := 'Phiên bản v{#MyAppVersion} đã có sẵn trên máy này';
      OptUpdate := 'Cập nhật / Cài đè {#MyAppName} v{#MyAppVersion} — giữ nguyên thư mục & cài đặt cũ';
    end;
    PageDesc := 'Chọn Cập nhật để nâng cấp nhanh không mất phím tắt, cấu hình HUD hay dữ liệu bản đồ. Sau khi xong, chỉ cần bấm Tải lại (↻) trên Extension trình duyệt.';
    OptCustom := 'Cài đặt tùy chỉnh (chọn lại thư mục cài đặt và tùy chọn khởi động)';
  end
  else
  begin
    WizardForm.WelcomeLabel1.Caption := 'Update {#MyAppName} {#MyAppVersion}';
    PageTitle := 'Existing {#MyAppName} Installation Detected';
    if (GPrevVersion <> '') and (GPrevVersion <> '{#MyAppVersion}') then
    begin
      PageSub := 'Installed: v' + GPrevVersion + ' -> New version: v{#MyAppVersion}';
      OptUpdate := 'Update {#MyAppName} (v' + GPrevVersion + ' -> v{#MyAppVersion}) — keep existing folder & settings';
    end
    else
    begin
      PageSub := 'Version {#MyAppVersion} is already installed on this PC';
      OptUpdate := 'Update / Reinstall {#MyAppName} v{#MyAppVersion} — keep existing folder & settings';
    end;
    PageDesc := 'Choose Update to upgrade in place while keeping your hotkeys, HUD layout, and map datasets. Afterward, simply click Reload (↻) on the browser extension.';
    OptCustom := 'Custom install (choose destination folder and startup tasks)';
  end;

  ModePage := CreateInputOptionPage(wpWelcome, PageTitle, PageSub, PageDesc, True, False);
  ModePage.Add(OptUpdate);
  ModePage.Add(OptCustom);
  ModePage.SelectedValueIndex := 0;
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := False;
  if IsQuickUpdateSelected() then
  begin
    if (PageID = wpSelectDir) or (PageID = wpSelectProgramGroup) or (PageID = wpSelectTasks) then
      Result := True;
  end;
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if (CurPageID = wpReady) and IsQuickUpdateSelected() then
  begin
    if (GPrevInstallDir <> '') and (WizardForm.DirEdit.Text <> GPrevInstallDir) then
      WizardForm.DirEdit.Text := GPrevInstallDir;
    if IsVietnameseUI() then
      WizardForm.NextButton.Caption := '&Cập nhật'
    else
      WizardForm.NextButton.Caption := '&Update';
  end
  else if (CurPageID = wpFinished) and GExistingInstall then
  begin
    if IsVietnameseUI() then
      WizardForm.FinishedLabel.Caption :=
        'Đã cập nhật {#MyAppName} lên phiên bản {#MyAppVersion} thành công!' + #13#10 + #13#10 +
        'Giữ tích trang hướng dẫn trình duyệt bên dưới để xem bước Tải lại (↻) Extension (mất ~5 giây, không cần Load unpacked lại).'
    else
      WizardForm.FinishedLabel.Caption :=
        '{#MyAppName} has been updated to version {#MyAppVersion}!' + #13#10 + #13#10 +
        'Keep the Browser Extension guide checked below for the 1-click Extension Reload (↻) step (~5 seconds, no need to Load unpacked again).';
  end;
end;

function IsExeRunningWmi(const ExeName: string): Boolean;
var
  FSWbemLocator, FWMIService, FWbemObjectSet: Variant;
begin
  Result := False;
  try
    FSWbemLocator := CreateOleObject('WbemScripting.SWbemLocator');
    FWMIService := FSWbemLocator.ConnectServer('', 'root\CIMV2', '', '');
    FWbemObjectSet := FWMIService.ExecQuery(
      'SELECT ProcessId FROM Win32_Process WHERE Name = ''' + ExeName + ''''
    );
    Result := (FWbemObjectSet.Count > 0);
  except
    Result := False;
  end;
end;

function IsAnyAppProcessRunning(): Boolean;
begin
  if (FindWindowByWindowName('GenshinHelperTray') <> 0) or
     CheckForMutexes('Local\GenshinHelper_SingleInstance_v4') then
  begin
    Result := True;
    Exit;
  end;
  Result := IsExeRunningWmi('{#MyAppExeName}') or IsExeRunningWmi('{#MyHostExeName}');
end;

function HasUninstallSwitch(const SwitchName: string): Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
  begin
    if CompareText(ParamStr(I), SwitchName) = 0 then
    begin
      Result := True;
      Exit;
    end;
  end;
end;

function TryGracefulHttpShutdown(const RequestExtUninstall: Boolean): Boolean;
var
  HttpReq: Variant;
  TrayHwnd: HWND;
  Payload: string;
begin
  Result := False;
  if RequestExtUninstall then
    Payload := '{"uninstall_extension":true,"skip_exit_update":true}'
  else
    Payload := '{"skip_exit_update":true}';
  try
    HttpReq := CreateOleObject('WinHttp.WinHttpRequest.5.1');
    HttpReq.SetTimeouts(400, 400, 800, 1200);
    HttpReq.Open('POST', 'http://127.0.0.1:8765/shutdown', False);
    HttpReq.SetRequestHeader('Content-Type', 'application/json');
    HttpReq.Send(Payload);
    Result := (HttpReq.Status = 200);
  except
    Result := False;
  end;

  if RequestExtUninstall and Result then
    Sleep(240);

  TrayHwnd := FindWindowByWindowName('GenshinHelperTray');
  if TrayHwnd <> 0 then
    PostMessageW(TrayHwnd, $0010, 0, 0);
end;

function StopRunningAppProcesses(const RequestExtUninstall: Boolean): Boolean;
var
  I, ResultCode: Integer;
begin
  if TryGracefulHttpShutdown(RequestExtUninstall) and RequestExtUninstall then
    GExtUninstallSignaled := True;
  for I := 1 to 12 do
  begin
    if not IsAnyAppProcessRunning() then
    begin
      Result := True;
      Exit;
    end;
    Sleep(200);
  end;

  if IsExeRunningWmi('{#MyAppExeName}') or
     (FindWindowByWindowName('GenshinHelperTray') <> 0) or
     CheckForMutexes('Local\GenshinHelper_SingleInstance_v4') then
  begin
    Exec(ExpandConstant('{sys}\taskkill.exe'), '/f /im {#MyAppExeName}', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  end;
  if IsExeRunningWmi('{#MyHostExeName}') then
  begin
    Exec(ExpandConstant('{sys}\taskkill.exe'), '/f /im {#MyHostExeName}', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  end;

  for I := 1 to 12 do
  begin
    if not IsAnyAppProcessRunning() then
    begin
      Result := True;
      Exit;
    end;
    Sleep(200);
  end;

  Result := not IsAnyAppProcessRunning();
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  PromptMsg, AbortMsg, RetryMsg, SilentFailMsg: string;
begin
  Result := '';
  if IsVietnameseUI() then
  begin
    PromptMsg :=
      '{#MyAppName} đang chạy nền (hoặc trong khay hệ thống).' + #13#10 + #13#10 +
      'Bạn có muốn tự động đóng ứng dụng để tiếp tục cài đặt phiên bản {#MyAppVersion} không?';
    AbortMsg := 'Đã hủy cài đặt vì {#MyAppName} vẫn đang chạy. Hãy thoát ứng dụng rồi chạy lại bộ cài.';
    RetryMsg := 'Không thể tự đóng hoàn toàn {#MyAppName}. Hãy thoát ứng dụng từ khay hệ thống rồi bấm Thử lại.';
    SilentFailMsg := 'Không thể đóng tiến trình {#MyAppName} đang chạy khi cài đặt im lặng.';
  end
  else
  begin
    PromptMsg :=
      '{#MyAppName} is currently running in the background (or system tray).' + #13#10 + #13#10 +
      'Would you like to close it automatically to continue installing v{#MyAppVersion}?';
    AbortMsg := 'Installation cancelled because {#MyAppName} is still running. Please quit the app and re-run Setup.';
    RetryMsg := 'Could not automatically stop {#MyAppName}. Please close it manually from the system tray and click Retry.';
    SilentFailMsg := 'Unable to close running {#MyAppName} process during silent install.';
  end;

  while IsAnyAppProcessRunning() do
  begin
    if WizardSilent() then
    begin
      if not StopRunningAppProcesses(False) then
      begin
        Result := SilentFailMsg;
      end;
      Break;
    end;

    if MsgBox(PromptMsg, mbConfirmation, MB_YESNO) <> IDYES then
    begin
      Result := AbortMsg;
      Exit;
    end;

    if StopRunningAppProcesses(False) then
      Break;

    if MsgBox(RetryMsg, mbError, MB_RETRYCANCEL) <> IDRETRY then
    begin
      Result := AbortMsg;
      Exit;
    end;
  end;
end;

function InitializeUninstall(): Boolean;
begin
  Result := True;
  GExtUninstallSignaled := False;
  GPurgeUserData := HasUninstallSwitch('/PURGEDATA');
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  PurgeMsg, RoamingDir, LocalDir, DoneMsg: string;
begin
  if CurUninstallStep = usUninstall then
  begin
    if (not UninstallSilent()) and (not GPurgeUserData) then
    begin
      if IsVietnameseUI() then
      begin
        PurgeMsg :=
          'Bạn có muốn xóa sạch toàn bộ dữ liệu người dùng không?' + #13#10 + #13#10 +
          '• Cấu hình phím tắt, ngôn ngữ & bố cục HUD (%APPDATA%\GenshinHelper)' + #13#10 +
          '• Cache dữ liệu bản đồ HoYoLAB (~15 MB) & bộ nhớ đệm cập nhật (%LOCALAPPDATA%\GenshinHelper)' + #13#10 +
          '• Mở xác nhận gỡ Browser Extension (nếu trình duyệt đang mở)' + #13#10 + #13#10 +
          'Chọn Không (No) nếu bạn chỉ muốn gỡ ra cài lại và giữ nguyên cài đặt cũ.';
      end
      else
      begin
        PurgeMsg :=
          'Do you also want to completely remove all user data?' + #13#10 + #13#10 +
          '• Hotkeys, language & HUD layout (%APPDATA%\GenshinHelper)' + #13#10 +
          '• HoYoLAB map cache (~15 MB) & update cache (%LOCALAPPDATA%\GenshinHelper)' + #13#10 +
          '• Prompt to remove the Browser Extension (if your browser is open)' + #13#10 + #13#10 +
          'Choose No if you plan to reinstall and keep your existing settings.';
      end;
      GPurgeUserData := (MsgBox(PurgeMsg, mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES);
    end;

    if IsAnyAppProcessRunning() then
      StopRunningAppProcesses(GPurgeUserData);
  end
  else if (CurUninstallStep = usPostUninstall) and GPurgeUserData then
  begin
    RoamingDir := ExpandConstant('{userappdata}\GenshinHelper');
    LocalDir := ExpandConstant('{localappdata}\GenshinHelper');
    if DirExists(RoamingDir) then
      DelTree(RoamingDir, True, True, True);
    if DirExists(LocalDir) then
      DelTree(LocalDir, True, True, True);

    if not UninstallSilent() then
    begin
      if IsVietnameseUI() then
      begin
        if GExtUninstallSignaled then
          DoneMsg :=
            'Đã xóa sạch hoàn toàn {#MyAppName} cùng toàn bộ dữ liệu người dùng!' + #13#10 + #13#10 +
            'Nếu hộp thoại gỡ Extension chưa hiện trên trình duyệt, bạn có thể mở chrome://extensions (hoặc Edge/Brave) và bấm Remove thẻ Genshin Helper.'
        else
          DoneMsg :=
            'Đã xóa sạch hoàn toàn {#MyAppName} cùng toàn bộ dữ liệu người dùng!' + #13#10 + #13#10 +
            'Bước cuối (nếu bạn có cài Extension): mở chrome://extensions (hoặc Edge/Brave) và bấm Remove thẻ Genshin Helper (hoặc bấm nút Gỡ Extension ngay trong Popup).';
      end
      else
      begin
        if GExtUninstallSignaled then
          DoneMsg :=
            '{#MyAppName} and all user data have been completely removed!' + #13#10 + #13#10 +
            'If the browser removal dialog did not appear, open chrome://extensions (or Edge/Brave) and click Remove on Genshin Helper.'
        else
          DoneMsg :=
            '{#MyAppName} and all user data have been completely removed!' + #13#10 + #13#10 +
            'Final step (if you installed the Extension): open chrome://extensions (or Edge/Brave) and click Remove on Genshin Helper (or click Remove Extension inside its Popup).';
      end;
      MsgBox(DoneMsg, mbInformation, MB_OK);
    end;
  end;
end;

function EscapeJsonPath(const S: string): string;
begin
  Result := S;
  StringChangeEx(Result, '\', '\\', True);
  StringChangeEx(Result, '"', '\"', True);
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  AppDir, HostExe, ExtDir, ManifestPath, PathJsPath, JsonText, JsText, InstallMode, InstallLang: string;
begin
  if CurStep = ssPostInstall then
  begin
    AppDir := ExpandConstant('{app}');
    HostExe := AppDir + '\{#MyHostExeName}';
    ExtDir := AppDir + '\extension';
    ManifestPath := AppDir + '\{#NativeHostName}.json';
    PathJsPath := AppDir + '\extension_path.js';

    JsonText :=
      '{' + #13#10 +
      '  "name": "{#NativeHostName}",' + #13#10 +
      '  "description": "Genshin Helper Native Messaging Host (Wake & Control Desktop Sync Server)",' + #13#10 +
      '  "path": "' + EscapeJsonPath(HostExe) + '",' + #13#10 +
      '  "type": "stdio",' + #13#10 +
      '  "allowed_origins": [' + #13#10 +
      '    "chrome-extension://{#FixedExtensionId}/"' + #13#10 +
      '  ]' + #13#10 +
      '}' + #13#10;
    SaveStringToFile(ManifestPath, JsonText, False);

    if GExistingInstall then
      InstallMode := 'update'
    else
      InstallMode := 'fresh';

    if IsVietnameseUI() then
      InstallLang := 'vi'
    else
      InstallLang := 'en';

    JsText :=
      'window.__GH_EXT_PATH = "' + EscapeJsonPath(ExtDir) + '";' + #13#10 +
      'window.__GH_INSTALL_MODE = "' + InstallMode + '";' + #13#10 +
      'window.__GH_INSTALL_LANG = "' + InstallLang + '";' + #13#10 +
      'window.__GH_PREV_VERSION = "' + EscapeJsonPath(GPrevVersion) + '";' + #13#10;
    SaveStringToFile(PathJsPath, JsText, False);
  end;
end;
