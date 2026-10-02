#!/bin/sh
# Lint: the source alone, beside build.sh (fun-ci runs those two at once, so
# neither may write what the other reads). clippy gets a target dir of its own
# (see env) rather than the build's.
. "$(dirname "$0")/env"

$TASKSET cargo fmt --check || exit $?
exec $TASKSET cargo clippy --all-targets -- -D warnings
