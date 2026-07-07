#!/usr/bin/env python3
"""badge.py — turn cclimits.sh --json into a single SecondStatusLine line.

Reads the ccquota JSON on stdin and prints ONE ready-to-render line (the plugin
owns its ANSI color, per aish's file-backed statusline contract, TASK-316):

    <color>⚡cc 63%w ·142%<reset>

Picks the single most-constraining window: highest pace when any window reports
pace, else highest percent. Color escalates dim→yellow→red with pressure. Prints
nothing (empty) on unusable input so refresh.sh leaves the prior badge to age.
"""
import json
import sys

DIM = "\033[2m"
YELLOW = "\033[33m"
RED = "\033[31m"
RESET = "\033[0m"


def short_window(key: str) -> str:
    k = key.lower()
    if "week" in k:
        return "w"
    if "month" in k:
        return "mo"
    if "session" in k:
        return "s"
    if "opus" in k:
        return "op"
    return ""


def main() -> int:
    raw = sys.stdin.read().strip()
    if not raw:
        return 0
    try:
        data = json.loads(raw)
    except (ValueError, TypeError):
        return 0
    if not isinstance(data, dict) or not data:
        return 0

    best = None  # (sort_key, percent, pace, window_short)
    for key, v in data.items():
        if not isinstance(v, dict):
            continue
        pct = v.get("percent")
        if not isinstance(pct, (int, float)):
            continue
        pace = v.get("pace") if isinstance(v.get("pace"), (int, float)) else None
        # Rank by pace when present (burn rate matters most), else by percent.
        rank = (pace if pace is not None else -1, pct)
        cand = (rank, int(pct), pace, short_window(key))
        if best is None or cand[0] > best[0]:
            best = cand

    if best is None:
        return 0

    _, pct, pace, win = best
    label = f"⚡cc {pct}%{win}"
    if pace is not None:
        label += f" \u00b7{int(pace)}%"

    # Color by the sharper of the two pressures.
    pace_v = pace if pace is not None else 0
    if pct >= 95 or pace_v >= 140:
        color = RED
    elif pct >= 80 or pace_v >= 115:
        color = YELLOW
    else:
        color = DIM

    sys.stdout.write(f"{color}{label}{RESET}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
