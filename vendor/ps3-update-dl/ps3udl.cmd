@echo off
setlocal
set "PYTHONPATH=%~dp0"
"%~dp0python311\python.exe" -c "import sys; from ps3_update_dl import main; main()" %*
exit /b %ERRORLEVEL%
