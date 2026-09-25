# Sourced by the repro scripts. TM = binary under test, SP = scratch dir.
set -euo pipefail
TM="${TM:-turbomerger}"
SP="${SP:?set SP to a scratch directory}"
mkdir -p "$SP"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
