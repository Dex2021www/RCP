@echo off
setlocal
cd /d "%~dp0"
rem Release build that keeps the build machine's paths out of the exe.
rem Without the remap, panic locations of dependencies embed paths like
rem C:\Users\NAME\.cargo\registry\... Do not publish the .pdb next to the
rem exe: it stores the linker's full command line.
set "RUSTFLAGS=%RUSTFLAGS% --remap-path-prefix=%USERPROFILE%="
cargo build --release %*
