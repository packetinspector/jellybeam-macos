#!/usr/bin/env bash
#
# dev/server.sh — the dev Jellyfin server and its synthetic corpus.
#
#   dev/server.sh corpus     generate the synthetic media library into dev/media/
#   dev/server.sh up         start the pinned Jellyfin on :8096 and run first-boot setup
#   dev/server.sh down       stop it (data survives)
#   dev/server.sh reset      wipe its volumes and the corpus, regenerate, start fresh
#   dev/server.sh logs       tail its logs
#   dev/server.sh doctor     check Docker, images, corpus and both servers
#   dev/server.sh 12-up      opt-in Jellyfin 12.x on :8097 (compose profile v12)
#   dev/server.sh 12-down    stop only that container
#   dev/server.sh 12-reset   wipe only that server's volumes and start it fresh
#   dev/server.sh 12-logs    tail its logs
#
# See dev/README.md.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

COMPOSE=(docker compose -f dev/docker-compose.yml)
COMPOSE12=(docker compose -f dev/docker-compose.yml --profile v12)
JELLYFIN_URL="http://localhost:8096"
JELLYFIN12_URL="http://localhost:8097"
PINNED_IMAGE="$(awk '/^[[:space:]]*image: jellyfin\/jellyfin:/ {sub(/^[[:space:]]*image: /, ""); print; exit}' dev/docker-compose.yml)"

status() {
  local label="$1"; shift
  printf '%-20s' "$label:"
  "$@"
}

doctor() {
  echo "== doctor =="
  status "docker daemon" bash -c 'if docker info >/dev/null 2>&1; then echo OK; else echo "FAIL (is Docker running?)"; exit 1; fi'
  status "jellyfin image" bash -c "if docker image inspect $PINNED_IMAGE >/dev/null 2>&1; then echo \"OK ($PINNED_IMAGE present)\"; else echo \"MISSING (docker pull $PINNED_IMAGE)\"; fi"
  status "ffmpeg gen image" bash -c 'if docker image inspect linuxserver/ffmpeg:latest >/dev/null 2>&1; then echo "OK"; else echo "MISSING (docker pull linuxserver/ffmpeg:latest)"; fi'
  status "media corpus" bash -c 'n=$(find dev/media -type f \( -name "*.mkv" -o -name "*.mp4" -o -name "*.ts" \) 2>/dev/null | wc -l | tr -d " "); if [ "$n" -gt 0 ]; then echo "OK ($n media files in dev/media)"; else echo "EMPTY (run: dev/server.sh corpus)"; fi'
  status "server" bash -c "if curl -fsS -o /dev/null $JELLYFIN_URL/System/Info/Public 2>/dev/null; then echo \"OK ($JELLYFIN_URL)\"; else echo \"DOWN (run: dev/server.sh up)\"; fi"
  status "jellyfin12 image" bash -c 'if docker image inspect jellyfin/jellyfin:12.0 >/dev/null 2>&1; then echo OK; else echo "MISSING (opt-in: dev/server.sh 12-up pulls it)"; fi'
  status "jellyfin12 server" bash -c "if curl -fsS -o /dev/null $JELLYFIN12_URL/System/Info/Public 2>/dev/null; then echo \"OK ($JELLYFIN12_URL)\"; else echo \"not started (opt-in: dev/server.sh 12-up)\"; fi"
}

case "${1:-}" in
  corpus)   dev/corpus/gen-corpus.sh ;;
  up)       "${COMPOSE[@]}" up -d; dev/setup-server.sh "$JELLYFIN_URL" ;;
  down)     "${COMPOSE[@]}" down ;;
  reset)    "${COMPOSE[@]}" down -v; rm -rf dev/media; dev/corpus/gen-corpus.sh; "${COMPOSE[@]}" up -d; dev/setup-server.sh "$JELLYFIN_URL" ;;
  logs)     "${COMPOSE[@]}" logs -f jellyfin ;;
  doctor)   doctor ;;
  12-up)    "${COMPOSE12[@]}" up -d jellyfin12; dev/setup-server.sh "$JELLYFIN12_URL" ;;
  12-down)  "${COMPOSE12[@]}" stop jellyfin12 ;;
  12-reset)
    "${COMPOSE12[@]}" stop jellyfin12
    "${COMPOSE12[@]}" rm -f jellyfin12
    # Compose's volume labels target exactly these two volumes whatever the project prefix is.
    docker volume rm -f \
      $(docker volume ls -q --filter label=com.docker.compose.volume=jellyfin12-config) \
      $(docker volume ls -q --filter label=com.docker.compose.volume=jellyfin12-cache)
    "${COMPOSE12[@]}" up -d jellyfin12
    dev/setup-server.sh "$JELLYFIN12_URL" ;;
  12-logs)  "${COMPOSE12[@]}" logs -f jellyfin12 ;;
  *) sed -n '3,15p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
