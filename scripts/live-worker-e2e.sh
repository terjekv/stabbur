#!/bin/sh
set -eu

server_bin=${STABBUR_SERVER_BIN:-target/debug/stabbur-server}
bind=${STABBUR_E2E_BIND:-127.0.0.1:18082}
base_url="http://${bind}"

for program in curl jq uname; do
  command -v "$program" >/dev/null 2>&1 || {
    echo "required program is unavailable: $program" >&2
    exit 69
  }
done
if [ "$(uname -s)" != Darwin ]; then
  echo "the live worker E2E must run on macOS" >&2
  exit 69
fi
test -x "$server_bin" || {
  echo "server binary is not executable: $server_bin" >&2
  echo "build it first with: cargo build --locked" >&2
  exit 69
}

umask 077
e2e_tmp=$(mktemp -d "${TMPDIR:-/tmp}/stabbur-worker-e2e.XXXXXX")
server_pid=
worker_pid=
cleanup() {
  status=$?
  trap - EXIT HUP INT TERM
  if [ -n "$worker_pid" ]; then
    kill "$worker_pid" 2>/dev/null || true
    wait "$worker_pid" 2>/dev/null || true
  fi
  if [ -n "$server_pid" ]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  if [ "$status" -ne 0 ]; then
    if [ -s "$e2e_tmp/server.log" ]; then
      echo "server log:" >&2
      tail -100 "$e2e_tmp/server.log" >&2
    fi
    if [ -s "$e2e_tmp/worker.log" ]; then
      echo "worker log:" >&2
      tail -100 "$e2e_tmp/worker.log" >&2
    fi
  fi
  if [ "${STABBUR_E2E_KEEP_TMP:-0}" = 1 ]; then
    echo "live worker E2E files retained at $e2e_tmp" >&2
  else
    rm -rf "$e2e_tmp"
  fi
  exit "$status"
}
trap cleanup EXIT HUP INT TERM

data_dir="$e2e_tmp/server"
worker_dir="$e2e_tmp/worker"
mkdir -m 0700 "$data_dir" "$worker_dir"
autopkg_stub="$e2e_tmp/autopkg-stub"
printf '%s\n' '#!/bin/sh' "test \"\${1:-}\" = version || exit 64" \
  'printf "%s\\n" "stabbur-e2e-autopkg 1"' >"$autopkg_stub"
chmod 0700 "$autopkg_stub"

"$server_bin" api --data-dir "$data_dir" --bind "$bind" \
  >"$e2e_tmp/server.log" 2>&1 &
server_pid=$!

attempt=0
until curl --fail --silent "$base_url/readyz" >"$e2e_tmp/readiness.json" 2>/dev/null; do
  if ! kill -0 "$server_pid" 2>/dev/null; then
    echo "server exited before becoming ready" >&2
    exit 1
  fi
  attempt=$((attempt + 1))
  if [ "$attempt" -ge 60 ]; then
    echo "server did not become ready within 60 seconds" >&2
    exit 1
  fi
  sleep 1
done
jq -e '.status == "ready"' "$e2e_tmp/readiness.json" >/dev/null
curl --fail --silent "$base_url/healthz" >"$e2e_tmp/health.json"
jq -e '.status == "ok"' "$e2e_tmp/health.json" >/dev/null

bootstrap_secret=$(tr -d '\r\n' <"$data_dir/bootstrap.secret")
admin_password='Stabbur-Worker-E2E-Only-2026!'
jq -n \
  --arg secret "$bootstrap_secret" \
  --arg username e2e-admin \
  --arg password "$admin_password" \
  '{secret: $secret, username: $username, password: $password}' \
  >"$e2e_tmp/bootstrap.json"
curl --fail --silent --show-error \
  --json @"$e2e_tmp/bootstrap.json" \
  "$base_url/api/v1/auth/bootstrap" >/dev/null

jq -n \
  --arg username e2e-admin \
  --arg password "$admin_password" \
  '{username: $username, password: $password}' \
  >"$e2e_tmp/login.json"
curl --fail --silent --show-error \
  --json @"$e2e_tmp/login.json" \
  "$base_url/api/v1/auth/login" \
  >"$e2e_tmp/session.json"
session_token=$(jq -er .token "$e2e_tmp/session.json")
printf 'header = "Authorization: Bearer %s"\n' "$session_token" \
  >"$e2e_tmp/auth.curl"
unset bootstrap_secret admin_password session_token
rm -f "$e2e_tmp/bootstrap.json" "$e2e_tmp/login.json" "$e2e_tmp/session.json"

"$server_bin" worker --print-capabilities \
  --autopkg-program "$autopkg_stub" >"$e2e_tmp/capabilities.json"
jq -e '
  (.capabilities | index("runtime.portable")) != null and
  (.capabilities | index("builder.fake")) != null and
  (.capabilities | index("builder.autopkg")) != null and
  (.capabilities | index("os.macos")) != null
' "$e2e_tmp/capabilities.json" >/dev/null
jq '{
  name: "e2e-macos-worker",
  allowed_capabilities: .capabilities
}' "$e2e_tmp/capabilities.json" >"$e2e_tmp/provision.json"

curl --fail --silent --show-error \
  --config "$e2e_tmp/auth.curl" \
  --json '{"slug":"e2e-portable","name":"E2E Portable Builder"}' \
  "$base_url/api/v1/software" \
  >"$e2e_tmp/software.json"
software_id=$(jq -er .id "$e2e_tmp/software.json")

curl --fail --silent --show-error \
  --config "$e2e_tmp/auth.curl" \
  --json '{"name":"e2e-portable"}' \
  "$base_url/api/v1/recipes" \
  >"$e2e_tmp/recipe.json"
recipe_id=$(jq -er .id "$e2e_tmp/recipe.json")

curl --fail --silent --show-error \
  --config "$e2e_tmp/auth.curl" \
  --json '{"builder":"fake","definition":{},"required_capabilities":[]}' \
  "$base_url/api/v1/recipes/$recipe_id/revisions" \
  >"$e2e_tmp/revision.json"
revision_id=$(jq -er .id "$e2e_tmp/revision.json")
jq -e '
  .builder == "fake" and
  .definition == {} and
  (.required_capabilities == ["builder.fake", "runtime.portable"])
' "$e2e_tmp/revision.json" >/dev/null

curl --fail --silent --show-error \
  --config "$e2e_tmp/auth.curl" \
  --json @"$e2e_tmp/provision.json" \
  "$base_url/api/v1/workers" \
  >"$worker_dir/credential.json"
chmod 0600 "$worker_dir/credential.json"
worker_id=$(jq -er .worker_id "$worker_dir/credential.json")

jq -n '{
  schema_version: 1,
  producer: "fake",
  source: {
    locator: "urn:stabbur:e2e:portable",
    revision: "fixture-1"
  },
  recipes: [{
    identifier: "e2e-portable.fake",
    builder: "fake",
    parents: [],
    required_capabilities: ["builder.fake", "runtime.portable"]
  }],
  diagnostics: []
}' >"$worker_dir/catalog.json"

"$server_bin" worker \
  --server-url "$base_url" \
  --token-file "$worker_dir/credential.json" \
  --data-dir "$worker_dir/state" \
  --autopkg-program "$autopkg_stub" \
  --catalog-manifest "$worker_dir/catalog.json" \
  >"$e2e_tmp/worker.log" 2>&1 &
worker_pid=$!

attempt=0
registered_seen=
while [ -z "$registered_seen" ]; do
  if ! kill -0 "$worker_pid" 2>/dev/null; then
    echo "worker exited before registering" >&2
    exit 1
  fi
  if curl --fail --silent --show-error \
    --config "$e2e_tmp/auth.curl" \
    "$base_url/api/v1/workers/$worker_id" \
    >"$e2e_tmp/worker.json.next" 2>/dev/null; then
    mv "$e2e_tmp/worker.json.next" "$e2e_tmp/worker.json"
    if jq -e --slurpfile expected "$e2e_tmp/capabilities.json" '
      .enabled == true and
      .name == "e2e-macos-worker" and
      ((.allowed_capabilities | sort) == ($expected[0].capabilities | sort)) and
      ((.advertised_capabilities | sort) == ($expected[0].capabilities | sort))
    ' "$e2e_tmp/worker.json" >/dev/null; then
      registered_seen=$(jq -er .last_seen_at "$e2e_tmp/worker.json")
      break
    fi
  fi
  attempt=$((attempt + 1))
  if [ "$attempt" -ge 60 ]; then
    echo "worker did not register its detected capability set within 60 seconds" >&2
    exit 1
  fi
  sleep 1
done

attempt=0
catalog_snapshot_id=
while [ -z "$catalog_snapshot_id" ]; do
  curl --fail --silent --show-error \
    --config "$e2e_tmp/auth.curl" \
    --get --data-urlencode 'identifier=e2e-portable.fake' \
    "$base_url/api/v1/recipe-catalog-entries" \
    >"$e2e_tmp/catalog-lookup.json"
  catalog_snapshot_id=$(jq -r '
    if .exists == true and (.matches | length) == 1
    then .matches[0].snapshot_id else empty end
  ' "$e2e_tmp/catalog-lookup.json")
  attempt=$((attempt + 1))
  if [ "$attempt" -ge 60 ]; then
    echo "worker did not publish its recipe catalog within 60 seconds" >&2
    exit 1
  fi
  if [ -z "$catalog_snapshot_id" ]; then
    sleep 1
  fi
done
curl --fail --silent --show-error \
  --config "$e2e_tmp/auth.curl" \
  "$base_url/api/v1/recipe-catalogs/$catalog_snapshot_id" \
  >"$e2e_tmp/catalog.json"
jq -e '
  .summary.producer == "fake" and
  .summary.recipe_count == 1 and
  .manifest.source.revision == "fixture-1" and
  .manifest.recipes[0].identifier == "e2e-portable.fake"
' "$e2e_tmp/catalog.json" >/dev/null

# Exercise server-requested catalog work without relying on public network input. The worker must
# claim the AutoPkg-capable job and record a typed safe failure for the intentionally unreachable
# pinned HTTPS repository; generator success is covered independently with a local Git fixture.
jq -n '{
  producer: "autopkg",
  source: {
    locator: "https://127.0.0.1:9/stabbur-e2e-catalog.git",
    revision: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  }
}' >"$e2e_tmp/catalog-scan-request.json"
curl --fail --silent --show-error \
  --config "$e2e_tmp/auth.curl" \
  --header 'Idempotency-Key: macos-e2e-catalog-scan' \
  --json @"$e2e_tmp/catalog-scan-request.json" \
  "$base_url/api/v1/recipe-catalog-scans" \
  >"$e2e_tmp/catalog-scan.json"
catalog_scan_id=$(jq -er .id "$e2e_tmp/catalog-scan.json")

attempt=0
while :; do
  if ! kill -0 "$worker_pid" 2>/dev/null; then
    echo "worker exited before terminalizing the requested catalog scan" >&2
    exit 1
  fi
  curl --fail --silent --show-error \
    --config "$e2e_tmp/auth.curl" \
    "$base_url/api/v1/recipe-catalog-scans/$catalog_scan_id" \
    >"$e2e_tmp/catalog-scan.json.next"
  mv "$e2e_tmp/catalog-scan.json.next" "$e2e_tmp/catalog-scan.json"
  state=$(jq -er .state "$e2e_tmp/catalog-scan.json")
  case "$state" in
    failed) break ;;
    succeeded|cancelled)
      echo "catalog scan reached unexpected terminal state: $state" >&2
      exit 1
      ;;
  esac
  attempt=$((attempt + 1))
  if [ "$attempt" -ge 60 ]; then
    echo "worker did not terminalize the requested catalog scan within 60 seconds" >&2
    exit 1
  fi
  sleep 1
done
jq -e '
  .producer == "autopkg" and
  .failure.producer == "autopkg" and
  .failure.code == "autopkg_materialization_failed" and
  .snapshot_id == null and
  .completed_at != null
' "$e2e_tmp/catalog-scan.json" >/dev/null

jq -n --arg software "$software_id" --arg revision "$revision_id" \
  '{
    name: "e2e-portable-scheduled",
    software: $software,
    recipe_revision: $revision,
    parameters: {fixture: "macos-e2e"},
    schedule: {kind: "interval", every_seconds: 31536000}
  }' \
  >"$e2e_tmp/target-request.json"
curl --fail --silent --show-error \
  --config "$e2e_tmp/auth.curl" \
  --json @"$e2e_tmp/target-request.json" \
  "$base_url/api/v1/build-targets" \
  >"$e2e_tmp/target.json"
target_id=$(jq -er .id "$e2e_tmp/target.json")

attempt=0
run_id=
while [ -z "$run_id" ]; do
  curl --fail --silent --show-error \
    --config "$e2e_tmp/auth.curl" \
    "$base_url/api/v1/build-targets/$target_id/runs" \
    >"$e2e_tmp/target-runs.json"
  run_id=$(jq -r 'if (.items | length) == 1 then .items[0].id else empty end' \
    "$e2e_tmp/target-runs.json")
  attempt=$((attempt + 1))
  if [ "$attempt" -ge 60 ]; then
    echo "scheduler did not create exactly one target run within 60 seconds" >&2
    exit 1
  fi
  if [ -z "$run_id" ]; then
    sleep 1
  fi
done

attempt=0
while :; do
  if ! kill -0 "$worker_pid" 2>/dev/null; then
    echo "worker exited before completing the fake run" >&2
    exit 1
  fi
  if curl --fail --silent --show-error \
    --config "$e2e_tmp/auth.curl" \
    "$base_url/api/v1/runs/$run_id" \
    >"$e2e_tmp/run.json.next" 2>/dev/null; then
    mv "$e2e_tmp/run.json.next" "$e2e_tmp/run.json"
    state=$(jq -er .state "$e2e_tmp/run.json")
    case "$state" in
      succeeded) break ;;
      failed|cancelled)
        echo "fake run reached unexpected terminal state: $state" >&2
        exit 1
        ;;
    esac
  fi
  attempt=$((attempt + 1))
  if [ "$attempt" -ge 60 ]; then
    echo "worker did not complete the fake run within 60 seconds" >&2
    exit 1
  fi
  sleep 1
done

jq -e --arg run "$run_id" '
  .state == "succeeded" and
  .completed_at != null and
  .result.schema_version == 1 and
  .result.run_id == $run and
  .result.adapter == "fake" and
  .result.build_result == null and
  .result.raw_report.builder == "fake"
' "$e2e_tmp/run.json" >/dev/null

curl --fail --silent --show-error \
  --config "$e2e_tmp/auth.curl" \
  "$base_url/api/v1/jobs?limit=50" \
  >"$e2e_tmp/jobs.json"
job_id=$(jq -er --arg run "$run_id" '
  [.items[] | select(.run_id == $run)] |
  if length == 1 then .[0].id else error("expected exactly one run job") end
' "$e2e_tmp/jobs.json")
catalog_scan_job_id=$(jq -er --arg scan "$catalog_scan_id" '
  [.items[] | select(.recipe_catalog_scan_id == $scan)] |
  if length == 1 then .[0].id else error("expected exactly one catalog scan job") end
' "$e2e_tmp/jobs.json")
curl --fail --silent --show-error \
  --config "$e2e_tmp/auth.curl" \
  "$base_url/api/v1/jobs/$job_id" \
  >"$e2e_tmp/job.json"
jq -e --arg run "$run_id" '
  .run_id == $run and
  .state == "succeeded" and
  .attempt_count == 1 and
  .payload.adapter == "fake" and
  .payload.adapter_definition == {} and
  .payload.request.run_id == $run and
  (.required_capabilities == ["builder.fake", "runtime.portable"])
' "$e2e_tmp/job.json" >/dev/null

curl --fail --silent --show-error \
  --config "$e2e_tmp/auth.curl" \
  "$base_url/api/v1/audit?limit=200" \
  >"$e2e_tmp/audit.json"
jq -e '
  ([
    "software.create",
    "recipe.create",
    "recipe.revision.create",
    "worker.provision",
    "recipe_catalog.publish",
    "recipe_catalog_scan.create",
    "recipe_catalog_scan.fail",
    "build_target.create",
    "build_target.schedule"
  ] - [.items[].action]) | length == 0
' "$e2e_tmp/audit.json" >/dev/null

curl --fail --silent --show-error \
  --config "$e2e_tmp/auth.curl" \
  "$base_url/api/v1/workers/$worker_id" \
  >"$e2e_tmp/worker.json"
current_seen=$(jq -er .last_seen_at "$e2e_tmp/worker.json")

jq -n \
  --arg server_version "$("$server_bin" --version)" \
  --arg worker_id "$worker_id" \
  --arg target_id "$target_id" \
  --arg catalog_scan_id "$catalog_scan_id" \
  --arg catalog_scan_job_id "$catalog_scan_job_id" \
  --arg run_id "$run_id" \
  --arg job_id "$job_id" \
  --arg first_seen_at "$registered_seen" \
  --arg completed_seen_at "$current_seen" \
  --slurpfile capabilities "$e2e_tmp/capabilities.json" \
  '{
    result: "passed",
    server_version: $server_version,
    worker_id: $worker_id,
    target_id: $target_id,
    catalog_scan_id: $catalog_scan_id,
    catalog_scan_job_id: $catalog_scan_job_id,
    run_id: $run_id,
    job_id: $job_id,
    capabilities: $capabilities[0].capabilities,
    first_seen_at: $first_seen_at,
    completed_seen_at: $completed_seen_at,
    run_state: "succeeded",
    catalog_scan_state: "failed_as_expected",
    job_state: "succeeded",
    attempt_count: 1
  }'
