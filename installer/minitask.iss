; MiniTask セットアップ (Inno Setup 6)
; 管理者権限で動作するアプリのため、Program Files 配下にインストールする。

#define MyAppName "MiniTask"
#ifndef MyAppVersion
  #define MyAppVersion "0.2.1"
#endif
#define MyAppExeName "minitask.exe"
#define MyIFEOKey "SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\taskmgr.exe"
#define MyShellMenu "Software\Classes\DesktopBackground\Shell\MiniTask"
#define MyShellMenuDir "Software\Classes\Directory\Background\shell\MiniTask"

[Setup]
AppId={{C85055DE-9602-4343-A1E0-8282041DC915}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher=MiniTask
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
OutputDir=..\dist
OutputBaseFilename=MiniTask-{#MyAppVersion}-setup
SetupIconFile=..\assets\app.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
CloseApplications=yes

[Languages]
Name: "japanese"; MessagesFile: "compiler:Languages\Japanese.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "デスクトップにショートカットを作成"; Flags: unchecked
Name: "replace_taskmgr"; Description: "タスクマネージャーの代わりにMiniTaskを起動"; Flags: unchecked
Name: "launchapp"; Description: "インストール後にMiniTaskを起動"; Flags: unchecked

[Files]
Source: "..\target\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Registry]
; チェックされた場合のみ IFEO を登録。アンインストール時にこの値を削除する。
Root: HKLM; Subkey: "{#MyIFEOKey}"; ValueType: string; ValueName: "Debugger"; ValueData: """{app}\{#MyAppExeName}"""; Tasks: replace_taskmgr; Flags: uninsdeletevalue

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#MyAppName}}"; Flags: nowait postinstall skipifsilent; Tasks: launchapp

[Code]
// トレイ常駐しているため、入れ替え前に実行中プロセスを停止する
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
begin
  Exec('taskkill', '/F /IM minitask.exe', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Result := '';
end;

// アンインストール時: 自分が登録した IFEO / シェルメニューのみ片付ける。
// 設定 (HKCU\Software\MiniTask) はユーザーデータとして残す。
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  ResultCode: Integer;
  Value: String;
  Expected: String;
begin
  if CurUninstallStep <> usUninstall then
    Exit;

  Exec('taskkill', '/F /IM minitask.exe', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);

  Expected := '"' + ExpandConstant('{app}\{#MyAppExeName}') + '"';
  if RegQueryStringValue(HKLM, '{#MyIFEOKey}', 'Debugger', Value) then
  begin
    if CompareText(Value, Expected) = 0 then
      RegDeleteValue(HKLM, '{#MyIFEOKey}', 'Debugger');
  end;

  if RegQueryStringValue(HKCU, '{#MyShellMenu}\command', '', Value) then
  begin
    if CompareText(Value, Expected) = 0 then
      RegDeleteKeyIncludingSubkeys(HKCU, '{#MyShellMenu}');
  end;
  if RegQueryStringValue(HKCU, '{#MyShellMenuDir}\command', '', Value) then
  begin
    if CompareText(Value, Expected) = 0 then
      RegDeleteKeyIncludingSubkeys(HKCU, '{#MyShellMenuDir}');
  end;
end;
