#!/usr/bin/env bash
# Fails unless the app's Cargo version and the newest <release> in the
# metainfo agree, and, when a version or tag is given (v1.2.3 or 1.2.3), unless
# both also equal it.
#
#   ci/check-version.sh            # Cargo.toml and metainfo agree
#   ci/check-version.sh v0.1.0     # ...and both are 0.1.0
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cargo=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/crates/app/Cargo.toml" | head -n1)
metainfo=$(grep -o '<release version="[^"]*"' \
  "$root/packaging/linux/io.github.alisayyah.HardcoverDesktop.metainfo.xml" |
  head -n1 | cut -d'"' -f2)
want=${1:-$cargo}
want=${want#v}

status=0
if [[ $cargo != "$want" ]]; then
  echo "::error::crates/app/Cargo.toml has version $cargo, expected $want"
  status=1
fi
if [[ $metainfo != "$want" ]]; then
  echo "::error::the newest <release> in the metainfo is $metainfo, expected $want"
  status=1
fi
exit $status
