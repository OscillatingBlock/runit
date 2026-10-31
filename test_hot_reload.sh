#!/bin/bash

# Exit immediately if any command fails
set -e

# Define paths
MOCK_CONFIG="./mock_config"
HOT_LOAD_DIR="$MOCK_CONFIG/hot_loaded_service"
RUN_SCRIPT="$HOT_LOAD_DIR/run"
LOG_FILE="./orchestrator.log"

echo "=== Step 1: Cleaning up old states and compiling ==="
rm -rf "$MOCK_CONFIG"
mkdir -p "$MOCK_CONFIG"
rm -f "$LOG_FILE"
cargo build --bin runsvdir

echo "=== Step 2: Launching Orchestrator in the background ==="
# Start the orchestrator, route logs to a file, and save its PID
CONFIG_DIR=$MOCK_CONFIG MAX_RESTART=3 target/debug/runsvdir >"$LOG_FILE" 2>&1 &
ORCHESTRATOR_PID=$!

# Ensure we kill the orchestrator if this test script exits unexpectedly
trap "echo 'Cleaning up orchestrator...'; kill $ORCHESTRATOR_PID 2>/dev/null || true" EXIT

echo "Orchestrator running with PID: $ORCHESTRATOR_PID"
sleep 2 # Let it initialize

echo "=== Step 3: Simulating Hot-Loading (Creating a new service) ==="
mkdir -p "$HOT_LOAD_DIR"
cat <<'EOF' >"$RUN_SCRIPT"
#!/bin/bash
echo "[HotLoad] Service spawned! My PID is $$"
while true; do
    sleep 1
done
EOF
chmod +x "$RUN_SCRIPT"
echo "New service folder created at $HOT_LOAD_DIR"

echo "Waiting 6 seconds for the orchestrator tick to pick it up..."
sleep 6

# Verify if the service actually launched
echo "Checking logs for launch confirmation..."
if grep -q "Hot-loading new service" "$LOG_FILE"; then
  echo "✓ SUCCESS: Orchestrator detected the new service folder!"
else
  echo "✗ FAILURE: Orchestrator missed the directory addition."
  cat "$LOG_FILE"
  exit 1
fi

# Find the PID of our running script
CHILD_SCRIPT_PID=$(pgrep -f "hot_loaded_service/run" || true)
if [ -n "$CHILD_SCRIPT_PID" ]; then
  echo "✓ SUCCESS: Hot-loaded service is running actively in the OS under PID: $CHILD_SCRIPT_PID"
else
  echo "✗ FAILURE: Service config detected but process was not launched."
  exit 1
fi

echo "=== Step 4: Simulating Hot-Eviction (Deleting the service) ==="
rm -rf "$HOT_LOAD_DIR"
echo "Deleted service folder $HOT_LOAD_DIR"

echo "Waiting 6 seconds for the orchestrator tick to clean it up..."
sleep 6

# Check if the process was killed
if pgrep -f "hot_loaded_service/run" >/dev/null; then
  echo "✗ FAILURE: The service directory was deleted, but the child process is STILL ALIVE!"
  exit 1
else
  echo "✓ SUCCESS: The service process was successfully killed and reaped by the orchestrator!"
fi

echo "Checking logs for termination confirmation..."
if grep -q "removed from disk. Terminating live PID" "$LOG_FILE"; then
  echo "✓ SUCCESS: Orchestrator logged the explicit termination routing code!"
else
  echo "Warning: Kill signal sent, but explicit removal string not found in logs."
fi

echo "=== All Milestone 3 Hot-Reload Tests Passed Successfully! ==="
