@echo off

:: Check if running as administrator
net session >nul 2>&1
if %errorLevel% neq 0 (
    echo This script must be run as Administrator.
    echo Please right-click on the batch file and select "Run as administrator".
    echo.
    pause
    exit /b 1
)

:: Unregister the previous DLL version
regsvr32 /u "%~dp0..\target\x86_64-pc-windows-msvc\release\win_svg_thumbs_x64.dll"
regsvr32 /u "%~dp0..\target\i686-pc-windows-msvc\release\win_svg_thumbs_x86.dll"

:: Get the absolute path to the DLL
set "SCRIPT_DIR=%~dp0"
pushd "%SCRIPT_DIR%\.."
set "DLL_PATH=%CD%\target\x86_64-pc-windows-msvc\release\win_svg_thumbs_x64.dll"
set "DLL_PATH_X86=%CD%\target\i686-pc-windows-msvc\release\win_svg_thumbs_x86.dll"
popd
::echo  "%DLL_PATH%"

:: Now use DLL_PATH in your unlock/delete commands
:: (Requires IObit Unlocker to be installed) We can't delete the DLL if it's in use, which is often the case even after unregistering it. So this will unlock it and delete it.
"C:\Program Files (x86)\IObit\IObit Unlocker\IObitUnlocker.exe" /Delete /Normal "%DLL_PATH%"
"C:\Program Files (x86)\IObit\IObit Unlocker\IObitUnlocker.exe" /Delete /Normal "%DLL_PATH_X86%"

:: Run the build. Tried to to make it run as a non-admin, but couldn't get it to work.
cd /d %~dp0..
cargo build --release --target=x86_64-pc-windows-msvc
cargo build --release --target=i686-pc-windows-msvc
cargo build --release --target=aarch64-pc-windows-msvc

:: Re-register the new DLL version. Only bother re-registering the 64 bit one.
regsvr32 "%~dp0..\target\x86_64-pc-windows-msvc\release\win_svg_thumbs_x64.dll"
regsvr32 "%~dp0..\target\i686-pc-windows-msvc\release\win_svg_thumbs_x86.dll"

:: Copy both DLLs to the MSI Installer folder
copy /Y "%~dp0..\target\x86_64-pc-windows-msvc\release\win_svg_thumbs_x64.dll" "%~dp0MSI Installer\win_svg_thumbs_x64.dll"
copy /Y "%~dp0..\target\i686-pc-windows-msvc\release\win_svg_thumbs_x86.dll" "%~dp0MSI Installer\win_svg_thumbs_x86.dll"
copy /Y "%~dp0..\target\aarch64-pc-windows-msvc\release\win_svg_thumbs_arm64.dll" "%~dp0MSI Installer\win_svg_thumbs_arm64.dll"
