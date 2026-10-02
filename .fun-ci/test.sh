#!/bin/sh
# The full workspace suite: the existing tests and the Command Code provider's
# scenario tests (S-001..S-017) alike. fun-ci 2.1.1's four stage names are
# fixed -- lint, build, fast, slow -- and only the last runs in the background,
# on the 5-minute budget a full suite needs, so this script is wired in as
# slow.sh. (The ~/fun-ci-src fork's .fun-ci/stages would let the stage itself
# be named `test`; until that is installed there is no such stage.)
. "$(dirname "$0")/env"

exec $TASKSET cargo test --workspace
