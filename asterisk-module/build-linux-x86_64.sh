#!/bin/sh
set -eu

usage() {
	printf 'Usage: %s {22|latest} [output-directory]\n' "$0" >&2
}

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

case "${1:-}" in
22)
	asterisk_feature=asterisk-22
	;;
latest)
	asterisk_feature=asterisk-latest
	;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		usage
		exit 2
		;;
esac
asterisk_ref=$("$script_dir/ci/resolve-asterisk-ref.sh" "$1")

if ! command -v docker >/dev/null 2>&1; then
	printf 'error: Docker is required; install and start Docker Desktop first.\n' >&2
	exit 1
fi
if ! docker info >/dev/null 2>&1; then
	printf 'error: the Docker daemon is not running; start Docker Desktop first.\n' >&2
	exit 1
fi
if ! docker buildx version >/dev/null 2>&1; then
	printf 'error: Docker Buildx is required; update Docker Desktop first.\n' >&2
	exit 1
fi

repo_dir=$(dirname -- "$script_dir")
output_dir=${2:-"$repo_dir/dist"}
mkdir -p "$output_dir"
module_version=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' "$script_dir/Cargo.toml" | head -n 1)
if [ -z "$module_version" ]; then
	printf 'error: unable to read the asterisk-module package version.\n' >&2
	exit 1
fi

docker buildx build \
	--pull \
	--platform linux/amd64 \
	--progress plain \
	--build-arg "ASTERISK_REF=$asterisk_ref" \
	--build-arg "ASTERISK_FEATURE=$asterisk_feature" \
	--build-arg "MODULE_VERSION=v$module_version" \
	--target artifact \
	--output "type=local,dest=$output_dir" \
	--file "$script_dir/ci/Dockerfile" \
	"$repo_dir"

artifact="$output_dir/chan_sccp2-asterisk-linux-x86_64-v${module_version}.so"
printf 'Built %s\n' "$artifact"
