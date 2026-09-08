#!/bin/sh
# Boot only the supported OCI entry point in a scratch home. The Python runner
# owns cleanup, real readiness/registry checks and the loopback fake-provider turn.
set -eu
SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
exec python3 "$SCRIPT_DIR/oci-smoke.py" "$@"
