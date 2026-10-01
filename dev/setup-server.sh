#!/usr/bin/env bash
# dev/setup-server.sh
#
# Idempotent first-boot automation for the Jellybeam dev Jellyfin server.
# Drives the startup wizard over the REST API (no browser needed), creates
# an admin + a non-admin test user, wires up Movies/Shows libraries pointed
# at dev/media, and kicks off a library scan.
#
# Safe to re-run: if the wizard is already complete it skips straight to
# checking/creating users and libraries, then re-triggers a scan.
#
# Usage: dev/setup-server.sh [base_url]
#   base_url defaults to http://localhost:8096

set -euo pipefail

BASE_URL="${1:-http://localhost:8096}"
ADMIN_USER="jellybeam-admin"
ADMIN_PASS="jellybeam-test"
SECOND_USER="jellybeam-user"
SECOND_PASS="jellybeam-test"
SERVER_NAME="Jellybeam Dev Server"
CLIENT_HEADER='Client="Jellybeam Dev Setup", Device="setup-script", DeviceId="jellybeam-setup-script", Version="1.0.0"'

# Path *inside the jellyfin container* (see dev/docker-compose.yml: ./media -> /media)
MEDIA_ROOT="/media"
MOVIES_PATH="${MEDIA_ROOT}/Movies"
SHOWS_PATH="${MEDIA_ROOT}/Shows"

log() { printf '[setup-server] %s\n' "$*" >&2; }
die() { printf '[setup-server] ERROR: %s\n' "$*" >&2; exit 1; }

curl_json() {
  # curl_json METHOD PATH [DATA] [EXTRA_AUTH_HEADER]
  local method="$1" path="$2" data="${3:-}" auth="${4:-}"
  local -a args=(-fsS -X "$method" "${BASE_URL}${path}" -H "Content-Type: application/json")
  # Standard `Authorization: MediaBrowser ...` header, same scheme/value as
  # the legacy `X-Emby-Authorization` header it replaces. Jellyfin 12.0
  # rejects `X-Emby-Authorization` by default (legacy authorization is
  # disabled unless re-enabled server-side); `Authorization` works
  # identically on 10.10/10.11 too, so one header covers every server this
  # script targets.
  if [[ -n "$auth" ]]; then
    args+=(-H "Authorization: MediaBrowser ${CLIENT_HEADER}, Token=\"${auth}\"")
  else
    args+=(-H "Authorization: MediaBrowser ${CLIENT_HEADER}")
  fi
  if [[ -n "$data" ]]; then
    args+=(-d "$data")
  fi
  curl "${args[@]}"
}

wait_for_server() {
  log "waiting for ${BASE_URL} to come up..."
  local i=0
  until curl -fsS -o /dev/null "${BASE_URL}/System/Info/Public" 2>/dev/null \
      && curl -fsS -o /dev/null "${BASE_URL}/health" 2>/dev/null; do
    i=$((i + 1))
    if [[ $i -gt 90 ]]; then
      die "server did not respond within ~90s"
    fi
    sleep 1
  done
  log "server is responding"
}

wizard_completed() {
  curl -fsS "${BASE_URL}/System/Info/Public" | grep -q '"StartupWizardCompleted":true'
}

run_startup_wizard() {
  if wizard_completed; then
    log "startup wizard already completed, skipping wizard steps"
    return
  fi

  log "running startup wizard..."

  # 1. Server-wide config (name, language, metadata locale).
  curl_json POST /Startup/Configuration \
    "$(printf '{"ServerName":"%s","UICulture":"en-US","MetadataCountryCode":"US","PreferredMetadataLanguage":"en"}' "$SERVER_NAME")" \
    >/dev/null

  # 2. Remote access / UPnP — off, this is a local dev box only.
  curl_json POST /Startup/RemoteAccess '{"EnableRemoteAccess":false}' >/dev/null

  # 3. Initialize + fetch the auto-created first user, then set its
  #    username/password. This is what the wizard's "create admin account"
  #    screen does under the hood.
  curl -fsS "${BASE_URL}/Startup/User" >/dev/null # triggers _userManager.InitializeAsync()
  curl_json POST /Startup/User \
    "$(printf '{"Name":"%s","Password":"%s"}' "$ADMIN_USER" "$ADMIN_PASS")" \
    >/dev/null

  # 4. Complete the wizard.
  curl_json POST /Startup/Complete '' >/dev/null

  log "startup wizard complete; admin user '${ADMIN_USER}' created"
}

authenticate() {
  local username="$1" password="$2"
  local resp
  resp="$(curl_json POST /Users/AuthenticateByName \
    "$(printf '{"Username":"%s","Pw":"%s"}' "$username" "$password")")"
  printf '%s' "$resp" | sed -n 's/.*"AccessToken":"\([^"]*\)".*/\1/p'
}

ensure_second_user() {
  local admin_token="$1"
  local existing
  existing="$(curl_json GET /Users "" "$admin_token" | grep -o "\"Name\":\"${SECOND_USER}\"" || true)"
  if [[ -z "$existing" ]]; then
    log "creating non-admin user '${SECOND_USER}'..."
    curl_json POST /Users/New \
      "$(printf '{"Name":"%s","Password":"%s"}' "$SECOND_USER" "$SECOND_PASS")" \
      "$admin_token" >/dev/null
    log "user '${SECOND_USER}' created (non-admin by default)"
  else
    log "user '${SECOND_USER}' already exists"
  fi
  # Always (re-)grant library access: EnableAllFolders drifts back to false
  # when libraries are re-created after the user (observed repeatedly — it
  # breaks the multi-user E2E with "no libraries"), so enforce it on every
  # run rather than only at creation time. Idempotent.
  local uid policy
  uid="$(curl_json GET /Users "" "$admin_token" \
    | python3 -c 'import json,sys; us=json.load(sys.stdin); print(next(u["Id"] for u in us if u["Name"]=="'"${SECOND_USER}"'"))')"
  policy="$(curl_json GET "/Users/${uid}" "" "$admin_token" \
    | python3 -c 'import json,sys; p=json.load(sys.stdin)["Policy"]; p["EnableAllFolders"]=True; print(json.dumps(p))')"
  curl_json POST "/Users/${uid}/Policy" "$policy" "$admin_token" >/dev/null
  log "user '${SECOND_USER}' granted access to all libraries (EnableAllFolders)"
}

ensure_library() {
  local admin_token="$1" name="$2" collection_type="$3" path="$4"
  local existing
  existing="$(curl_json GET /Library/VirtualFolders "" "$admin_token" | grep -o "\"Name\":\"${name}\"" || true)"
  if [[ -n "$existing" ]]; then
    log "library '${name}' already exists, skipping"
    return
  fi
  log "creating library '${name}' (${collection_type}) -> ${path}"
  local encoded_path="${path// /%20}"
  # Disable remote (internet) metadata/image fetchers for every item type.
  # Without this, Jellyfin's TMDb identifier will happily "match" our
  # codec-named synthetic files (e.g. "av1-aac") against unrelated real
  # movies and silently rename them, which defeats the point of using
  # descriptive filenames as stable, predictable fake titles.
  local type_options='[
    {"Type":"Movie","MetadataFetchers":[],"ImageFetchers":[]},
    {"Type":"Series","MetadataFetchers":[],"ImageFetchers":[]},
    {"Type":"Season","MetadataFetchers":[],"ImageFetchers":[]},
    {"Type":"Episode","MetadataFetchers":[],"ImageFetchers":[]}
  ]'
  curl -fsS -X POST \
    "${BASE_URL}/Library/VirtualFolders?name=${name}&collectionType=${collection_type}&paths=${encoded_path}&refreshLibrary=false" \
    -H "Authorization: MediaBrowser ${CLIENT_HEADER}, Token=\"${admin_token}\"" \
    -H "Content-Type: application/json" \
    -d "$(printf '{"LibraryOptions":{"EnablePhotos":false,"EnableRealtimeMonitor":false,"TypeOptions":%s}}' "$type_options")" \
    >/dev/null
}

trigger_scan() {
  local admin_token="$1"
  log "triggering library scan..."
  curl -fsS -X POST "${BASE_URL}/Library/Refresh" \
    -H "Authorization: MediaBrowser ${CLIENT_HEADER}, Token=\"${admin_token}\"" \
    >/dev/null
  log "scan triggered (runs in background on the server)"
}

main() {
  wait_for_server
  run_startup_wizard

  log "authenticating as '${ADMIN_USER}'..."
  local admin_token=""
  local tries=0
  until [[ -n "$admin_token" ]]; do
    admin_token="$(authenticate "$ADMIN_USER" "$ADMIN_PASS" || true)"
    if [[ -z "$admin_token" ]]; then
      tries=$((tries + 1))
      [[ $tries -gt 20 ]] && die "could not authenticate as ${ADMIN_USER} after wizard"
      sleep 1
    fi
  done
  log "authenticated; got admin access token"

  ensure_library "$admin_token" "Movies" "movies" "$MOVIES_PATH"
  ensure_library "$admin_token" "Shows" "tvshows" "$SHOWS_PATH"
  ensure_second_user "$admin_token"
  trigger_scan "$admin_token"

  cat >&2 <<EOF

============================================================
 Jellybeam dev Jellyfin server is ready.

   URL:            ${BASE_URL}
   Admin user:      ${ADMIN_USER} / ${ADMIN_PASS}
   Non-admin user:  ${SECOND_USER} / ${SECOND_PASS}
   Admin API token: ${admin_token}

   A library scan was just triggered; it runs asynchronously.
   Check progress with: dev/server.sh logs
============================================================
EOF
  # Also print just the token on stdout for scripting (e.g. `dev/server.sh up`
  # capturing it, or another agent grabbing it non-interactively).
  printf '%s\n' "$admin_token"
}

main "$@"
