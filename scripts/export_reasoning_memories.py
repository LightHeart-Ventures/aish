#!/usr/bin/env python3
"""
FIX for audit issue #1: Reasoning events not persisted to memory.

Problem (audit finding):
- 63 reasoning events logged to telemetry
- 0 memories created from those events
- Escalation rate 85.7% unfeedback'd — every decision pays full cost

Root cause: reasoning_note() tool in aish framework is logging events but NOT
actually persisting them to the durable memory store. The telemetry write works,
but the memory write (the tool's primary job) fails silently.

This script extracts all completed reasoning events from telemetry and
outputs them in a format suitable for bulk import into durable memory.

Usage:
  python3 scripts/export_reasoning_memories.py > reasoning_memories.jsonl
  # Then import into memory system (how depends on backend)

Output format: one JSON line per event, containing the durable memory representation
"""

import json
import sys
from pathlib import Path

AISH_HOME = Path.home() / '.aish'
TELEMETRY_LOG = AISH_HOME / 'reasoning-telemetry.jsonl'

def load_telemetry():
    """Load reasoning telemetry."""
    events = []
    if not TELEMETRY_LOG.exists():
        print("No telemetry found", file=sys.stderr)
        return events
    
    with open(TELEMETRY_LOG) as f:
        for line in f:
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    return events

def event_to_memory(event):
    """Convert reasoning event to durable memory format."""
    if event.get('outcome') in [None, 'pending']:
        return None  # Skip unclosed events
    
    memory_id = f"reasoning_{event['id']}"
    decision = event.get('decision', 'unknown')
    topic = event.get('topic', '')
    outcome = event.get('outcome', 'unknown')
    
    # Build content
    lines = [
        f"Reasoning decision: {decision.upper()}",
        f"Topic: {topic}",
    ]
    
    if event.get('rationale'):
        lines.append(f"Rationale: {event['rationale']}")
    
    lines.append(f"Outcome: {outcome}")
    
    if event.get('complexity'):
        lines.append(f"Complexity: {event['complexity']}")
    
    content = '\n'.join(lines)
    
    # Build tags
    tags = [decision, decision + '_' + outcome]
    if event.get('complexity'):
        tags.append(event['complexity'])
    if event.get('ambiguity'):
        tags.append('ambiguity_' + event['ambiguity'])
    if decision == 'escalated':
        tags.append('escalation')
    
    return {
        'id': memory_id,
        'content': content,
        'tags': ','.join(tags),
        'timestamp': event.get('ts'),
        'decision': decision,
        'outcome': outcome
    }

def main():
    events = load_telemetry()
    closed = [e for e in events if e.get('outcome') not in [None, 'pending']]
    
    print(f"# Reasoning memories for bulk import", file=sys.stderr)
    print(f"# Total events: {len(events)}, Closed: {len(closed)}", file=sys.stderr)
    
    exported = 0
    for event in closed:
        memory = event_to_memory(event)
        if memory:
            print(json.dumps(memory))
            exported += 1
    
    print(f"# Exported {exported} memories", file=sys.stderr)
    return 0

if __name__ == '__main__':
    sys.exit(main())
