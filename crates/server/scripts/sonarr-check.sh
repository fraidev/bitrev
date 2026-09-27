#!/bin/sh
# Talks to bitrev the way Sonarr's qBittorrent client does.
# Usage: BITREV_PASSWORD=secret crates/server/scripts/sonarr-check.sh
set -eu

base="${BITREV_URL:-http://127.0.0.1:8080}"
user="${BITREV_USER:-admin}"
category="${BITREV_CATEGORY:-tv-sonarr}"
magnet="${BITREV_MAGNET:-magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c&dn=Big%20Buck%20Bunny}"
hash="${BITREV_HASH:-dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c}"

if [ -z "${BITREV_PASSWORD:-}" ]; then
    echo "Set BITREV_PASSWORD to the bitrev serve password." >&2
    exit 1
fi

jar="$(mktemp)"
trap 'rm -f "$jar"' EXIT

login="$(curl -sS -c "$jar" -X POST "$base/api/v2/auth/login" \
    -H "Content-Type: application/x-www-form-urlencoded" \
    --data-urlencode "username=$user" \
    --data-urlencode "password=$BITREV_PASSWORD")"
if [ "$login" != "Ok." ]; then
    echo "login failed: $login" >&2
    exit 1
fi

version="$(curl -sS -b "$jar" "$base/api/v2/app/version")"
webapi="$(curl -sS -b "$jar" "$base/api/v2/app/webapiVersion")"
if [ "$version" != "v4.6.7" ] || [ "$webapi" != "2.9.3" ]; then
    echo "unexpected identity: version=$version webapi=$webapi" >&2
    exit 1
fi

curl -sS -b "$jar" "$base/api/v2/app/preferences" >/dev/null

add="$(curl -sS -b "$jar" -X POST "$base/api/v2/torrents/add" \
    -F "urls=$magnet" \
    -F "category=$category" \
    -F "paused=true")"
if [ "$add" != "Ok." ]; then
    echo "add failed: $add" >&2
    exit 1
fi

info="$(curl -sS -b "$jar" --get "$base/api/v2/torrents/info" \
    --data-urlencode "category=$category")"
case "$info" in
    *"\"hash\":\"$hash\""*"\"category\":\"$category\""*|*"\"category\":\"$category\""*"\"hash\":\"$hash\""*)
        echo "ok $hash category=$category"
        ;;
    *)
        echo "magnet $hash missing from category $category" >&2
        echo "$info" >&2
        exit 1
        ;;
esac
