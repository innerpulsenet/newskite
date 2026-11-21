; Inno Setup script for the Newskite Windows installer.
;
;   iscc /DAppVersion=1.5.0 /DSourceDir=C:\path\to\stage /DOutputDir=C:\path\to\out
;        [/DIconFile=C:\path\to\newskite.ico] packaging\windows\newskite.iss
;
; SourceDir holds newskite.exe, README.md, copyright.txt and fonts-OFL.txt (the release
; workflow stages them). The installer is per user by default (no administrator prompt,
; installs to %LOCALAPPDATA%\Programs\Newskite); the first page offers an install for all
; users instead. It adds a Start menu entry, optionally a desktop shortcut, and registers
; Newskite for .nzb files and nzb:/nzblnk: links like the Linux desktop entry does.
; Uninstalling keeps the queue, settings and downloads (%APPDATA%\innerpulse\Newskite).

#ifndef AppVersion
  #error Pass /DAppVersion=x.y.z
#endif
#ifndef SourceDir
  #error Pass /DSourceDir=<folder with newskite.exe>
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif

#define AppName "Newskite"
#define AppExe "newskite.exe"
#define ProgId "Newskite.nzb"

[Setup]
; Never change AppId: it is how upgrades and the uninstaller find an existing install.
AppId={{A6C6CF1D-D204-4D6E-8BAC-3EE909179130}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=innerpulse
AppPublisherURL=https://innerpulse.net
AppSupportURL=https://github.com/innerpulsenet/newskite
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
ChangesAssociations=yes
CloseApplications=yes
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
OutputDir={#OutputDir}
OutputBaseFilename=Newskite-{#AppVersion}-windows-x64-setup
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
#ifdef IconFile
SetupIconFile={#IconFile}
#endif

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "associate"; Description: "Open .nzb files and nzb: links with Newskite"; GroupDescription: "File types:"

[Files]
Source: "{#SourceDir}\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\copyright.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\fonts-OFL.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"; Comment: "Download files from Usenet using NZB files"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
; HKA is HKCU for a per-user install and HKLM for an all-users one.
; .nzb files
Root: HKA; Subkey: "Software\Classes\.nzb"; ValueType: string; ValueName: "Content Type"; ValueData: "application/x-nzb"; Tasks: associate; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\.nzb\OpenWithProgids"; ValueType: string; ValueName: "{#ProgId}"; ValueData: ""; Tasks: associate; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\{#ProgId}"; ValueType: string; ValueName: ""; ValueData: "NZB file"; Tasks: associate; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\{#ProgId}\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#AppExe},0"; Tasks: associate
Root: HKA; Subkey: "Software\Classes\{#ProgId}\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#AppExe}"" ""%1"""; Tasks: associate
; Lists Newskite under "Open with" for .nzb files even when another program is the default.
Root: HKA; Subkey: "Software\Classes\Applications\{#AppExe}\SupportedTypes"; ValueType: string; ValueName: ".nzb"; ValueData: ""; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Applications\{#AppExe}\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#AppExe}"" ""%1"""
; nzb: and nzblnk: links
Root: HKA; Subkey: "Software\Classes\nzb"; ValueType: string; ValueName: ""; ValueData: "URL:NZB link"; Tasks: associate; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\nzb"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""; Tasks: associate
Root: HKA; Subkey: "Software\Classes\nzb\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#AppExe},0"; Tasks: associate
Root: HKA; Subkey: "Software\Classes\nzb\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#AppExe}"" ""%1"""; Tasks: associate
Root: HKA; Subkey: "Software\Classes\nzblnk"; ValueType: string; ValueName: ""; ValueData: "URL:NZBLNK link"; Tasks: associate; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\nzblnk"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""; Tasks: associate
Root: HKA; Subkey: "Software\Classes\nzblnk\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#AppExe},0"; Tasks: associate
Root: HKA; Subkey: "Software\Classes\nzblnk\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#AppExe}"" ""%1"""; Tasks: associate

[Run]
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent
