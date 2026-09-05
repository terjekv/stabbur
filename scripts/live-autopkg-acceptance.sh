#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 REVISION_JSON EVIDENCE_JSON" >&2
  exit 64
fi

revision_json=$1
evidence_json=$2
server_bin=${STABBUR_SERVER_BIN:-target/release/stabbur-server}
autopkg_program=${STABBUR_AUTOPKG_PROGRAM:-}
bind=${STABBUR_ACCEPTANCE_BIND:-127.0.0.1:18081}
base_url="http://${bind}"

for program in curl jq git; do
  command -v "$program" >/dev/null 2>&1 || {
    echo "required program is unavailable: $program" >&2
    exit 69
  }
done
if [ -n "$autopkg_program" ]; then
  case "$autopkg_program" in
    /*) ;;
    *)
      echo "STABBUR_AUTOPKG_PROGRAM must be an absolute path" >&2
      exit 64
      ;;
  esac
  test -x "$autopkg_program" || {
    echo "configured AutoPkg program is not executable" >&2
    exit 69
  }
else
  command -v autopkg >/dev/null 2>&1 || {
    echo "required program is unavailable: autopkg" >&2
    exit 69
  }
fi
test -x "$server_bin" || {
  echo "server binary is not executable: $server_bin" >&2
  exit 69
}

test -r "$revision_json" || {
  echo "revision fixture is not readable: $revision_json" >&2
  exit 66
}
jq -e 'type == "object" and (.sources | length > 0) and (.output.variants | length > 0)' \
  "$revision_json" >/dev/null

acceptance_tmp=$(mktemp -d "${TMPDIR:-/tmp}/stabbur-autopkg.XXXXXX")
server_pid=
worker_pid=
cleanup() {
  if [ -n "$worker_pid" ]; then
    kill "$worker_pid" 2>/dev/null || true
    wait "$worker_pid" 2>/dev/null || true
  fi
  if [ -n "$server_pid" ]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$acceptance_tmp"
}
trap cleanup EXIT HUP INT TERM

data_dir="$acceptance_tmp/server"
worker_dir="$acceptance_tmp/worker"
mkdir -m 0700 "$data_dir" "$worker_dir"

"$server_bin" api --data-dir "$data_dir" --bind "$bind" \
  >"$acceptance_tmp/server.log" 2>&1 &
server_pid=$!

attempt=0
until curl --fail --silent "$base_url/readyz" >/dev/null 2>&1; do
  attempt=$((attempt + 1))
  if [ "$attempt" -ge 60 ]; then
    echo "server did not become ready" >&2
    exit 1
  fi
  sleep 1
done

bootstrap_secret=$(tr -d '\r\n' <"$data_dir/bootstrap.secret")
admin_password='Stabbur-Acceptance-Only-2026!'
jq -n \
  --arg secret "$bootstrap_secret" \
  --arg username acceptance-admin \
  --arg password "$admin_password" \
  '{secret: $secret, username: $username, password: $password}' \
  >"$acceptance_tmp/bootstrap.json"
curl --fail --silent --show-error \
  --json @"$acceptance_tmp/bootstrap.json" \
  "$base_url/api/v1/auth/bootstrap" >/dev/null

jq -n \
  --arg username acceptance-admin \
  --arg password "$admin_password" \
  '{username: $username, password: $password}' \
  >"$acceptance_tmp/login.json"
curl --fail --silent --show-error \
  --json @"$acceptance_tmp/login.json" \
  "$base_url/api/v1/auth/login" \
  >"$acceptance_tmp/session.json"
session_token=$(jq -er .token "$acceptance_tmp/session.json")
umask 077
printf 'header = "Authorization: Bearer %s"\n' "$session_token" \
  >"$acceptance_tmp/auth.curl"
unset bootstrap_secret admin_password session_token
rm -f "$acceptance_tmp/bootstrap.json" "$acceptance_tmp/login.json" \
  "$acceptance_tmp/session.json"

curl --fail --silent --show-error \
  --config "$acceptance_tmp/auth.curl" \
  --json '{"slug":"acceptance-autopkg","name":"AutoPkg acceptance fixture"}' \
  "$base_url/api/v1/software" \
  >"$acceptance_tmp/software.json"
software_id=$(jq -er .id "$acceptance_tmp/software.json")

curl --fail --silent --show-error \
  --config "$acceptance_tmp/auth.curl" \
  --json '{"name":"acceptance-autopkg"}' \
  "$base_url/api/v1/recipes" \
  >"$acceptance_tmp/recipe.json"
recipe_id=$(jq -er .id "$acceptance_tmp/recipe.json")

jq '{
  builder: "autopkg",
  definition: del(.required_capabilities),
  required_capabilities: (.required_capabilities // [])
}' "$revision_json" >"$acceptance_tmp/revision-request.json"
curl --fail --silent --show-error \
  --config "$acceptance_tmp/auth.curl" \
  --json @"$acceptance_tmp/revision-request.json" \
  "$base_url/api/v1/recipes/$recipe_id/revisions" \
  >"$acceptance_tmp/revision.json"
revision_id=$(jq -er .id "$acceptance_tmp/revision.json")

curl --fail --silent --show-error \
  --config "$acceptance_tmp/auth.curl" \
  --json '{"name":"acceptance-macos-worker","allowed_capabilities":["runtime.portable","builder.fake","os.macos","builder.autopkg","tool.apple-xcode"]}' \
  "$base_url/api/v1/workers" \
  >"$worker_dir/credential.json"
chmod 0600 "$worker_dir/credential.json"

if [ -n "$autopkg_program" ]; then
  "$server_bin" worker \
    --server-url "$base_url" \
    --token-file "$worker_dir/credential.json" \
    --data-dir "$worker_dir/state" \
    --autopkg-program "$autopkg_program" \
    >"$acceptance_tmp/worker.log" 2>&1 &
else
  "$server_bin" worker \
    --server-url "$base_url" \
    --token-file "$worker_dir/credential.json" \
    --data-dir "$worker_dir/state" \
    >"$acceptance_tmp/worker.log" 2>&1 &
fi
worker_pid=$!

jq -n \
  --arg software "$software_id" \
  --arg revision "$revision_id" \
  '{software: $software, recipe_revision: $revision, parameters: {}}' \
  >"$acceptance_tmp/run-request.json"
curl --fail --silent --show-error \
  --config "$acceptance_tmp/auth.curl" \
  --header 'Idempotency-Key: live-autopkg-acceptance-v1' \
  --json @"$acceptance_tmp/run-request.json" \
  "$base_url/api/v1/runs" \
  >"$acceptance_tmp/run.json"
run_id=$(jq -er .id "$acceptance_tmp/run.json")

attempt=0
while :; do
  curl --fail --silent --show-error \
    --config "$acceptance_tmp/auth.curl" \
    "$base_url/api/v1/runs/$run_id" \
    >"$acceptance_tmp/run.json"
  state=$(jq -er .state "$acceptance_tmp/run.json")
  case "$state" in
    succeeded)
      break
      ;;
    failed|cancelled)
      jq . "$acceptance_tmp/run.json" >&2
      exit 1
      ;;
  esac
  attempt=$((attempt + 1))
  if [ "$attempt" -ge 720 ]; then
    echo "AutoPkg acceptance run did not finish within two hours" >&2
    exit 1
  fi
  sleep 10
done

curl --fail --silent --show-error \
  --config "$acceptance_tmp/auth.curl" \
  "$base_url/api/v1/software/$software_id/releases" \
  >"$acceptance_tmp/releases.json"
release_id=$(jq -er '.items[0].id' "$acceptance_tmp/releases.json")
curl --fail --silent --show-error \
  --config "$acceptance_tmp/auth.curl" \
  "$base_url/api/v1/releases/$release_id" \
  >"$acceptance_tmp/release.json"
curl --fail --silent --show-error \
  --config "$acceptance_tmp/auth.curl" \
  "$base_url/api/v1/releases/$release_id/variants" \
  >"$acceptance_tmp/variants.json"
artifact_digest=$(jq -er \
  '[.items[].artifacts[] | select(.role == "primary_installer")] | if length == 1 then .[0].digest else error("expected exactly one primary installer") end' \
  "$acceptance_tmp/variants.json")
curl --fail --silent --show-error \
  --config "$acceptance_tmp/auth.curl" \
  "$base_url/api/v1/artifacts/$artifact_digest" \
  >"$acceptance_tmp/artifact.json"
curl --fail --silent --show-error \
  --config "$acceptance_tmp/auth.curl" \
  "$base_url/api/v1/artifacts/$artifact_digest/locations" \
  >"$acceptance_tmp/locations.json"
curl --fail --silent --show-error \
  --config "$acceptance_tmp/auth.curl" \
  "$base_url/api/v1/software/$software_id/channels/candidate" \
  >"$acceptance_tmp/candidate.json"
curl --fail --silent --show-error \
  --config "$acceptance_tmp/auth.curl" \
  "$base_url/api/v1/runs/$run_id/logs?limit=200" \
  >"$acceptance_tmp/logs.json"

jq -e --arg release "$release_id" \
  '.release_id == $release' "$acceptance_tmp/candidate.json" >/dev/null
jq -e '[.items[].artifacts[] | select(.role == "primary_installer")] | length == 1' \
  "$acceptance_tmp/variants.json" >/dev/null
jq -e '.state == "candidate"' "$acceptance_tmp/release.json" >/dev/null
jq -e '[.items[] | select(.state == "present" and .verified_at != null)] | length >= 1' \
  "$acceptance_tmp/locations.json" >/dev/null

server_version=$("$server_bin" --version)
if [ -n "$autopkg_program" ]; then
  autopkg_version=$("$autopkg_program" version 2>&1 | tail -1)
else
  autopkg_version=$(autopkg version 2>&1 | tail -1)
fi
xcode_version=$(xcodebuild -version 2>&1 | tr '\n' ' ')
jq -n \
  --arg recorded_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --arg server_version "$server_version" \
  --arg autopkg_version "$autopkg_version" \
  --arg xcode_version "$xcode_version" \
  --slurpfile software "$acceptance_tmp/software.json" \
  --slurpfile recipe "$acceptance_tmp/recipe.json" \
  --slurpfile revision "$acceptance_tmp/revision.json" \
  --slurpfile run "$acceptance_tmp/run.json" \
  --slurpfile releases "$acceptance_tmp/releases.json" \
  --slurpfile release "$acceptance_tmp/release.json" \
  --slurpfile variants "$acceptance_tmp/variants.json" \
  --slurpfile artifact "$acceptance_tmp/artifact.json" \
  --slurpfile locations "$acceptance_tmp/locations.json" \
  --slurpfile candidate "$acceptance_tmp/candidate.json" \
  --slurpfile logs "$acceptance_tmp/logs.json" \
  '{
    recorded_at: $recorded_at,
    server_version: $server_version,
    autopkg_version: $autopkg_version,
    xcode_version: $xcode_version,
    software: $software[0],
    recipe: $recipe[0],
    revision: $revision[0],
    run: $run[0],
    releases: $releases[0],
    release: $release[0],
    variants: $variants[0],
    artifact: $artifact[0],
    locations: $locations[0],
    candidate: $candidate[0],
    logs: $logs[0]
  }' >"$evidence_json"

echo "AutoPkg acceptance evidence written to $evidence_json"
