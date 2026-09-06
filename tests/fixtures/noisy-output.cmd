@echo off
rem Fixture for `zirv ctx run --compact` (issue #326): the Windows sibling of
rem noisy-output.sh, printing byte-for-byte the same lines. Data only.
for /L %%i in (1,1,120) do @echo filler line %%i
echo error[E0308]: mismatched types
echo   --^> src/lib.rs:42:9
echo warning: unused variable: `x`
echo failures:
echo.
echo     module::tests::alpha
echo     module::tests::beta
echo.
echo test result: FAILED. 3 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out
exit /b 3
