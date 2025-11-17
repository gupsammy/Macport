#!/bin/bash

# Macport Notification Test Script
# This script starts and stops servers on various ports to trigger macport notifications

echo "🧪 Macport Notification Test Script"
echo "===================================="
echo ""
echo "This script will:"
echo "1. Start servers on ports 4000, 4001, 9000"
echo "2. Wait 5 seconds (watch for 'Ports now listening' notification)"
echo "3. Stop the servers"
echo "4. Wait 5 seconds (watch for 'Ports freed' notification)"
echo "5. Repeat the cycle 3 times"
echo ""
echo "Make sure macport is running and notifications are enabled!"
echo ""
read -p "Press Enter to start the test..."

# Function to start test servers
start_servers() {
    echo ""
    echo "⬆️  Starting servers on ports 4000, 4001, 9000..."

    # Start Python HTTP servers in background
    python3 -m http.server 4000 > /dev/null 2>&1 &
    PID1=$!

    python3 -m http.server 4001 > /dev/null 2>&1 &
    PID2=$!

    python3 -m http.server 9000 > /dev/null 2>&1 &
    PID3=$!

    # Store PIDs in a file for cleanup
    echo "$PID1 $PID2 $PID3" > /tmp/macport-test-pids.txt

    echo "✅ Servers started: PIDs $PID1, $PID2, $PID3"
    echo "   You should see a notification: 'Ports now listening: [4000, 4001, 9000]'"
}

# Function to stop test servers
stop_servers() {
    echo ""
    echo "⬇️  Stopping servers..."

    if [ -f /tmp/macport-test-pids.txt ]; then
        PIDS=$(cat /tmp/macport-test-pids.txt)
        for pid in $PIDS; do
            kill $pid 2>/dev/null
        done
        rm /tmp/macport-test-pids.txt
        echo "✅ Servers stopped"
        echo "   You should see a notification: 'Ports freed: [4000, 4001, 9000]'"
    fi
}

# Cleanup function
cleanup() {
    echo ""
    echo "🧹 Cleaning up..."
    stop_servers
    exit 0
}

# Set trap for cleanup on script exit
trap cleanup EXIT INT TERM

# Run 3 cycles
for cycle in 1 2 3; do
    echo ""
    echo "========================================="
    echo "Cycle $cycle of 3"
    echo "========================================="

    start_servers
    echo ""
    echo "⏱️  Waiting 5 seconds..."
    sleep 5

    stop_servers
    echo ""
    echo "⏱️  Waiting 5 seconds before next cycle..."
    sleep 5
done

echo ""
echo "========================================="
echo "✅ Test completed!"
echo "========================================="
echo ""
echo "You should have seen 6 notifications total:"
echo "  - 3x 'Ports now listening' notifications"
echo "  - 3x 'Ports freed' notifications"
echo ""
echo "If you didn't see notifications, check:"
echo "  1. Macport is running"
echo "  2. Notifications are enabled in config"
echo "  3. Notifications are not snoozed"
echo "  4. macOS notification permissions are granted"
echo ""
