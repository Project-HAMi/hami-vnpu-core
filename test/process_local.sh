#!/usr/bin/env bash
set -euo pipefail
case "$(uname -s)" in
    Linux|Darwin) ;;
    *) echo 'These process/fork checks require Linux or macOS.' >&2; exit 1 ;;
esac
repo_dir="$(cd "$(dirname "$0")/.." && pwd)"
test_dir="$(mktemp -d "${TMPDIR:-/tmp}/hami-process-local.XXXXXX")"
trap 'rm -rf "$test_dir"' EXIT
rustc --edition=2024 --test "$repo_dir/test/process_local.rs" -o "$test_dir/process_local"
"$test_dir/process_local" --test-threads=1
