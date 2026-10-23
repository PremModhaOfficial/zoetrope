#!/bin/sh
# Build: runs beside lint.sh. --tests compiles the test code too, so the suites
# run on what this leaves (fast.sh has only a 10s budget and must not compile
# anything). It is --tests, not --all-targets, because the suites only run test
# targets: --all-targets would also compile the criterion benches, which no
# suite runs, and those do not fit the 30s budget.
. "$(dirname "$0")/env"

exec $TASKSET cargo build --workspace --tests
