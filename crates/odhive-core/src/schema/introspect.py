# Prints the odoo-bin server options as JSON. Run by OdHive with the project's
# own Python: python -I -B introspect.py <odoo-source-dir>
# Reads option metadata only; it never starts a server or touches a database.
import json
import optparse
import sys

source = sys.argv[1]
sys.argv = ["odoo-bin"]
sys.path.insert(0, source)

from odoo import release  # noqa: E402
from odoo.tools import config  # noqa: E402


def dump(option, group):
    help_text = option.help
    if help_text == optparse.SUPPRESS_HELP:
        help_text = None
    return {
        "dest": option.dest,
        "flags": option._short_opts + option._long_opts,
        "type": option.type,
        "action": option.action,
        "default": getattr(option, "my_default", None),
        "help": help_text,
        "metavar": option.metavar,
        "choices": list(option.choices) if option.choices else None,
        "group": group,
        "cli_loadable": getattr(option, "cli_loadable", True),
        "file_loadable": getattr(option, "file_loadable", True),
        "file_exportable": getattr(option, "file_exportable", True),
    }


parser = config.parser
options = [dump(o, None) for o in parser.option_list if o.dest]
for group in parser.option_groups:
    options += [dump(o, group.title) for o in group.option_list if o.dest]

json.dump(
    {"format": 1, "series": release.series, "options": options},
    sys.stdout,
    default=repr,
    indent=1,
    sort_keys=True,
)
sys.stdout.write("\n")
