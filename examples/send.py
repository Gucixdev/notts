#!/usr/bin/env python3
"""Send one notification through the notts CLI. Usage: send.py APP SEVERITY TITLE [BODY]"""
import json
import subprocess
import sys


def notify(app, severity, title, body="", state_key=None, state=None, attach=None):
    cmd = ["notts", "send", "--app", app, "--severity", severity, "--title", title, "--json"]
    if state_key and state:
        cmd += ["--key", state_key, "--state", state]
    if attach:
        cmd += ["--attach", attach]
    # the body goes over stdin, so any length and any characters are safe
    r = subprocess.run(cmd, input=body.encode(), capture_output=True, timeout=300)
    # exit code: 0 delivered (or deliberately not sent), 1 failed, 2 bad usage/config, 3 queued
    return r.returncode, json.loads(r.stdout or b"{}")


if __name__ == "__main__":
    code, report = notify(*sys.argv[1:5])
    print(code, report.get("disposition"), [(c["name"], c["status"]) for c in report.get("results", [])])
    sys.exit(code)
