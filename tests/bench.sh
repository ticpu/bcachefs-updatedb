#!/bin/bash
# Wall time and peak RSS of each README performance row, output to /dev/null.
# usage: bench.sh <mount> <live-tree-prune-path|-> <workdir>
set -euo pipefail
mount=$1
prune=$2
work=$3
here=$(cd "$(dirname "$0")/.." && pwd)
bin=${BIN:-$here/target/release/bcachefs-updatedb}
mkdir -p "$work"
group=$(stat -c %G "$work")
t() {
	local tag=$1
	shift
	/usr/bin/time -f "$tag: %es wall, %M kB peak" "$@" 2>&1 >/dev/null | grep -v "^  \|^[A-Z][a-z ]*:\|^database group\|dictionary\|^[0-9]* subvolumes\|^pass 1\|^resolved\|^emitted\|files, .* trigrams"
}
t "stats" "$bin" stats "$mount"
if [ "$prune" != - ]; then
	t "paths live" "$bin" paths "$mount" --no-conf --prune "$prune"
	rm -f "$work/live.db"
	t "build live no dict" "$bin" build "$mount" --no-conf --prune "$prune" --output "$work/live.db" --group "$group"
	t "build live with dict" "$bin" build "$mount" --no-conf --prune "$prune" --output "$work/live.db" --group "$group"
	ls -l "$work/live.db"
fi
t "paths all" "$bin" paths "$mount" --no-conf
rm -f "$work/all.db"
t "build all no dict" "$bin" build "$mount" --no-conf --output "$work/all.db" --group "$group"
ls -l "$work/all.db"
