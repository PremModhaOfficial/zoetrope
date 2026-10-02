#!/bin/sh
# Fast suite: the quick path through the Command Code provider -- its unit
# tests and the S-001..S-017 scenario tests. Runs after lint and build pass,
# beside the full workspace suite (test.sh, wired in as the slow stage).
. "$(dirname "$0")/env"

exec $TASKSET cargo test -p zoetrope provider::commandcode
