#!/bin/sh
set -eu

support_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$support_dir/lifecycle-measurement.sh"

fixture_root=$(mktemp -d "${TMPDIR:-/tmp}/sccp-lifecycle-measurement.XXXXXX")
trap 'rm -rf "$fixture_root"' EXIT HUP INT TERM

# File-backed counters survive the command substitutions used by the sampler.
metric() {
	if [ "$1" = threads ]; then
		printf '25\n'
		return
	fi
	count=$(cat "$fixture_root/count")
	count=$((count + 1))
	printf '%s\n' "$count" >"$fixture_root/count"
	case "$scenario" in
	delayed)
		if [ "$count" -le 2 ]; then printf '11\n'; else printf '10\n'; fi
		;;
	unstable) printf '%s\n' "$((10 + count))" ;;
	unavailable) return 1 ;;
	esac
}

sleep() { :; }

scenario=delayed
printf '0\n' >"$fixture_root/count"
settle_lifecycle_metrics delayed
[ "$(cat "$fixture_root/count")" -eq 7 ]

scenario=unstable
printf '0\n' >"$fixture_root/count"
if settle_lifecycle_metrics unstable 2>"$fixture_root/error"; then
	printf 'unstable resource counts unexpectedly passed\n' >&2
	exit 1
fi
[ "$(cat "$fixture_root/count")" -eq 40 ]
grep -q 'did not settle after unstable' "$fixture_root/error"

scenario=unavailable
if settle_lifecycle_metrics unavailable; then
	printf 'unavailable process metrics unexpectedly passed\n' >&2
	exit 1
fi

cli() {
	[ "$1" = 'malloc trim' ] || return 2
	printf '%s\n' "$cli_result"
	return "$cli_status"
}

cli_status=0
for cli_result in 'Returned some memory to the OS.' 'No memory returned to the OS.'; do
	[ "$(reclaim_lifecycle_free_heap)" = "$cli_result" ]
done
cli_result='No such command malloc trim'
if reclaim_lifecycle_free_heap 2>"$fixture_root/error"; then
	printf 'unsupported allocator diagnostic unexpectedly passed\n' >&2
	exit 1
fi
grep -q 'requires Asterisk malloc trim support' "$fixture_root/error"
cli_status=1
cli_result='Returned some memory to the OS.'
if reclaim_lifecycle_free_heap; then
	printf 'failed allocator diagnostic unexpectedly passed\n' >&2
	exit 1
fi

printf 'Lifecycle measurement contracts passed\n'
