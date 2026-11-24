#!/bin/sh
# Send one notification through the notts CLI. Usage: send.sh APP SEVERITY TITLE [BODY]
# The exit code is notts's: 0 delivered (or deliberately not sent), 1 failed, 2 usage/config, 3 queued.
app=$1 severity=$2 title=$3 body=${4:-}
printf '%s' "$body" | notts send --app "$app" --severity "$severity" --title "$title"
