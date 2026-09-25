@echo off
rem Double-click to start AudiobookAI (it is built first when the code changed).
cd /d "%~dp0"
set "PYTHON=python"
where py >nul 2>nul && set "PYTHON=py -3"
%PYTHON% scripts\launch\start_audiobookai.py
if errorlevel 1 pause
