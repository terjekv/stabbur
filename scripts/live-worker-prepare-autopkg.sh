#!/bin/sh
set -eu

fixture=${1:-tests/fixtures/autopkg-prepare/autopkg-2.9.0.json}
server_bin=${STABBUR_SERVER_BIN:-target/debug/stabbur-server}

if [ "$(uname -s)" != "Darwin" ]; then
  echo "AutoPkg preparation acceptance requires macOS" >&2
  exit 69
fi
if [ "${STABBUR_DISPOSABLE_MACOS:-}" != "1" ]; then
  echo "refusing to install AutoPkg unless STABBUR_DISPOSABLE_MACOS=1" >&2
  exit 77
fi
if [ "$(/usr/bin/id -u)" = "0" ]; then
  echo "run this acceptance script as an unprivileged user with passwordless sudo" >&2
  exit 77
fi

for program in awk curl jq shasum stat sudo; do
  command -v "$program" >/dev/null 2>&1 || {
    echo "required program is unavailable: $program" >&2
    exit 69
  }
done
jq_program=$(command -v jq)
test -x "$server_bin" || {
  echo "server binary is not executable: $server_bin" >&2
  exit 69
}
test -r "$fixture" || {
  echo "AutoPkg preparation fixture is not readable: $fixture" >&2
  exit 66
}

jq --exit-status '
  .schema_version == 1 and
  (.release.tag | type == "string" and length > 0) and
  (.release.page | type == "string" and startswith("https://")) and
  (.release.url | type == "string" and startswith("https://")) and
  (.release.size | type == "number" and . > 0) and
  (.release.sha256 | test("^[0-9a-f]{64}$")) and
  (.expected_manifest_sha256 | test("^[0-9a-f]{64}$")) and
  .manifest.schema_version == 1 and
  .manifest.builder == "autopkg" and
  .manifest.package.sha256 == .release.sha256 and
  .manifest.package.signature == {"policy":"unsigned"} and
  (.manifest.health_check.program | startswith("/"))
' "$fixture" >/dev/null

acceptance_tmp=$(mktemp -d "${TMPDIR:-/tmp}/stabbur-worker-prepare.XXXXXX")
receipt="$acceptance_tmp/state/autopkg-prepared.json"
cleanup() {
  sudo -n /bin/rm -f "$receipt" 2>/dev/null || true
  sudo -n /bin/rmdir "$acceptance_tmp/state" 2>/dev/null || true
  rm -rf "$acceptance_tmp"
}
trap cleanup EXIT HUP INT TERM

package="$acceptance_tmp/autopkg.pkg"
manifest="$acceptance_tmp/manifest.json"
checked="$acceptance_tmp/checked.json"
installed="$acceptance_tmp/installed.json"
repeated="$acceptance_tmp/repeated.json"
capabilities="$acceptance_tmp/capabilities.json"

release_url=$(jq -r '.release.url' "$fixture")
expected_size=$(jq -r '.release.size' "$fixture")
expected_sha256=$(jq -r '.release.sha256' "$fixture")

curl --fail --silent --show-error --location \
  --proto '=https' --proto-redir '=https' \
  --output "$package" "$release_url"
chmod 0600 "$package"

actual_size=$(stat -f '%z' "$package")
actual_sha256=$(shasum -a 256 "$package" | awk '{print $1}')
test "$actual_size" = "$expected_size" || {
  echo "downloaded AutoPkg package size does not match the fixture" >&2
  exit 65
}
test "$actual_sha256" = "$expected_sha256" || {
  echo "downloaded AutoPkg package SHA-256 does not match the fixture" >&2
  exit 65
}

jq --arg package "$package" '.manifest | .package.path = $package' \
  "$fixture" >"$manifest"
chmod 0600 "$manifest"

"$server_bin" worker prepare --manifest "$manifest" --receipt "$receipt" --check >"$checked"
jq --exit-status \
  --arg version "$(jq -r '.manifest.version' "$fixture")" \
  --arg sha256 "$expected_sha256" \
  --arg manifest_sha256 "$(jq -r '.expected_manifest_sha256' "$fixture")" \
  '.status == "verified" and .builder == "autopkg" and
   .version == $version and .package_sha256 == $sha256 and
   .manifest_sha256 == $manifest_sha256' \
  "$checked" >/dev/null

installed_report=$(sudo -n "$server_bin" worker prepare \
  --manifest "$manifest" --receipt "$receipt")
printf '%s\n' "$installed_report" >"$installed"
jq --exit-status \
  --arg version "$(jq -r '.manifest.version' "$fixture")" \
  --arg sha256 "$expected_sha256" \
  '.status == "installed" and .builder == "autopkg" and
   .version == $version and .package_sha256 == $sha256' \
  "$installed" >/dev/null

repeated_report=$(sudo -n "$server_bin" worker prepare \
  --manifest "$manifest" --receipt "$receipt")
printf '%s\n' "$repeated_report" >"$repeated"
jq --exit-status \
  '.status == "already_prepared" and .builder == "autopkg"' \
  "$repeated" >/dev/null

sudo -n "$jq_program" --exit-status \
  '.builder == "autopkg" and .version == "2.9.0" and
   .package_identifier == "com.github.autopkg.autopkg" and
   .signature == {"policy":"unsigned"}' \
  "$receipt" >/dev/null

"$server_bin" worker --print-capabilities \
  --autopkg-program /Library/AutoPkg/autopkg >"$capabilities"
jq --exit-status '
  (.capabilities | index("builder.autopkg")) != null and
  .tools.autopkg == "2.9.0"
' "$capabilities" >/dev/null

echo "pinned AutoPkg worker preparation acceptance passed"
