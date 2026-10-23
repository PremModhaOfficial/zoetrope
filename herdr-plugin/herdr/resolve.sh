#!/usr/bin/env bash
# Print "<agent> <session-id>" for the pane the plugin was invoked from, or
# exit non-zero with the reason on stderr.
#
# The pane id comes from `focused_pane_id` in HERDR_PLUGIN_CONTEXT_JSON, never
# from HERDR_PANE_ID: in a pane command HERDR_PANE_ID is the plugin's own new
# pane, and asking Herdr about that one returns a pane with no agent. The
# context names the pane that was focused when the plugin was invoked, and it
# is the same field for an action and for a pane command.
#
# Herdr's Claude Code and Codex integrations report the native session id from
# a SessionStart hook (`pane.report_agent_session`), so `pane.get` carries
# `agent_session: {source, agent, kind: "id", value}` for both. That pair is
# all zoe needs. Nothing is guessed from the working directory — with one
# exception: a cmd pane on Herdr 0.9.x, whose session refs are ignored, falls
# back to the pane's directory (printed with a trailing "dir" mode). When
# Herdr has no id and no fallback applies, the caller says so.
set -euo pipefail

herdr="${HERDR_BIN_PATH:-herdr}"
ctx="${HERDR_PLUGIN_CONTEXT_JSON:-{\}}"

command -v jq >/dev/null 2>&1 || { echo "jq is not on PATH, and the plugin reads Herdr's JSON with it" >&2; exit 1; }

pane_id=$(printf '%s' "$ctx" | jq -r '.focused_pane_id // empty')
[ -n "$pane_id" ] || { echo "no focused pane in the invocation context" >&2; exit 1; }

resp=$("$herdr" pane get "$pane_id" 2>&1) || { echo "herdr pane get failed: $resp" >&2; exit 1; }
err=$(printf '%s' "$resp" | jq -r '.error.message // empty' 2>/dev/null || true)
[ -z "$err" ] || { echo "herdr: $err" >&2; exit 1; }

# `pane.get` answers `{"result": {"pane": {...}, "type": "pane_info"}}`: the
# record is under `.result.pane`, not `.result` itself.
pane=$(printf '%s' "$resp" | jq '.result.pane // .result')
agent=$(printf '%s' "$pane" | jq -r '.agent_session.agent // .agent // empty')
kind=$(printf  '%s' "$pane" | jq -r '.agent_session.kind  // empty')
value=$(printf '%s' "$pane" | jq -r '.agent_session.value // empty')

case "$agent" in
  claude | codex | opencode | jcode) provider=$agent ;;
  cmd | commandcode) provider=commandcode ;;
  "") echo "pane $pane_id has no agent: focus a Claude Code, Codex, Command Code, opencode or jcode pane" >&2; exit 1 ;;
  *)  echo "agent '$agent' in pane $pane_id is not one zoe reads (Claude Code, Codex, Command Code, opencode and jcode)" >&2; exit 1 ;;
esac

case "$kind" in
  id) ;;
  "")
     # Command Code and jcode: Herdr has no session id for
     # them (cmd: 0.9.x ignores session refs; jcode: no
     # integration exists at all), so fall back to the pane's
     # directory — zoe follows the newest session of that
     # project. The "dir" mode tells open.sh to open the
     # directory instead of an id.
     if [ "$provider" = commandcode ] || [ "$provider" = jcode ]; then
       cwd=$(printf '%s' "$pane" | jq -r '.foreground_cwd // .cwd // empty')
       if [ -n "$cwd" ]; then
         printf '%s %s dir\n' "$provider" "$cwd"
         exit 0
       fi
     fi
     if [ "$provider" = jcode ]; then
       cat >&2 <<MSG
Herdr has no session id for this jcode pane, and no directory to
fall back on either.

  Herdr ships no jcode integration, so the session id never reaches
  it. Open the session by file or id instead:

    zoe --provider jcode <session.json>    (or: zoe <session-id>)
MSG
       exit 1
     fi
     cat >&2 <<MSG
Herdr has no session id for this $agent pane.

  The id comes from the agent's SessionStart hook, which fires only when a
  session begins. So: install the integration if it is missing, then start the
  agent in that pane again. A session that was already running when the
  integration was installed never reports one.

    herdr integration install $agent    (herdr integration status lists them)
    for a cmd pane: restart cmd there, so its channel plugin reports the id
MSG
     exit 1 ;;
  *)  echo "Herdr reports a $kind for this $agent pane, and the plugin expects an id" >&2; exit 1 ;;
esac

printf '%s %s\n' "$provider" "$value"
