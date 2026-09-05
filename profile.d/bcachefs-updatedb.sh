# A non-existent path in LOCATE_PATH makes every plocate run exit 1, so an
# unmatched glob must never reach it.
for _bcachefs_db in /var/lib/plocate/bcachefs-*.db; do
	[ -e "$_bcachefs_db" ] || continue
	LOCATE_PATH="${LOCATE_PATH:+$LOCATE_PATH:}$_bcachefs_db"
	export LOCATE_PATH
done
unset _bcachefs_db
