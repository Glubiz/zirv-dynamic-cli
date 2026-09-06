#!/bin/sh
# Fixture for `zirv ctx run --compact` (issue #326): a lot of noise wrapped
# around the handful of lines a compact summary must never lose. Data only --
# see tests/fixtures' own convention. The `.cmd` sibling prints byte-for-byte
# the same lines on Windows.
i=1
while [ "$i" -le 120 ]; do
  echo "filler line $i"
  i=$((i + 1))
done
echo 'error[E0308]: mismatched types'
echo '  --> src/lib.rs:42:9'
echo 'warning: unused variable: `x`'
echo 'failures:'
echo ''
echo '    module::tests::alpha'
echo '    module::tests::beta'
echo ''
echo 'test result: FAILED. 3 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out'
exit 3
