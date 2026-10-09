#!/usr/bin/env bash
# Runs cargo test for the given crates on a Windows host over SSH (no RDP, no desktop).
# Usage: windows-test.sh <crate>... [-- <extra cargo test args>]
set -euo pipefail
host="${HCC_WINDOWS_HOST:?defina HCC_WINDOWS_HOST (alias ssh do Windows de build)}"
dest="hcc-build/$(basename "$(git rev-parse --show-toplevel)")"
psq() { local s=${1//\'/\'\'}; printf "'%s'" "$s"; }
args=()
while (($#)); do
  if [[ $1 == -- ]]; then shift; for a in "$@"; do args+=("$(psq "$a")"); done; break; fi
  args+=(-p "$(psq "$1")"); shift
done
ps="\$ProgressPreference = 'SilentlyContinue'; \$d =Join-Path \$env:USERPROFILE $(psq "$dest"); New-Item -ItemType Directory -Force \$d | Out-Null; Set-Location \$d; tar -xf -; if (\$LASTEXITCODE) { exit \$LASTEXITCODE }; cargo test ${args[*]}; exit \$LASTEXITCODE"
# -EncodedCommand reaches PowerShell intact whether the remote login shell is cmd or PowerShell.
enc=$(printf %s "$ps" | iconv -f UTF-8 -t UTF-16LE | base64 -w0)
git ls-files -co --exclude-standard -z | tar --null --ignore-failed-read -T - -cf - | \
  ssh -o BatchMode=yes "$host" "powershell -NoProfile -NonInteractive -EncodedCommand $enc"
