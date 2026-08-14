#!/usr/bin/env python3
"""Keep an OMP-named foreground process around the sandbox launcher."""

import ctypes
import subprocess
import sys


libc = ctypes.CDLL(None, use_errno=True)
if libc.prctl(15, b"omp", 0, 0, 0) != 0:
    raise OSError(ctypes.get_errno(), "prctl(PR_SET_NAME) failed")

child = subprocess.Popen(sys.argv[1:])
try:
    exit_code = child.wait()
except KeyboardInterrupt:
    exit_code = child.wait()
raise SystemExit(exit_code)
