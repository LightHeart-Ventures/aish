#!/usr/bin/env python3
"""
Comprehensive aish audit tool with LLM-powered analysis.
Scans SQLite databases, reasoning telemetry, memories, and turn history.
Uses Claude to evaluate patterns and generate intelligent recommendations.
"""

import sqlite3
import json
import os
import sys
from pathlib import Path
from collections import defaultdict, Counter
from datetime import datetime
import anthropic

AISH_HOME = Path.home() / ".aish"
AISH_DB = AISH_HOME / "database" / "aish.db"
PLUGINS_DB = AISH_HOME / "database" / "plugins.db"
TELEMETRY_LOG = AISH_HOME / "reasoning-telemetry.jsonl"

class AishAudit:
    def __init__(self):
        self.findings = []
        self.stats = {}
        self.memories = []
        self.history = []
        self.telemetry = []
        # Initialize Anthropic client with API key from environment
        api_key = os.getenv("ANTHROPIC_API_KEY")
        if not api_key:
            self.log("llm", "ANTHROPIC_API_KEY not set — LLM analysis will be skipped", "warning")
            self.client = None
        else:
            try:
                client_kwargs = {"api_key": api_key}
                workspace_id = os.getenv("ANTHROPIC_WORKSPACE_ID")
                if workspace_id:
                    client_kwargs["default_headers"] = {"anthropic-workspace-id": workspace_id}
                self.client = anthropic.Anthropic(**client_kwargs)
                # Test the client with a cheap models call to verify auth
                try:
                    self.client.models.list()
                except anthropic.BadRequestError as e:
                    if "workspace" in str(e).lower():
                        # Try again without workspace header
                        self.log("llm", "Retrying without workspace header...", "info")
                        self.client = anthropic.Anthropic(api_key=api_key)
                        self.client.models.list()  # Verify it works
            except anthropic.AuthenticationError as e:
                self.log("llm", f"Authentication failed: {e} — LLM analysis will be skipped", "warning")
                self.client = None
        
    def log(self, category, message, severity="info"):
        """Log a finding with category, message, and severity level."""
        self.findings.append({
            "category": category,
            "message": message,
            "severity": severity
        })
        
    def connect_db(self, db_path):
        """Connect to a database, return cursor or None."""
        try:
            conn = sqlite3.connect(db_path)
            conn.row_factory = sqlite3.Row
            return conn
        except Exception as e:
            self.log("database", f"Failed to connect to {db_path}: {e}", "error")
            return None
    
    def audit_aish_db(self):
        """Audit the main aish.db database."""
        print("[*] Auditing aish.db...")
        conn = self.connect_db(AISH_DB)
        if not conn:
            return
        
        cur = conn.cursor()
        
        # Get all tables
        cur.execute("SELECT name FROM sqlite_master WHERE type='table';")
        tables = [row[0] for row in cur.fetchall()]
        self.log("database", f"Found {len(tables)} tables in aish.db", "info")
        
        for table in tables:
            try:
                # Get row count and size
                cur.execute(f"SELECT COUNT(*) FROM {table}")
                row_count = cur.fetchone()[0]
                
                cur.execute(f"SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size();")
                try:
                    size = cur.fetchone()[0]
                except:
                    size = 0
                
                self.log("database", f"  Table '{table}': {row_count} rows, ~{size/1024:.1f}KB", "info")
                
                # Table-specific audits
                if "coordinator" in table.lower() or "job" in table.lower():
                    self._audit_coordinator_table(cur, table)
                elif "tool" in table.lower() or "call" in table.lower():
                    self._audit_tool_table(cur, table)
                elif "memory" in table.lower():
                    self._audit_memory_table(cur, table)
                elif "history" in table.lower():
                    self._load_history(cur, table)
            except sqlite3.OperationalError as e:
                if "no such module" in str(e):
                    self.log("database", f"  Table '{table}': skipped (requires extension)", "info")
                else:
                    self.log("database", f"  Table '{table}': error - {e}", "warning")
        
        conn.close()
    
    def _load_history(self, cur, table):
        """Load turn history for LLM analysis."""
        try:
            cur.execute(f"SELECT * FROM {table} ORDER BY rowid DESC LIMIT 100")
            rows = cur.fetchall()
            for row in rows:
                self.history.append(dict(row))
        except:
            pass
    
    def _audit_coordinator_table(self, cur, table):
        """Audit coordinator/job tables for performance patterns."""
        try:
            # Different tables have different schemas
            if table == "batch_jobs":
                # batch_jobs: local_id, anthropic_id, task, model, status, result, error, created_at, session_id, session_name
                # No duration_ms, just created_at timestamp
                cur.execute(f"""
                    SELECT COUNT(*) FROM {table} 
                    WHERE status IN ('running', 'pending', 'queued')
                """)
                active = cur.fetchone()[0]
                if active > 0:
                    self.log("coordinator", f"  {active} batch jobs still in active state (may be stalled)", "warning")
                # Can't compute duration without end time; skip
                
            elif table == "coordinator_runs":
                # coordinator_runs: run_id, task, phase, result, error, session_id, session_name, created_at, heartbeat_at, stand_down, checkpoint
                # Phase values: 'coordinating', 'awaiting_batch', 'checkpoint', 'done', 'failed'
                cur.execute(f"""
                    SELECT COUNT(*) FROM {table} 
                    WHERE phase IN ('coordinating', 'awaiting_batch', 'checkpoint')
                """)
                active = cur.fetchone()[0]
                if active > 0:
                    self.log("coordinator", f"  {active} coordinator runs still in active phase (may be stalled)", "warning")
                # Can't compute duration without explicit end time; skip
                
            elif table == "coordinator_registry":
                # coordinator_registry: coord_id, generation, pid, batch_job_id, phase, started_at, owner_session
                cur.execute(f"""
                    SELECT COUNT(*) FROM {table} 
                    WHERE phase IN ('coordinating', 'awaiting_batch')
                """)
                active = cur.fetchone()[0]
                if active > 0:
                    self.log("coordinator", f"  {active} coordinator processes still running", "info")
        except Exception as e:
            self.log("database", f"  Error auditing {table}: {e}", "warning")
    
    def _audit_tool_table(self, cur, table):
        """Audit tool call patterns."""
        try:
            # Skip allowed_tools — it's just a permission list, not telemetry
            if table == "allowed_tools":
                return
            
            # tool_telemetry: tool column, is_error flag
            # Most commonly called tools
            cur.execute(f"""
                SELECT tool, COUNT(*) as count
                FROM {table}
                GROUP BY tool
                ORDER BY count DESC
                LIMIT 10
            """)
            tools = cur.fetchall()
            if tools:
                self.log("tools", f"  Top 10 most-called tools:", "info")
                for tool, count in tools:
                    self.log("tools", f"    - {tool}: {count} calls", "info")
            
            # Failure rate (using is_error flag, not status column)
            cur.execute(f"""
                SELECT 
                  COUNT(*) as total,
                  SUM(CASE WHEN is_error = 1 THEN 1 ELSE 0 END) as failures
                FROM {table}
            """)
            row = cur.fetchone()
            if row and row[0] > 0:
                fail_rate = (row[1] or 0) / row[0] * 100
                if fail_rate > 5:
                    self.log("reliability", 
                        f"  Tool call failure rate: {fail_rate:.1f}% ({row[1]}/{row[0]})", "warning")
                else:
                    self.log("reliability", 
                        f"  Tool call failure rate: {fail_rate:.1f}%", "info")
        except Exception as e:
            self.log("database", f"  Error auditing {table}: {e}", "warning")
    
    def _audit_memory_table(self, cur, table):
        """Audit memory/context usage."""
        try:
            cur.execute(f"""
                SELECT 
                  COUNT(*) as total,
                  AVG(LENGTH(content)) as avg_size,
                  MAX(LENGTH(content)) as max_size
                FROM {table}
            """)
            row = cur.fetchone()
            if row and row[0] > 0:
                self.log("memory", 
                    f"  {row[0]} memories stored, avg size: {row[1]:.0f}B, max: {row[2]:.0f}B", "info")
                if row[2] > 100000:
                    self.log("memory", 
                        f"  Some memories > 100KB; consider compression or archival", "warning")
            
            # Load memories for LLM analysis
            cur.execute(f"SELECT content, tags FROM {table} ORDER BY rowid DESC LIMIT 50")
            for row in cur.fetchall():
                self.memories.append({
                    "content": row[0],
                    "tags": row[1] if row[1] else ""
                })
        except Exception as e:
            self.log("database", f"  Error auditing {table}: {e}", "warning")
    
    def audit_plugins_db(self):
        """Audit the plugins.db database."""
        print("[*] Auditing plugins.db...")
        conn = self.connect_db(PLUGINS_DB)
        if not conn:
            return
        
        cur = conn.cursor()
        
        # Get all tables
        cur.execute("SELECT name FROM sqlite_master WHERE type='table';")
        tables = [row[0] for row in cur.fetchall()]
        
        for table in tables:
            cur.execute(f"SELECT COUNT(*) FROM {table}")
            row_count = cur.fetchone()[0]
            self.log("database", f"  Table '{table}': {row_count} rows", "info")
        
        conn.close()
    
    def audit_telemetry(self):
        """Audit reasoning telemetry log."""
        print("[*] Auditing reasoning telemetry...")
        if not TELEMETRY_LOG.exists():
            self.log("telemetry", "No reasoning-telemetry.jsonl found", "warning")
            return
        
        try:
            events = []
            with open(TELEMETRY_LOG) as f:
                for line in f:
                    try:
                        events.append(json.loads(line))
                    except:
                        pass
            
            self.telemetry = events
            self.log("telemetry", f"Parsed {len(events)} reasoning events", "info")
            
            if events:
                # Decision patterns
                decisions = Counter(e.get("decision") for e in events if "decision" in e)
                if decisions:
                    self.log("reasoning", "Decision distribution:", "info")
                    for decision, count in decisions.most_common():
                        pct = count / len(events) * 100
                        self.log("reasoning", f"  {decision}: {count} ({pct:.1f}%)", "info")
                
                # Outcome analysis (for closed events)
                outcomes = [e for e in events if "outcome" in e]
                if outcomes:
                    success = sum(1 for e in outcomes if e.get("outcome") == "correct")
                    fail = sum(1 for e in outcomes if e.get("outcome") == "wrong_turn")
                    if success + fail > 0:
                        acc = success / (success + fail) * 100
                        self.log("reasoning", 
                            f"Closed reasoning events: {acc:.1f}% correct ({success}/{success+fail})", 
                            "warning" if acc < 80 else "info")
                
                # Complexity vs escalation
                escalated = sum(1 for e in events if e.get("decision") == "escalated")
                guessed = sum(1 for e in events if e.get("decision") == "guessed")
                if escalated + guessed > 0:
                    self.log("reasoning", 
                        f"Escalation rate: {escalated/(escalated+guessed)*100:.1f}%", "info")
        except Exception as e:
            self.log("telemetry", f"Error parsing telemetry: {e}", "warning")
    
    def audit_filesystem(self, skip_large_dirs=False):
        """Audit aish filesystem usage."""
        print("[*] Auditing filesystem...")
        
        if not AISH_HOME.exists():
            self.log("filesystem", f"{AISH_HOME} not found", "warning")
            return
        
        # Quick estimate: check major directories only
        if skip_large_dirs:
            self.log("filesystem", "Skipping filesystem audit (--skip-fs flag set)", "info")
            return
        
        # Use du -sh for accurate, fast total (avoids file-by-file walk)
        try:
            import subprocess
            result = subprocess.run(
                ["du", "-sh", str(AISH_HOME)],
                capture_output=True,
                text=True,
                timeout=10
            )
            if result.returncode == 0:
                size_str = result.stdout.split()[0]
                self.log("filesystem", f"Total size: {size_str} (using du -sh for accuracy)", "info")
                return
        except Exception:
            pass
        
        # Fallback: scan up to 10K files with early exit
        total_size = 0
        count = 0
        aborted = False
        for root, dirs, files in os.walk(AISH_HOME, onerror=lambda e: None):
            # Skip worktrees for speed
            dirs[:] = [d for d in dirs if d not in ['worktrees']]
            for f in files:
                path = os.path.join(root, f)
                try:
                    total_size += os.path.getsize(path)
                    count += 1
                except:
                    pass
                # Hard limit: stop and report via du
                if count > 10000:
                    aborted = True
                    break
            if aborted:
                break
        
        if aborted:
            self.log("filesystem", 
                f"Scan limit exceeded ({count}+ files); use `du -sh {AISH_HOME}` for exact total", 
                "info")
        else:
            self.log("filesystem", 
                f"Scanned {count} files, total: {total_size/1024/1024:.1f}MB (excluding worktrees)", 
                "info")
    
    def get_llm_analysis(self):
        """Use Claude to analyze telemetry, memories, and history."""
        if not self.client:
            return "(LLM analysis skipped — ANTHROPIC_API_KEY not set)"
        
        print("\n[*] Generating LLM-powered analysis (this may take 10-15 seconds)...")
        
        # Prepare context for LLM
        escalation_count = sum(1 for e in self.telemetry if e.get("decision") == "escalated")
        escalation_rate = (escalation_count / len(self.telemetry) * 100) if self.telemetry else 0
        
        prompt = f"""You are analyzing aish (an AI shell) audit data. Provide a concise, expert assessment.

**Telemetry Summary:**
- Total reasoning events: {len(self.telemetry)}
- Escalation rate: {escalation_rate:.1f}%
- Recent decisions: {json.dumps([e.get('decision') for e in self.telemetry[-20:] if 'decision' in e])}

**Stored Memories Count:** {len(self.memories)} (most recent 20):
{json.dumps([m['content'][:150] + '...' if len(m['content']) > 150 else m['content'] for m in self.memories[:20]], indent=2)}

**Critical Findings:**
{json.dumps([f for f in self.findings if f['severity'] in ['error', 'warning']], indent=2)}

**Your Task (be concise and direct):**
1. **Pattern Recognition**: What patterns in decision-making and escalation do you see?
2. **Cost/Latency Insights**: What specific optimizations are valuable?
3. **Top 3 Actionable Improvements**: Rank by impact.

Focus on concrete, measurable recommendations."""

        try:
            response = self.client.messages.create(
                model="claude-opus-5",
                max_tokens=1200,
                messages=[
                    {
                        "role": "user",
                        "content": prompt
                    }
                ]
            )
            # Handle thinking blocks (skip them, find the text block)
            for block in response.content:
                if hasattr(block, 'text'):
                    return block.text
            return None
        except anthropic.BadRequestError as e:
            if "workspace" in str(e).lower():
                self.log("llm", f"Authentication failed: identity-linked API key requires ANTHROPIC_WORKSPACE_ID env var", "warning")
                return "(LLM analysis skipped — identity-linked API key requires workspace ID)"
            else:
                print(f"DEBUG: LLM error: {type(e).__name__}: {e}", file=sys.stderr)
                self.log("llm", f"Failed to get LLM analysis: {type(e).__name__}: {str(e)[:100]}", "error")
                return None
        except Exception as e:
            print(f"DEBUG: LLM error: {type(e).__name__}: {e}", file=sys.stderr)
            self.log("llm", f"Failed to get LLM analysis: {type(e).__name__}: {str(e)[:100]}", "error")
            return None
    
    def print_report(self):
        """Print formatted audit report with LLM recommendations."""
        print("\n" + "="*80)
        print("AISH COMPREHENSIVE AUDIT REPORT")
        print("="*80 + "\n")
        
        # Group findings by category
        by_category = defaultdict(list)
        for finding in self.findings:
            by_category[finding["category"]].append(finding)
        
        # Print by severity
        for severity in ["error", "warning", "info"]:
            items = [f for f in self.findings if f["severity"] == severity]
            if items:
                icon = "❌" if severity == "error" else "⚠️" if severity == "warning" else "ℹ️"
                print(f"\n{icon} {severity.upper()}")
                print("-" * 80)
                for item in items:
                    print(f"  [{item['category']}] {item['message']}")
        
        print("\n" + "="*80)
        print("LLM-POWERED ANALYSIS & RECOMMENDATIONS")
        print("="*80)
        
        llm_rec = self.get_llm_analysis()
        if llm_rec:
            print(llm_rec)
        else:
            print("(LLM analysis unavailable — see error above)")
        
        print("\n" + "="*80)
    
    def run(self, skip_fs=False):
        """Run full audit."""
        print("\n🔍 Starting comprehensive aish audit with LLM analysis...\n")
        self.audit_aish_db()
        self.audit_plugins_db()
        self.audit_telemetry()
        self.audit_filesystem(skip_large_dirs=skip_fs)
        self.print_report()

if __name__ == "__main__":
    skip_fs = "--skip-fs" in sys.argv
    audit = AishAudit()
    audit.run(skip_fs=skip_fs)
