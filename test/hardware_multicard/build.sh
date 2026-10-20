#!/usr/bin/env bash
set -euo pipefail
probe_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cann_root=${ASCEND_TOOLKIT_HOME:-/usr/local/Ascend/ascend-toolkit/latest}
"${CXX:-g++}" -std=c++17 -O2 -Wall -Wextra -Werror \
  -I"$cann_root/include" "$probe_dir/multicard_probe.cpp" \
  -L"$cann_root/lib64" -Wl,-rpath-link,"$cann_root/lib64" \
  -lascendcl -lopapi -lnnopbase -ldl -pthread -o "$probe_dir/multicard_probe"
