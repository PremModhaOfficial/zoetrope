#!/bin/sh
# fun-ci's fourth stage: lint, build, fast, slow are the fixed names, and only
# this one runs in the background, on the 5-minute budget (trigger_params.rb
# DEFAULT_BUDGETS). It runs the full suite from test.sh, which keeps the suite
# under the name it would have if fun-ci ever lets projects name their stages.
exec "$(dirname "$0")/test.sh"
