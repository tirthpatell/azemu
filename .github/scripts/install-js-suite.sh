#!/usr/bin/env bash
# Installs the npm packages of the JavaScript SDK suite (tests/sdk-js) exactly as the repository records them.
#
#   .github/scripts/install-js-suite.sh
#
# With package-lock.json: `npm ci` (the exact tree of the lock file). With only package.json: `npm install` (the exact versions
# that package.json names; its dependencies are pinned without ranges). With neither the script stops with a message, because
# installing "the latest of everything" would test against different SDK versions than the suite was written for.
set -euo pipefail
cd "$(dirname "$0")/../../tests/sdk-js"
if [ -f package-lock.json ]; then
    npm ci --no-audit --no-fund
elif [ -f package.json ]; then
    npm install --no-audit --no-fund
else
    echo "tests/sdk-js has no package.json and no package-lock.json in this checkout." >&2
    echo "Add them (tests/sdk-js/.gitignore must not ignore them): they pin the @azure/* versions the suite was written for." >&2
    exit 1
fi
npm ls --depth=0 | head -40
