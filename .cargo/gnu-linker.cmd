@echo off
setlocal EnableDelayedExpansion
for /f "usebackq delims=" %%I in (`rustc --print sysroot`) do set "RUST_SYSROOT=%%I"
if not defined RUST_SYSROOT exit /b 1
set "SELF_CONTAINED=%RUST_SYSROOT%\lib\rustlib\x86_64-pc-windows-gnu\bin\self-contained"
set "SELF_CONTAINED_LIB=%RUST_SYSROOT%\lib\rustlib\x86_64-pc-windows-gnu\lib\self-contained"
set "LINK_ARGS=%*"
set "STARTUP_OBJECT=crt2.o"
set "HAS_STARTUP=0"
:scan_args
if "%~1"=="" goto scanned_args
set "ARG=%~1"
if "!ARG:~0,1!"=="@" (
  set "RESP_FILE=!ARG:~1!"
  findstr /I /C:"-shared" "!RESP_FILE!" >nul 2>nul && set "STARTUP_OBJECT=dllcrt2.o"
  findstr /I /C:"crt2.o" /C:"dllcrt2.o" "!RESP_FILE!" >nul 2>nul && set "HAS_STARTUP=1"
) else (
  if /I "%~1"=="-shared" set "STARTUP_OBJECT=dllcrt2.o"
  if /I "%~nx1"=="crt2.o" set "HAS_STARTUP=1"
  if /I "%~nx1"=="dllcrt2.o" set "HAS_STARTUP=1"
)
shift
goto scan_args
:scanned_args
if "%HAS_STARTUP%"=="1" (
  "%SELF_CONTAINED%\x86_64-w64-mingw32-gcc.exe" -nostartfiles -B"%SELF_CONTAINED%\\" -L"%SELF_CONTAINED_LIB%" %LINK_ARGS%
) else (
  "%SELF_CONTAINED%\x86_64-w64-mingw32-gcc.exe" -nostartfiles "%SELF_CONTAINED_LIB%\%STARTUP_OBJECT%" -B"%SELF_CONTAINED%\\" -L"%SELF_CONTAINED_LIB%" %LINK_ARGS%
)
