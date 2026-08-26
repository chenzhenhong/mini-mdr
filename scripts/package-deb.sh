#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

cargo deb --no-build --release
echo "deb package: target/debian/"
