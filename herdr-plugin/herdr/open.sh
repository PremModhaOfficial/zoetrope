#!/usr/bin/env bash
# Pane command: resolve the session of the pane this was opened from, and run
# `zoe` on it. Every failure is printed here and held until you press enter,
# since this terminal is what the user is looking at.
set -euo pipefail

root="${HERDR_PLUGIN_ROOT:-.}"

die() {
  printf '\n  %s\n\n' "$*" >&2
  read -r -p '  press enter to close ' _ || true
  exit 1
}

command -v zoe >/dev/null 2>&1 || die "zoe is not on PATH (brew install furkankly/tap/zoetrope, or cargo install zoetrope)"

if ! target=$(bash "$root/herdr/resolve.sh" 2>&1); then
  die "$target"
fi

# resolve.sh prints "<provider> <target> [mode]": mode "dir" means Herdr had no
# session id (a cmd pane on Herdr 0.9.x) and <target> is the pane's directory —
# follow the newest session of that project instead of an id. Otherwise ride
# the live edge; space and the scrubber go back.
# shellcheck disable=SC2086
set -- $target
provider=$1
value=$2
mode=${3:-}
if [ "$mode" = dir ]; then
  zoe --provider "$provider" "$value" \
    || die "zoe exited with status $? (zoe --provider $provider $value)"
  exit 0
fi
zoe --provider "$provider" --follow "$value" \
  || die "zoe exited with status $? (zoe --provider $provider --follow $value)"
