@echo off
setlocal enabledelayedexpansion

:: Check if resource hacker is in the current directory
SET "RESOURCE_HACKER=ResourceHacker.exe"
IF NOT EXIST "%RESOURCE_HACKER%" (
    echo ERROR: ResourceHacker.exe not found in the current directory.
    echo Please ensure it is present in the MSI Installer folder.
    pause
    exit /b 1
)

echo Parsing version from cargo.toml...

REM Find the line starting with "version" in the toml file two directories up.
REM The FOR loop splits the line by spaces and grabs the 3rd token (e.g., "1.8.0").
FOR /F "tokens=3 delims= " %%v IN ('findstr /R "^version" ..\..\cargo.toml') DO (
    SET "PRODUCT_VERSION_QUOTED=%%v"
)

IF NOT DEFINED PRODUCT_VERSION_QUOTED (
    echo ERROR: Could not find version in ..\..\cargo.toml
    pause
    exit /b 1
)

REM Remove the surrounding quotes from the version string.
SET "PRODUCT_VERSION_3=%PRODUCT_VERSION_QUOTED:"=%"

REM Add .0 to make it 4-part version for Windows resources
SET "PRODUCT_VERSION=%PRODUCT_VERSION_3%.0"

echo Found version: %PRODUCT_VERSION_3% (using %PRODUCT_VERSION% for resources)
echo.

REM Create version resource file
echo Creating version resource file...
(
echo 1 VERSIONINFO
echo FILEVERSION %PRODUCT_VERSION:.=,%
echo PRODUCTVERSION %PRODUCT_VERSION:.=,%
echo FILEOS 0x4
echo FILETYPE 0x1
echo {
echo BLOCK "StringFileInfo"
echo {
echo     BLOCK "040904E4"
echo     {
echo         VALUE "CompanyName", "ThioJoe\0"
echo         VALUE "FileDescription", "Thio's SVG Thumbnail Extension\0"
echo         VALUE "FileVersion", "%PRODUCT_VERSION%\0"
echo         VALUE "ProductName", "Thio's SVG Thumbnail Extension\0"
echo         VALUE "InternalName", "Thio's SVG Thumbnail Extension\0"
echo         VALUE "LegalCopyright", "Copyright 2025\0"
echo         VALUE "OriginalFilename", "win_svg_thumbs_x64.dll\0"
echo         VALUE "ProductVersion", "%PRODUCT_VERSION%\0"
echo     }
echo }
echo.
echo BLOCK "VarFileInfo"
echo {
echo     VALUE "Translation", 0x0409 0x04E4  
echo }
echo }
) > resources.rc

REM Compile resource file
echo Compiling resource file...
ResourceHacker.exe -open resources.rc -save resources.res -action compile -log CONSOLE

IF ERRORLEVEL 1 (
    echo ERROR: Failed to compile resource file
    pause
    exit /b 1
)

REM Update x64 DLL if it exists
SET "DLL_X64=win_svg_thumbs_x64.dll"
IF EXIST "%DLL_X64%" (
    echo Updating version info for x64 DLL...
    ResourceHacker.exe -open "%DLL_X64%" -save "%DLL_X64%" -resource resources.res -action addoverwrite -mask VersionInfo, -log CONSOLE
    IF ERRORLEVEL 1 (
        echo WARNING: Failed to update x64 DLL version info
    ) ELSE (
        echo Successfully updated x64 DLL version info
    )
) ELSE (
    echo WARNING: x64 DLL not found at %DLL_X64%
)

REM Update x86 DLL if it exists
SET "DLL_X86=win_svg_thumbs_x86.dll"
IF EXIST "%DLL_X86%" (
    echo Updating version info for x86 DLL...
    REM Create x86-specific resource file
    (
    echo 1 VERSIONINFO
    echo FILEVERSION %PRODUCT_VERSION:.=,%
    echo PRODUCTVERSION %PRODUCT_VERSION:.=,%
    echo FILEOS 0x4
    echo FILETYPE 0x1
    echo {
    echo BLOCK "StringFileInfo"
    echo {
    echo     BLOCK "040904E4"
    echo     {
    echo         VALUE "CompanyName", "ThioJoe\0"
    echo         VALUE "FileDescription", "Thio's SVG Thumbnail Extension\0"
    echo         VALUE "FileVersion", "%PRODUCT_VERSION%\0"
    echo         VALUE "ProductName", "Thio's SVG Thumbnail Extension\0"
    echo         VALUE "InternalName", "Thio's SVG Thumbnail Extension\0"
    echo         VALUE "LegalCopyright", "Copyright 2025\0"
    echo         VALUE "OriginalFilename", "win_svg_thumbs_x86.dll\0"
    echo         VALUE "ProductVersion", "%PRODUCT_VERSION%\0"
    echo     }
    echo }
    echo.
    echo BLOCK "VarFileInfo"
    echo {
    echo     VALUE "Translation", 0x0409 0x04E4  
    echo }
    echo }
    ) > resources_x86.rc
    
    ResourceHacker.exe -open resources_x86.rc -save resources_x86.res -action compile -log CONSOLE
    ResourceHacker.exe -open "%DLL_X86%" -save "%DLL_X86%" -resource resources_x86.res -action addoverwrite -mask VersionInfo, -log CONSOLE
    IF ERRORLEVEL 1 (
        echo WARNING: Failed to update x86 DLL version info
    ) ELSE (
        echo Successfully updated x86 DLL version info
    )
) ELSE (
    echo WARNING: x86 DLL not found at %DLL_X86%
)

REM Update x86 DLL if it exists
SET "DLL_ARM64=win_svg_thumbs_arm64.dll"
IF EXIST "%DLL_ARM64%" (
    echo Updating version info for ARM64 DLL...
    REM Create ARM64-specific resource file
    (
    echo 1 VERSIONINFO
    echo FILEVERSION %PRODUCT_VERSION:.=,%
    echo PRODUCTVERSION %PRODUCT_VERSION:.=,%
    echo FILEOS 0x4
    echo FILETYPE 0x1
    echo {
    echo BLOCK "StringFileInfo"
    echo {
    echo     BLOCK "040904E4"
    echo     {
    echo         VALUE "CompanyName", "ThioJoe\0"
    echo         VALUE "FileDescription", "Thio's SVG Thumbnail Extension\0"
    echo         VALUE "FileVersion", "%PRODUCT_VERSION%\0"
    echo         VALUE "ProductName", "Thio's SVG Thumbnail Extension\0"
    echo         VALUE "InternalName", "Thio's SVG Thumbnail Extension\0"
    echo         VALUE "LegalCopyright", "Copyright 2025\0"
    echo         VALUE "OriginalFilename", "win_svg_thumbs_arm64.dll\0"
    echo         VALUE "ProductVersion", "%PRODUCT_VERSION%\0"
    echo     }
    echo }
    echo.
    echo BLOCK "VarFileInfo"
    echo {
    echo     VALUE "Translation", 0x0409 0x04E4  
    echo }
    echo }
    ) > resources_arm64.rc
    
    ResourceHacker.exe -open resources_arm64.rc -save resources_arm64.res -action compile -log CONSOLE
    ResourceHacker.exe -open "%DLL_ARM64%" -save "%DLL_ARM64%" -resource resources_arm64.res -action addoverwrite -mask VersionInfo, -log CONSOLE
    IF ERRORLEVEL 1 (
        echo WARNING: Failed to update arm64 DLL version info
    ) ELSE (
        echo Successfully updated arm64 DLL version info
    )
) ELSE (
    echo WARNING: arm64 DLL not found at %DLL_ARM64%
)


REM Clean up temporary files
IF EXIST resources.rc del resources.rc
IF EXIST resources.res del resources.res
IF EXIST resources_x86.rc del resources_x86.rc
IF EXIST resources_x86.res del resources_x86.res
IF EXIST resources_arm64.rc del resources_arm64.rc
IF EXIST resources_arm64.res del resources_arm64.res

echo.
echo Finished updating version info for DLLs.
