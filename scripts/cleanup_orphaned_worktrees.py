#!/usr/bin/env python3
"""
Cleanup orphaned worktrees: remove stale directories for coordinator runs that no longer
exist in the database. Run after major controller upsets or database cleanups.

Usage:
  python3 cleanup_orphaned_worktrees.py [--dry-run] [--force]

Flags:
  --dry-run    Show what would be deleted without deleting
  --force      Actually delete (without --force, runs in dry-run mode)
"""

import os
import sys
import shutil
import sqlite3
import json
from pathlib import Path
from datetime import datetime

AISH_HOME = Path.home() / '.aish'
WORKTREES_DIR = AISH_HOME / 'worktrees'
AISH_DB = AISH_HOME / 'aish.db'

def load_active_runs():
    """Load all coordinator_runs IDs from database."""
    if not AISH_DB.exists():
        return set()
    
    try:
        conn = sqlite3.connect(AISH_DB)
        cur = conn.cursor()
        cur.execute("SELECT id FROM coordinator_runs")
        active_ids = {row[0] for row in cur.fetchall()}
        conn.close()
        return active_ids
    except Exception as e:
        print(f"Error reading database: {e}", file=sys.stderr)
        return set()

def find_orphaned_worktrees():
    """Find worktree directories that don't have corresponding DB entries."""
    if not WORKTREES_DIR.exists():
        return []
    
    active_ids = load_active_runs()
    orphaned = []
    
    for item in WORKTREES_DIR.iterdir():
        if item.is_dir():
            wt_id = item.name
            # Worktree dir pattern: LightHeart-Ventures--repo/w_XXXXX
            # or just w_XXXXX
            dir_parts = wt_id.split('/')
            coordinator_id = dir_parts[-1] if '/' in wt_id else wt_id
            
            if coordinator_id not in active_ids:
                orphaned.append(item)
    
    return sorted(orphaned)

def cleanup_worktrees(dry_run=True):
    """Remove orphaned worktree directories."""
    orphaned = find_orphaned_worktrees()
    
    if not orphaned:
        print("✓ No orphaned worktrees found.")
        return 0
    
    print(f"Found {len(orphaned)} orphaned worktree(s):\n")
    
    deleted_count = 0
    for wt_path in orphaned:
        size_kb = sum(
            f.stat().st_size for f in wt_path.rglob('*') if f.is_file()
        ) // 1024
        
        status = "[DRY-RUN]" if dry_run else "[DELETE]"
        print(f"{status} {wt_path.relative_to(AISH_HOME.parent)} (~{size_kb}KB)")
        
        if not dry_run:
            try:
                shutil.rmtree(wt_path, ignore_errors=True)
                deleted_count += 1
            except Exception as e:
                print(f"  Error: {e}", file=sys.stderr)
    
    print()
    if dry_run:
        print(f"Would delete {len(orphaned)} worktree(s) ({sum(f.stat().st_size for wt in orphaned for f in wt.rglob('*') if f.is_file()) // 1024 // 1024}MB)")
        print("Run with --force to actually delete.")
    else:
        print(f"✓ Deleted {deleted_count} orphaned worktree(s).")
    
    return deleted_count

def main():
    dry_run = '--force' not in sys.argv
    
    if '--help' in sys.argv or '-h' in sys.argv:
        print(__doc__)
        return 0
    
    try:
        cleanup_worktrees(dry_run=dry_run)
        return 0
    except Exception as e:
        print(f"Fatal error: {e}", file=sys.stderr)
        return 1

if __name__ == '__main__':
    sys.exit(main())
