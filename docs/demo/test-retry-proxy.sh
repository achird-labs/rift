#!/bin/bash
# Test script for retry proxy simulation
#
# This demonstrates Rift's scripting capability to simulate a service that
# fails the first 2 requests and succeeds on the 3rd (passes through to upstream).

set -e

RIFT_URL="${RIFT_URL:-http://localhost:4560}"
FLOW_ID="${FLOW_ID:-test-flow-$(date +%s)}"

echo "=========================================="
echo "Retry Proxy Simulation Test"
echo "=========================================="
echo "Rift URL: $RIFT_URL"
echo "Flow ID: $FLOW_ID"
echo ""

# Function to make a request and show result
make_request() {
    local attempt=$1
    echo "--- Attempt $attempt ---"
    response=$(curl -s -w "\n%{http_code}" -H "X-Flow-Id: $FLOW_ID" "$RIFT_URL/api/resource")
    http_code=$(echo "$response" | tail -n1)
    body=$(echo "$response" | sed '$d')

    echo "HTTP Status: $http_code"
    echo "Response: $body"
    echo ""

    return 0
}

# Reset counter first
echo "Resetting counter for flow: $FLOW_ID"
reset=$(curl -s -X DELETE -H "X-Flow-Id: $FLOW_ID" "$RIFT_URL/api/reset")
echo "$reset" | jq . 2>/dev/null || echo "$reset"
echo ""
echo ""

# Make 4 requests to demonstrate the pattern
echo "Making requests to demonstrate retry behavior..."
echo "(First 2 should fail with 503, 3rd+ should pass through)"
echo ""

for i in 1 2 3 4; do
    make_request $i
    sleep 0.5
done

echo "=========================================="
echo "Test complete!"
echo ""
echo "Expected behavior:"
echo "  - Attempt 1: 503 (script-generated failure)"
echo "  - Attempt 2: 503 (script-generated failure)"
echo "  - Attempt 3: 200 (passed through to upstream)"
echo "  - Attempt 4: 200 (passed through to upstream)"
echo "=========================================="
