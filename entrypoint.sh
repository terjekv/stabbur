#!/bin/sh
set -eu

if [ "$#" -eq 0 ]; then
  set -- all
fi

exec /usr/local/bin/stabbur-server "$@"
