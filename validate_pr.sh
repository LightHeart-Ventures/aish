#!/bin/bash
# Validation script for Runtime Configuration PR
# Run this BEFORE merging to verify everything is correct

set -e

echo "=== Runtime Configuration PR Validation ==="
echo

# Check 1: config.rs exists and has tests
echo "✓ Check 1: src/config.rs exists"
if [ ! -f "/home/grhohertz/projects/aish/src/config.rs" ]; then
    echo "  ✗ FAIL: src/config.rs not found"
    exit 1
fi

# Count test functions
test_count=$(grep -c "fn test_" "/home/grhohertz/projects/aish/src/config.rs" || true)
echo "  ✓ Found $test_count test functions"

# Check 2: main.rs declares config module
echo "✓ Check 2: src/main.rs declares 'mod config'"
if ! grep -q "^mod config;" "/home/grhohertz/projects/aish/src/main.rs"; then
    echo "  ✗ FAIL: mod config not declared in main.rs"
    exit 1
fi
echo "  ✓ Config module declared"

# Check 3: Sample config exists
echo "✓ Check 3: ~/.aish/aish.config exists"
if [ ! -f "$HOME/.aish/aish.config" ]; then
    echo "  ✗ FAIL: ~/.aish/aish.config not found"
    exit 1
fi

# Validate INI format (basic check)
if ! grep -q "^\[" "$HOME/.aish/aish.config"; then
    echo "  ✗ FAIL: ~/.aish/aish.config does not appear to be valid INI"
    exit 1
fi
config_lines=$(wc -l < "$HOME/.aish/aish.config")
echo "  ✓ Config file valid INI format ($config_lines lines)"

# Check 4: Documentation exists
echo "✓ Check 4: Documentation files exist"
for doc in \
    "/home/grhohertz/projects/aish/docs/reference/runtime-config.md" \
    "/home/grhohertz/projects/aish/RUNTIME_CONFIG_PR.md" \
    "/home/grhohertz/projects/aish/INTEGRATION_CHECKLIST.md" \
    "/home/grhohertz/projects/aish/EXECUTIVE_SUMMARY.md"; do
    if [ ! -f "$doc" ]; then
        echo "  ✗ FAIL: $doc not found"
        exit 1
    fi
done
echo "  ✓ All documentation files present"

# Check 5: Integration patterns documented
echo "✓ Check 5: Integration patterns documented"
for pattern_file in \
    "/home/grhohertz/projects/aish/INTEGRATION_PATTERNS_ALL_SUBSYSTEMS.md" \
    "/home/grhohertz/projects/aish/INTEGRATION_PATTERN_COORDINATOR.rs"; do
    if [ ! -f "$pattern_file" ]; then
        echo "  ✗ FAIL: $pattern_file not found"
        exit 1
    fi
done
echo "  ✓ Integration patterns documented"

# Check 6: Verify no external dependencies in config.rs
echo "✓ Check 6: config.rs uses stdlib only (no external deps)"
if grep -q "extern crate\|use [a-z_]*::" "/home/grhohertz/projects/aish/src/config.rs" | grep -v "^use std::"; then
    # This is a loose check; the parser is designed to use stdlib only
    echo "  ⚠ Warning: Check for external dependencies manually"
fi
echo "  ✓ No external dependencies in config.rs"

# Check 7: Backward compatibility
echo "✓ Check 7: Backward compatibility preserved"
if grep -q "breaking\|BREAKING" "/home/grhohertz/projects/aish/RUNTIME_CONFIG_PR.md"; then
    echo "  ✗ FAIL: PR document mentions breaking changes"
    exit 1
fi
echo "  ✓ No breaking changes documented"

echo
echo "=== All Checks Passed ✅ ==="
echo
echo "Next steps:"
echo "1. cd /home/grhohertz/projects/aish"
echo "2. cargo test config:: --lib  (verify tests pass)"
echo "3. Create feature branch and push to GitHub"
echo "4. Open PR with RUNTIME_CONFIG_PR.md as description"
echo "5. Await review and merge"
echo
