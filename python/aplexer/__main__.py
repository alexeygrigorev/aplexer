"""``python -m aplexer worker --id <uuid>`` — in-process worker, no extra binary."""

from __future__ import annotations

import argparse
import sys


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="aplexer")
    sub = parser.add_subparsers(dest="cmd", required=True)
    worker = sub.add_parser("worker")
    worker.add_argument("--id", required=True)
    args = parser.parse_args(argv)
    if args.cmd == "worker":
        from aplexer import _native

        _native.run_worker(args.id)
        return 0
    return 2


if __name__ == "__main__":
    sys.exit(main())
