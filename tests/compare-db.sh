#!/bin/bash
# Compare the database written by `build` against plocate-build's, on one
# bcachefs mount. Needs root: the dirent ioctl does.
set -euo pipefail

if [ $# -lt 2 ]; then
	echo "usage: $0 <mount> <workdir> [prune-path...]" >&2
	exit 2
fi

mount=$1
workdir=$2
shift 2
prunes=()
for p in "$@"; do
	prunes+=(--prune "$p")
done
here=$(cd "$(dirname "$0")/.." && pwd)
bin=$here/target/release/bcachefs-updatedb
build=$(command -v plocate-build || echo /usr/sbin/plocate-build)
group=$(id -gn)
failed=0

mkdir -p "$workdir"
list=$workdir/list.txt
a=$workdir/a.db
b=$workdir/b.db

step() {
	local name=$1
	shift
	if "$@"; then
		echo "PASS $name"
	else
		echo "FAIL $name"
		failed=1
	fi
}

echo "== paths -> $list"
"$bin" paths "$mount" "${prunes[@]}" >"$list"
wc -l "$list"

echo "== $build -p $list $a"
"$build" -p "$list" "$a"

echo "== build --output $b"
"$bin" build "$mount" --output "$b" --group "$group" "${prunes[@]}"

echo "== posting lists"
"$bin" dbinfo --posting-lists "$a" >"$workdir/a.pl"
"$bin" dbinfo --posting-lists "$b" >"$workdir/b.pl"
step "posting lists identical" diff -q "$workdir/a.pl" "$workdir/b.pl"

compare_search() {
	diff <(plocate -d "$a" "$@" | sort) <(plocate -d "$b" "$@" | sort)
}

for needle in a ab bin .rs /usr/lib README; do
	step "search $needle" compare_search "$needle"
done
step "search -r ." compare_search -r .

echo "== second build, over its own database"
size_before=$(stat -c %s "$b")
"$bin" build "$mount" --output "$b" --group "$group" "${prunes[@]}" 2>"$workdir/second.log"
cat "$workdir/second.log"
size_after=$(stat -c %s "$b")
step "reused the stored dictionary" grep -q "reusing the .* next dictionary" "$workdir/second.log"
step "database shrank ($size_before -> $size_after)" test "$size_after" -lt "$size_before"

exit "$failed"
