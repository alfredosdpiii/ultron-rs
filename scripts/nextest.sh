#!/bin/sh
# `cargo nextest run` with a hard CPU cap, so a test run (and the build before it) never starves the desktop: the
# whole process tree runs in a transient systemd scope limited to ULTRON_TEST_CPUS cores' worth of time (default 6),
# at nice 19. Arguments go to `cargo nextest run`:
#
#   scripts/nextest.sh --workspace --locked
#   scripts/nextest.sh -p review eval::
#
# Without a systemd user session (CI, a container) it runs under nice alone.
set -eu
cpus="${ULTRON_TEST_CPUS:-6}"
case "$cpus" in
'' | *[!0-9]*)
	echo "ULTRON_TEST_CPUS must be a whole number of cores, not '$cpus'" >&2
	exit 2
	;;
esac
if command -v systemd-run >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; then
	exec systemd-run --user --scope --quiet --collect -p "CPUQuota=${cpus}00%" -- nice -n 19 cargo nextest run "$@"
fi
exec nice -n 19 cargo nextest run "$@"
