"""Render the mihomo config template by substituting `${VAR}` tokens from the
environment (with `${VAR:-default}` fallback support).

Run::

    python3 render_config.py --template config/config.yaml.template \
        --out /etc/mihomo/config.yaml

The renderer is intentionally dependency-free so it runs in the Alpine image
that already ships ``python3`` for the crawler.
"""
from __future__ import annotations

import argparse
import os
import re
import sys

_TOKEN = re.compile(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}")

DEFAULTS = {
    "MIXED_PORT": "7890",
    "API_PORT": "9090",
    "CONTROLLER_HOST": "127.0.0.1",
    "CONTROLLER_PORT": "19090",
    "ALLOW_LAN": "true",
    "LOG_LEVEL": "info",
    "SECRET": "freebox",
    "EXTERNAL_UI": "/etc/mihomo/ui",
    "PROVIDERS_DIR": "./providers",
    "RULESET_BASE_URL": "https://cdn.metacubex.com/RuleSet",
    "RULESET_GEO_URL": "https://cdn.metacubex.com",
}


def render(text: str, env: dict) -> str:
    def sub(m: re.Match) -> str:
        name, default = m.group(1), m.group(2)
        return os.environ.get(name, env.get(name, default if default is not None else ""))

    return _TOKEN.sub(sub, text)


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description="Render the mihomo config template")
    ap.add_argument("--template", default=os.environ.get("CONFIG_TEMPLATE", "/app/config/config.yaml.template"))
    ap.add_argument("--out", default=None, help="write to file instead of stdout")
    args = ap.parse_args(argv)

    with open(args.template, "r", encoding="utf-8") as fh:
        rendered = render(fh.read(), DEFAULTS)

    if args.out:
        os.makedirs(os.path.dirname(os.path.abspath(args.out)), exist_ok=True)
        with open(args.out, "w", encoding="utf-8") as fh:
            fh.write(rendered)
        print(f"[config] rendered {args.template} -> {args.out}", file=sys.stderr)
    else:
        sys.stdout.write(rendered)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
