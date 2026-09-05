#!/bin/sh
set -eu

case "${1:-}" in
22)
	printf '%s\n' 22.7.0
	;;
latest)
	ref=$(git ls-remote https://github.com/asterisk/asterisk.git refs/heads/master \
		| awk 'NR == 1 { print $1 }')
	if ! printf '%s\n' "$ref" | grep -Eq '^[0-9a-f]{40}$'; then
		printf 'error: unable to resolve upstream Asterisk master\n' >&2
		exit 1
	fi
	printf '%s\n' "$ref"
	;;
-h | --help)
	printf 'Usage: %s {22|latest}\n' "$0"
	;;
*)
	printf 'Usage: %s {22|latest}\n' "$0" >&2
	exit 2
	;;
esac
