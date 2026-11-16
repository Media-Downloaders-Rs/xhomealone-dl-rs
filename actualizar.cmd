@echo off
cd /d "%~dp0"
cargo build --release --locked
if errorlevel 1 pause
