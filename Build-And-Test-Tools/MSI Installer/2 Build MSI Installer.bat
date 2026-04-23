@echo off
setlocal

echo Parsing version from cargo.toml...

REM Find the line starting with "version" in the toml file two directories up.
REM The FOR loop splits the line by spaces and grabs the 3rd token (e.g., "1.8.0.0").
FOR /F "tokens=3 delims= " %%v IN ('findstr /R "^version" ..\..\cargo.toml') DO (
    SET "PRODUCT_VERSION_QUOTED=%%v"
)

IF NOT DEFINED PRODUCT_VERSION_QUOTED (
    echo ERROR: Could not find version in ..\..\cargo.toml
    pause
    exit /b 1
)

REM Remove the surrounding quotes from the version string.
SET "PRODUCT_VERSION=%PRODUCT_VERSION_QUOTED:"=%"

echo Found version: %PRODUCT_VERSION%
echo.
echo Starting WiX build...

REM Run the wix build command, passing the parsed version.
wix build win_svg_thumbs.wxs -d ProductVersion="%PRODUCT_VERSION%" -o "SVG-Thumbnail-Extension-Installer_%PRODUCT_VERSION%_x64.msi" -ext WixToolset.UI.wixext -arch x64
wix build win_svg_thumbs.wxs -d ProductVersion="%PRODUCT_VERSION%" -o "SVG-Thumbnail-Extension-Installer_%PRODUCT_VERSION%_arm64.msi" -ext WixToolset.UI.wixext -arch arm64

echo.
echo Build finished. Press any key to exit.
pause >nul

