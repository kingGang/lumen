; Lumen 安装包脚本（Inno Setup 6+）——F3 热更分发用。
;
; 用法：
;   iscc /DMyAppVersion=0.2.0 installer\lumen.iss
; 产物：dist\Lumen-Setup-<version>.exe（覆盖安装、关闭运行中进程、可重启）。
;
; 设计要点（需求池 F3）：
;   - 覆盖安装时由安装器关闭运行中的 lumen.exe 再替换（CloseApplications）。
;   - Lumen 客户端自身不碰 exe 自替换；热更时它下载本安装包、拉起、然后
;     优雅退出，安装器接手替换并（按用户选择）重启。
;   - base64/编码无关：安装包是标准 PE，跨机分发无 integration.ps1 的编码坑。

#ifndef MyAppVersion
  #define MyAppVersion "0.1.0"
#endif

#define MyAppName "Lumen"
#define MyAppPublisher "Lumen"
#define MyAppExeName "lumen.exe"
#define MyAppURL "https://github.com/jimhy/lumen"

[Setup]
; AppId 唯一标识本应用（升级时据此识别同一程序，勿改）。
AppId={{8F2A9C14-3B7E-4D6A-9E1F-2C5A7B8D0E3F}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
; 默认装到 Program Files\Lumen（64 位）。
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
; 不强制管理员：优先按用户安装（lowest），免 UAC 也能更新；
; 装到 Program Files 时 Inno 会按需提权。
PrivilegesRequiredOverridesAllowed=dialog commandline
DisableProgramGroupPage=yes
; 64 位专用（与 wgpu/ConPTY 一致）。
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; 覆盖安装时关闭正在运行的 Lumen，安装完按需重启（热更核心）。
CloseApplications=yes
RestartApplications=yes
; 输出。
OutputDir=..\dist
OutputBaseFilename=Lumen-Setup-{#MyAppVersion}
SetupIconFile=..\icons\lumen.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern

[Languages]
Name: "chinesesimp"; MessagesFile: "compiler:Default.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
; 主程序（构建产物）。release 构建已嵌入图标资源（winresource）。
Source: "..\target\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
; 图标随包（卸载显示/快捷方式备用）。
Source: "..\icons\lumen.ico"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\卸载 {#MyAppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
; 安装完成后按需启动（热更场景：覆盖安装后重启 Lumen）。
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#MyAppName}}"; Flags: nowait postinstall skipifsilent
