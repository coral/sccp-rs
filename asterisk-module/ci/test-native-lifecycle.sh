#!/bin/sh
set -eu

module_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$module_dir/test-support/asterisk-sandbox.sh"
identity_verifier="$module_dir/../verify-loaded-module.sh"

WARMUP_CYCLES=${SCCP_LIFECYCLE_WARMUP_CYCLES:-4}
BATCH_CYCLES=${SCCP_LIFECYCLE_BATCH_CYCLES:-12}
RSS_TOLERANCE_KB=${SCCP_LIFECYCLE_RSS_TOLERANCE_KB:-1024}
SCCP_PORT=${SCCP_LIFECYCLE_PORT:-24999}
LIVE_BRIDGES=${SCCP_LIVE_BRIDGES:-0}

module_path=${1:-}
native_module_dir=${ASTERISK_MODULE_DIR:-/usr/lib/asterisk/modules}
native_data_dir=${ASTERISK_DATA_DIR:-/var/lib/asterisk}
asterisk_bin=${ASTERISK_BIN:-asterisk}

for bounded_value in "$WARMUP_CYCLES" "$BATCH_CYCLES" "$RSS_TOLERANCE_KB" "$SCCP_PORT"; do
	case "$bounded_value" in
	'' | *[!0-9]*)
		printf 'lifecycle bounds and port must be unsigned decimal integers\n' >&2
		exit 2
		;;
	esac
done
if [ "$LIVE_BRIDGES" != 0 ] && [ "$LIVE_BRIDGES" != 1 ]; then
	printf 'SCCP_LIVE_BRIDGES must be 0 or 1\n' >&2
	exit 2
fi
if [ "$WARMUP_CYCLES" -lt 1 ] || [ "$WARMUP_CYCLES" -gt 20 ] \
	|| [ "$BATCH_CYCLES" -lt 1 ] || [ "$BATCH_CYCLES" -gt 50 ] \
	|| [ "$RSS_TOLERANCE_KB" -gt 16384 ] \
	|| [ "$SCCP_PORT" -lt 1 ] || [ "$SCCP_PORT" -gt 65535 ]; then
	printf 'lifecycle bounds or port are outside the permitted range\n' >&2
	exit 2
fi

if [ -z "$module_path" ] || [ ! -f "$module_path" ]; then
	printf 'usage: %s /path/to/libchan_sccp2.so\n' "$0" >&2
	exit 2
fi
if [ ! -d "$native_module_dir" ]; then
	printf 'Asterisk module directory does not exist: %s\n' "$native_module_dir" >&2
	exit 2
fi
if [ ! -d "$native_data_dir/documentation" ]; then
	printf 'Asterisk documentation directory does not exist: %s/documentation\n' \
		"$native_data_dir" >&2
	exit 2
fi
if ! command -v "$asterisk_bin" >/dev/null 2>&1; then
	printf 'Asterisk executable is unavailable: %s\n' "$asterisk_bin" >&2
	exit 2
fi

test_root=
asterisk_pid=
diagnostics=
asterisk_log=
cli_log=

finish() {
	status=$1
	trap - EXIT HUP INT TERM
	sccp_sandbox_stop
	if [ "$status" -ne 0 ]; then
		sccp_sandbox_diagnostics "$diagnostics" "$cli_log" "$asterisk_log"
	fi
	sccp_sandbox_cleanup
	exit "$status"
}
trap 'finish $?' EXIT
trap 'exit 130' HUP INT TERM

sccp_sandbox_create chan-sccp2-lifecycle "$module_path" \
	"$native_module_dir" "$native_data_dir"
test_root=$SCCP_SANDBOX_ROOT
asterisk_pid=$SCCP_SANDBOX_PID
diagnostics="$test_root/lifecycle.tsv"
asterisk_log="$test_root/asterisk.log"
cli_log="$test_root/cli.log"

mkdir -p "$test_root/var/lib/moh"

if [ -d /etc/asterisk ]; then
	cp -R /etc/asterisk/. "$test_root/etc/"
fi
sccp_sandbox_write_config "$native_data_dir"

cat >"$test_root/etc/modules.conf" <<'EOF'
[modules]
autoload = no
noload = chan_sccp2.so
EOF

if [ "$LIVE_BRIDGES" -eq 1 ]; then
	for required_module in \
		res_timing_timerfd.so bridge_simple.so bridge_softmix.so codec_ulaw.so \
		format_pcm.so res_musiconhold.so app_confbridge.so; do
		if [ ! -f "$test_root/modules/$required_module" ]; then
			printf 'live bridge dependency is unavailable: %s\n' "$required_module" >&2
			exit 2
		fi
		printf 'load = %s\n' "$required_module" >>"$test_root/etc/modules.conf"
	done
	dd if=/dev/zero of="$test_root/var/lib/moh/silence.ulaw" \
		bs=160 count=50 2>/dev/null
	cat >"$test_root/etc/musiconhold.conf" <<EOF
[default]
mode = files
directory = $test_root/var/lib/moh
EOF
	cat >"$test_root/etc/confbridge.conf" <<'EOF'
[default_user]
type = user
music_on_hold_when_empty = no

[default_bridge]
type = bridge
EOF
fi

cat >"$test_root/etc/sccp.conf" <<EOF
[general]
bind = 127.0.0.1:$SCCP_PORT
advertised_address = 127.0.0.1
disallow = all
allow = ulaw

[SEP001122334455]
type = device
line = 1001

[1001]
type = line
label = Lifecycle
context = default
EOF

sccp_sandbox_start "$asterisk_bin" "$test_root/etc/sccp.conf" "$asterisk_log"
asterisk_pid=$SCCP_SANDBOX_PID

cli() {
	sccp_sandbox_cli "$asterisk_bin" "$1"
}

capture_cli() {
	label=$1
	command=$2
	printf '\n[%s] %s\n' "$label" "$command" >>"$cli_log"
	if ! cli "$command" >>"$cli_log" 2>&1; then
		printf 'Asterisk CLI command failed during lifecycle cycle %s: %s\n' \
			"$label" "$command" >&2
		return 1
	fi
}

if ! sccp_sandbox_wait_ready "$asterisk_bin"; then
	if [ "$SCCP_SANDBOX_READY_FAILURE" = exited ]; then
		printf 'Asterisk exited during startup (status %s)\n' \
			"$SCCP_SANDBOX_EXIT_STATUS" >&2
	else
		printf 'Asterisk did not become ready within 10 seconds\n' >&2
	fi
	exit 1
fi

count_running_module_rows() {
	awk '$1 == "chan_sccp2.so" && $(NF - 2) != "Not" && $(NF - 1) == "Running" { count += 1 } \
		END { print count + 0 }'
}

running_module_count() {
	cli 'module show like chan_sccp2.so' | count_running_module_rows
}

channel_driver_count() {
	cli 'core show channeltypes' \
		| awk '$1 == "SCCP" { count += 1 } END { print count + 0 }'
}

assert_loaded_module_identity() {
	cycle_label=$1
	if ! "$identity_verifier" \
		"$test_root/modules/chan_sccp2.so" "$asterisk_pid" >>"$cli_log" 2>&1; then
		printf 'module mapping did not match the candidate binary during lifecycle cycle %s\n' \
			"$cycle_label" >&2
		exit 1
	fi
}

module_status_fixture='chan_sccp2.so Rust SCCP Channel Driver 0 Running extended
chan_sccp2.so Rust SCCP Channel Driver 0 Not Running extended'
if [ "$(printf '%s\n' "$module_status_fixture" | count_running_module_rows)" -ne 1 ]; then
	printf 'module status parser did not distinguish Running from Not Running\n' >&2
	exit 1
fi

assert_alive() {
	if ! kill -0 "$asterisk_pid" 2>/dev/null; then
		printf 'Asterisk exited during lifecycle cycle %s\n' "$1" >&2
		exit 1
	fi
}

run_cycle() {
	cycle_label=$1
	assert_alive "$cycle_label"
	record_metrics "$cycle_label-start"
	capture_cli "$cycle_label-load" 'module load chan_sccp2.so'
	capture_cli "$cycle_label-loaded-module" 'module show like chan_sccp2.so'
	capture_cli "$cycle_label-loaded-channeltypes" 'core show channeltypes'
	if [ "$(running_module_count)" -ne 1 ] || [ "$(channel_driver_count)" -ne 1 ]; then
		printf 'module was not running during lifecycle cycle %s\n' "$cycle_label" >&2
		exit 1
	fi
	assert_loaded_module_identity "$cycle_label"
	if [ "$LIVE_BRIDGES" -eq 1 ]; then
		bridge_result=$(cli 'sccp test bridges')
		printf '\n[%s-bridges] sccp test bridges\n%s\n' \
			"$cycle_label" "$bridge_result" >>"$cli_log"
		case "$bridge_result" in
		*'CONF-020 PASS scenarios=11'*) ;;
		*)
			printf 'live bridge harness failed during lifecycle cycle %s\n' \
				"$cycle_label" >&2
			exit 1
			;;
		esac
	fi
	capture_cli "$cycle_label-unload" 'module unload chan_sccp2.so'
	capture_cli "$cycle_label-unloaded-module" 'module show like chan_sccp2.so'
	capture_cli "$cycle_label-unloaded-channeltypes" 'core show channeltypes'
	if [ "$(running_module_count)" -ne 0 ] || [ "$(channel_driver_count)" -ne 0 ]; then
		printf 'module remained running after lifecycle cycle %s\n' "$cycle_label" >&2
		exit 1
	fi
	assert_alive "$cycle_label"
	record_metrics "$cycle_label-end"
}

metric() {
	case "$1" in
	fd)
		find "/proc/$asterisk_pid/fd" -mindepth 1 -maxdepth 1 -print | wc -l | awk '{ print $1 }'
		;;
	threads)
		find "/proc/$asterisk_pid/task" -mindepth 1 -maxdepth 1 -print | wc -l | awk '{ print $1 }'
		;;
	rss)
		awk '/^VmRSS:/ { print $2 }' "/proc/$asterisk_pid/status"
		;;
	*)
		return 2
		;;
	esac
}

record_metrics() {
	label=$1
	fd_count=$(metric fd)
	thread_count=$(metric threads)
	rss_kb=$(metric rss)
	printf '%s\t%s\t%s\t%s\n' "$label" "$fd_count" "$thread_count" "$rss_kb" >>"$diagnostics"
}

wait_for_metric_at_most() {
	metric_name=$1
	maximum=$2
	checkpoint=$3
	# Remote CLI workers retire asynchronously. Give them a bounded cleanup
	# window while still treating any persistent growth as a leak.
	attempt=0
	actual=$(metric "$metric_name")
	while [ "$actual" -gt "$maximum" ] && [ "$attempt" -lt 40 ]; do
		attempt=$((attempt + 1))
		sleep 0.05
		actual=$(metric "$metric_name")
	done
	if [ "$actual" -gt "$maximum" ]; then
		printf '%s count remained above the post-warmup baseline after %s: %s > %s\n' \
			"$metric_name" "$checkpoint" "$actual" "$maximum" >&2
		exit 1
	fi
}

printf 'step\tfds\tthreads\trss_kb\n' >"$diagnostics"
cycle=1
while [ "$cycle" -le "$WARMUP_CYCLES" ]; do
	run_cycle "warmup-$cycle"
	cycle=$((cycle + 1))
done
record_metrics warmup
baseline_fds=$(metric fd)
baseline_threads=$(metric threads)

batch=1
while [ "$batch" -le 3 ]; do
	cycle=1
	while [ "$cycle" -le "$BATCH_CYCLES" ]; do
		run_cycle "batch-$batch-$cycle"
		cycle=$((cycle + 1))
	done
	record_metrics "batch-$batch"
	wait_for_metric_at_most fd "$baseline_fds" "batch $batch"
	wait_for_metric_at_most threads "$baseline_threads" "batch $batch"
	if [ "$batch" -eq 2 ]; then
		second_batch_rss=$(metric rss)
	fi
	batch=$((batch + 1))
done

final_rss=$(metric rss)
maximum_final_rss=$((second_batch_rss + RSS_TOLERANCE_KB))
if [ "$final_rss" -gt "$maximum_final_rss" ]; then
	printf 'RSS grew from %s KiB after batch 2 to %s KiB after batch 3 (limit +%s KiB)\n' \
		"$second_batch_rss" "$final_rss" "$RSS_TOLERANCE_KB" >&2
	exit 1
fi

cli 'core stop now' >/dev/null
wait "$asterisk_pid"
asterisk_pid=
SCCP_SANDBOX_PID=
printf 'Native lifecycle gate passed: %s warmup + %s measured load/unload cycles\n' \
	"$WARMUP_CYCLES" "$((BATCH_CYCLES * 3))"
if [ "$LIVE_BRIDGES" -eq 1 ]; then
	printf 'Native bridge gate passed across every lifecycle cycle\n'
fi
