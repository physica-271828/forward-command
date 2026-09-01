@echo off
rem Forward Command - double-click mod installer.
rem Runs install-mod.ps1 (same folder) with the execution policy bypassed,
rem then pauses so the result stays on screen. No command line needed.
rem NOTE: this file is GBK-encoded (cmd parses .bat as ANSI) - edit it only
rem with byte-safe tools, never a UTF-8 editor.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install-mod.ps1"
if errorlevel 1 (
    echo.
    echo   安装失败 - 请阅读上方提示信息。
) else (
    echo.
    echo   完成 - mod 已安装，请在 HOI4 启动器中启用 Forward Command。
    echo.
    echo   【重要】若你已在 Steam 创意工坊订阅本 mod，请勿在启动器中
    echo   同时启用两个版本！同名 mod 同时加载会导致游戏内冲突。
    echo   已订阅的话请跳过本脚本，mod 更新由启动器自动完成。
)
echo.
pause
