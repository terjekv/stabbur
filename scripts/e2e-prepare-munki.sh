#!/bin/sh
set -eu

if [ "$(uname -s)" != Darwin ] || [ "${STABBUR_DISPOSABLE_MACOS:-}" != 1 ]; then
  echo 'Munki installation requires macOS and STABBUR_DISPOSABLE_MACOS=1' >&2
  exit 77
fi
# Installing client launchd jobs could affect a developer machine. This script is for a fresh VM.
if [ -e /Library/Preferences/ManagedInstalls.plist ] || [ -e /usr/local/munki/managedsoftwareupdate ]; then
  echo 'refusing to replace an existing Munki installation or configuration' >&2
  exit 77
fi
umask 077
munki_tmp=$(mktemp -d "${TMPDIR:-/tmp}/stabbur-munki-prepare.XXXXXX")
trap 'rm -rf "$munki_tmp"' EXIT HUP INT TERM
fixture=tests/fixtures/munki/munkitools-7.2.0.json
curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
  --connect-timeout 15 --max-time 300 \
  --output "$munki_tmp/munki.pkg" "$(jq -er .url "$fixture")"
test "$(stat -f '%z' "$munki_tmp/munki.pkg")" = "$(jq -er .size "$fixture")"
test "$(shasum -a 256 "$munki_tmp/munki.pkg" | awk '{print $1}')" = "$(jq -er .sha256 "$fixture")"
# Install only CLI core/admin components; no launchd jobs, app or background services.
pkgutil --expand "$munki_tmp/munki.pkg" "$munki_tmp/expanded"
for component in munkitools_core.pkg munkitools_admin.pkg; do
  pkgutil --flatten "$munki_tmp/expanded/$component" "$munki_tmp/$component"
  sudo -n /usr/sbin/installer -pkg "$munki_tmp/$component" -target / >/dev/null
done
test "$(/usr/local/munki/managedsoftwareupdate --version)" = "$(jq -er .version "$fixture")"
echo 'Pinned Munki CLI tools installed; no background services installed'
