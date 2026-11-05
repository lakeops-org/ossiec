#!/usr/bin/env bash
# Re-vendor Apache Ossie example models and compliance fixtures from apache/ossie main.
# Run, then `cargo test`: a spec change that breaks ossiec shows up as a failing test.
set -euo pipefail
ref="${1:-main}"
dest="$(cd "$(dirname "$0")/.." && pwd)/tests/fixtures/ossie"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
git clone --quiet --depth 1 --branch "$ref" https://github.com/apache/ossie "$tmp/ossie"
cp "$tmp"/ossie/examples/{tpcds_semantic_model,renamed_fields,flights.semantic_model,flights}.yaml "$dest/"
cp "$tmp"/ossie/compliance/portability/m*/*.ossie.yaml "$dest/"
rm -rf "$dest/query"
mkdir -p "$dest/query"
cp -R "$tmp"/ossie/compliance/query/{scott,cases} "$dest/query/"
echo "synced from apache/ossie@$(git -C "$tmp/ossie" rev-parse --short HEAD) into $dest"
