# Prints facts about the running interpreter as one JSON line.
# Run by OdHive as `python -I -B -c <this>`; must work on any Python 3 and import
# nothing outside the standard library.
import json
import sys
import sysconfig

print(json.dumps({
    "format": 1,
    "version": list(sys.version_info[:3]),
    "executable": sys.executable,
    "implementation": sys.implementation.name,
    "free_threaded": bool(sysconfig.get_config_var("Py_GIL_DISABLED")),
    "venv": sys.prefix != getattr(sys, "base_prefix", sys.prefix),
}))
