#!/usr/bin/env bash
# Download a branch build of ecash-com/bitcoin from releases.ecash.com,
# verify its build provenance, and install bitcoind/bitcoin-cli/bitcoin-util.
#
# Usage: fetch_ecash_bitcoin.sh CHANNEL TARGET DEST_DIR
#
# Idempotent: DEST_DIR records the commit it holds, and a re-run that finds
# the channel still pointing at that commit downloads nothing.
#
# The server keeps an immutable L1-ecash-bitcoin/<channel>/<commit>/ per
# build (zips, SHA256SUMS, release.json, provenance.jsonl) and a moving
# <channel>/latest/ copy of it. Only release.json is read through latest/;
# the rest comes from the immutable path it names.
#
# provenance.jsonl is the Sigstore bundle from actions/attest-build-provenance.
# `gh attestation verify --bundle` needs no GitHub login: it checks that the
# file's digest is attested by ecash-com/bitcoin's build-release workflow,
# run on the channel branch at the commit release.json claims.

set -euo pipefail

ECASH_RELEASES="${ECASH_RELEASES:-https://releases.ecash.com}"
ECASH_PROJECT='L1-ecash-bitcoin'
ECASH_REPO='ecash-com/bitcoin'

[ $# -eq 3 ] || { echo "Usage: $(basename "$0") CHANNEL TARGET DEST_DIR" >&2; exit 2; }
CHANNEL="$1"
TARGET="$2"
DEST_DIR="$3"
case "$CHANNEL" in
    *[!A-Za-z0-9._-]* | '' | .*) echo "invalid channel '$CHANNEL'" >&2; exit 2 ;;
esac

for tool in curl jq unzip gh; do
    command -v "$tool" >/dev/null || { echo "'$tool' is required" >&2; exit 1; }
done
if ! gh attestation verify --help 2>/dev/null | grep -q -- '--source-digest'; then
    echo "gh $(gh --version | head -1) is too old for the attestation flags used here; upgrade gh" >&2
    exit 1
fi

CHANNEL_URL="$ECASH_RELEASES/$ECASH_PROJECT/$CHANNEL"
ZIP_NAME="$ECASH_PROJECT-$TARGET.zip"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

if ! curl -fsSL "$CHANNEL_URL/latest/release.json" -o "$TMP/release.json"; then
    echo "could not fetch $CHANNEL_URL/latest/release.json" >&2
    exit 1
fi
COMMIT="$(jq -er '.commit' "$TMP/release.json")"
RELEASE_PATH="$(jq -er '.path' "$TMP/release.json")"
VERSION="$(jq -er '.version' "$TMP/release.json")"
EXPECTED_SHA256="$(jq -er --arg name "$ZIP_NAME" \
    '.files[] | select(.name == $name) | .sha256' "$TMP/release.json")" || {
    echo "$CHANNEL@${COMMIT:0:12} publishes no $ZIP_NAME (target '$TARGET')" >&2
    exit 1
}
case "$COMMIT" in
    *[!0-9a-f]* | '') echo "release.json: bad commit '$COMMIT'" >&2; exit 1 ;;
esac
case "$RELEASE_PATH" in
    "$ECASH_PROJECT/$CHANNEL/"*) ;;
    *) echo "release.json: path '$RELEASE_PATH' is outside $ECASH_PROJECT/$CHANNEL/" >&2; exit 1 ;;
esac
RELEASE_URL="$ECASH_RELEASES/${RELEASE_PATH%/}"

if [ -x "$DEST_DIR/bitcoind" ] &&
    [ "$(cat "$DEST_DIR/.release-commit" 2>/dev/null || true)" = "$COMMIT" ]; then
    echo "ecash bitcoin $CHANNEL: cached ($VERSION @ ${COMMIT:0:12})"
    exit 0
fi

echo "Downloading ecash bitcoin $CHANNEL $VERSION @ ${COMMIT:0:12} ($TARGET)..."
curl -fsSL "$RELEASE_URL/provenance.jsonl" -o "$TMP/provenance.jsonl"
curl -# -fL "$RELEASE_URL/$ZIP_NAME" -o "$TMP/$ZIP_NAME"

verify() {
    gh attestation verify "$1" \
        --bundle "$TMP/provenance.jsonl" \
        --repo "$ECASH_REPO" \
        --signer-workflow "$ECASH_REPO/.github/workflows/build-release.yml" \
        --source-ref "refs/heads/$CHANNEL" \
        --source-digest "$COMMIT" \
        --deny-self-hosted-runners
}
sha256() {
    if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1
}

# release.json is attested too, so once it passes every field above is trusted.
echo "Verifying build provenance..."
verify "$TMP/release.json" || { echo "release.json failed provenance verification" >&2; exit 1; }
ACTUAL_SHA256="$(sha256 "$TMP/$ZIP_NAME")"
if [ "$ACTUAL_SHA256" != "$EXPECTED_SHA256" ]; then
    echo "$ZIP_NAME: sha256 $ACTUAL_SHA256 does not match release.json ($EXPECTED_SHA256)" >&2
    exit 1
fi
verify "$TMP/$ZIP_NAME" || { echo "$ZIP_NAME failed provenance verification; not installing it" >&2; exit 1; }

# The zip holds one directory, L1-ecash-bitcoin-<version>-<commit>-<target>/.
mkdir "$TMP/unpacked"
unzip -q "$TMP/$ZIP_NAME" -d "$TMP/unpacked"
UNPACKED_DIRS=("$TMP/unpacked"/*/)
if [ ${#UNPACKED_DIRS[@]} -ne 1 ] || [ ! -d "${UNPACKED_DIRS[0]}" ]; then
    echo "$ZIP_NAME: expected exactly one top-level directory, got: $(ls "$TMP/unpacked")" >&2
    exit 1
fi
for bin in bitcoind bitcoin-cli bitcoin-util; do
    [ -f "${UNPACKED_DIRS[0]}$bin" ] || { echo "$ZIP_NAME: missing $bin" >&2; exit 1; }
done

rm -rf "$DEST_DIR"
mkdir -p "$(dirname "$DEST_DIR")"
mv "${UNPACKED_DIRS[0]%/}" "$DEST_DIR"
chmod +x "$DEST_DIR"/bitcoind "$DEST_DIR"/bitcoin-cli "$DEST_DIR"/bitcoin-util
printf '%s\n' "$COMMIT" > "$DEST_DIR/.release-commit"
echo "Installed ecash bitcoin $CHANNEL $VERSION @ ${COMMIT:0:12} to $DEST_DIR"
