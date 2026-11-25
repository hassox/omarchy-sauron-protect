"""Sauron: a UniFi Protect bridge for the Omarchy bar."""

import os
import sys

VERSION = "0.3.0"
# The JSON-lines protocol spoken with Service.qml; bump it when messages change incompatibly.
PROTOCOL = 2

# websockets and h11 ship inside the plugin (see _vendor/README.md), ahead of any system copy.
_VENDOR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "_vendor")
if _VENDOR not in sys.path:
    sys.path.insert(0, _VENDOR)
