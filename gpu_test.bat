@echo off
cd /d "%~dp0"

rem Keep the whole test within 6 CPU cores. The math library behind numpy defaults to one
rem thread per core, so it is capped too. To override, set these before running, for example:
rem     set OPENBLAS_NUM_THREADS=1   (and OMP_NUM_THREADS, MKL_NUM_THREADS)
if not defined OPENBLAS_NUM_THREADS set OPENBLAS_NUM_THREADS=1
if not defined OMP_NUM_THREADS set OMP_NUM_THREADS=1
if not defined MKL_NUM_THREADS set MKL_NUM_THREADS=1

if exist ".venv\Scripts\python.exe" (
    ".venv\Scripts\python.exe" gpu_pow_test.py --cpu-threads 1 %*
) else (
    python gpu_pow_test.py --cpu-threads 1 %*
)

echo.
pause
