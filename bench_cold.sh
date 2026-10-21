#!/bin/bash
# キャッシュを冷やした状態でのディレクトリスキャン計測
# usage: sudo ./bench_cold.sh <dir>...
#   REPS=3 MODES="gnu-find orig seq seq-ino par:2 par:4 par:8 par:16 par:32" で上書き可
# drop_caches はサーバー全体のキャッシュを捨てるので、利用者のいない時間帯に実行すること
set -u
if [ $# -eq 0 ]; then echo "usage: sudo $0 <dir>..." >&2; exit 1; fi
if [ "$(id -u)" -ne 0 ]; then echo "drop_caches のため root で実行してください" >&2; exit 1; fi
HERE=$(cd "$(dirname "$0")" && pwd)
B=$HERE/target/release/rust_directory_scan_test2
if [ ! -x "$B" ]; then echo "先に cargo build --release してください" >&2; exit 1; fi
REPS=${REPS:-3}
MODES=${MODES:-"gnu-find orig seq seq-ino par:2 par:4 par:8 par:16 par:32"}
OUT=$HERE/cold_result_$(date +%Y%m%d_%H%M%S).txt
TIMEFORMAT='  real=%R user=%U sys=%S'
for D in "$@"; do
  for r in $(seq "$REPS"); do
    for m in $MODES; do
      sync; echo 3 > /proc/sys/vm/drop_caches
      { echo "dir=$D rep=$r"
        if [ "$m" = gnu-find ]; then
          echo "mode=gnu-find"; time /usr/bin/find "$D" -name '.*' -prune -o -type d -print > /dev/null
        else
          time "$B" "$m" "$D"
        fi
      } 2>&1 | tee -a "$OUT"
    done
  done
done
[ -n "${SUDO_UID:-}" ] && chown "$SUDO_UID:$SUDO_GID" "$OUT"
echo "result: $OUT"
