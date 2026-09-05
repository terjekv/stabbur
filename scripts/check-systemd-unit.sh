#!/bin/sh
set -eu

unit=${1:-docs/examples/systemd/stabbur-server.service}
command -v systemd-analyze >/dev/null 2>&1 || {
  echo "required program is unavailable: systemd-analyze" >&2
  exit 69
}
test -r "$unit" || {
  echo "systemd unit is not readable: $unit" >&2
  exit 66
}

validation_tmp=$(mktemp -d "${TMPDIR:-/tmp}/stabbur-systemd.XXXXXX")
cleanup() {
  rm -rf "$validation_tmp"
}
trap cleanup EXIT HUP INT TERM

validation_unit="$validation_tmp/stabbur-server.service"
sed \
  -e 's/^User=stabbur$/User=root/' \
  -e 's/^Group=stabbur$/Group=root/' \
  -e 's|^ExecStart=.*$|ExecStart=/bin/true|' \
  "$unit" >"$validation_unit"

systemd-analyze verify "$validation_unit"
