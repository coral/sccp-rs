#!/bin/sh
# The caller supplies metric() and cli() for the isolated Asterisk process.

settle_lifecycle_metrics() {
	checkpoint=$1
	previous_sample=
	stable_samples=0
	measurement_attempt=0
	while [ "$measurement_attempt" -lt 40 ]; do
		measurement_fds=$(metric fd) || return 1
		measurement_threads=$(metric threads) || return 1
		current_sample="$measurement_fds $measurement_threads"
		if [ "$current_sample" = "$previous_sample" ]; then
			stable_samples=$((stable_samples + 1))
		else
			stable_samples=0
		fi
		if [ "$stable_samples" -ge 4 ]; then
			return 0
		fi
		previous_sample=$current_sample
		measurement_attempt=$((measurement_attempt + 1))
		sleep 0.05
	done
	printf 'fd/thread counts did not settle after %s\n' "$checkpoint" >&2
	return 1
}

reclaim_lifecycle_free_heap() {
	# Only free allocator pages are returned. Live leaked allocations remain
	# resident and subject to the unchanged RSS growth limit.
	trim_result=$(cli 'malloc trim') || return 1
	case "$trim_result" in
	'Returned some memory to the OS.' | 'No memory returned to the OS.')
		printf '%s\n' "$trim_result"
		;;
	*)
		printf 'native lifecycle memory gate requires Asterisk malloc trim support: %s\n' \
			"$trim_result" >&2
		return 1
		;;
	esac
}
