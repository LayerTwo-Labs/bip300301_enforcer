#!/bin/bash

echo "=== BIP300301 Enforcer Performance Analysis ==="
echo

# Create output directory
mkdir -p trace_results
cd trace_results

echo "Starting enforcer and tracing..."

# Start the enforcer in background
echo "1. Starting enforcer..."
cd ..

# Make sure the binary exists
if [ ! -f "./target/release/bip300301_enforcer" ]; then
    echo "   ERROR: Binary not found. Building first..."
    cargo build --release
fi

./target/release/bip300301_enforcer --data-dir ./datadir --node-rpc-addr=localhost:38332 --node-rpc-user=user --node-rpc-pass=password --enable-mempool > trace_results/enforcer.log 2>&1 &
ENFORCER_PID=$!
echo "   Enforcer PID: $ENFORCER_PID"

# Wait for it to start up and find the actual enforcer process
sleep 5

# Find the actual enforcer process (not the shell)
ACTUAL_PID=$(pgrep -f "bip300301_enforcer.*--data-dir")
if [ -n "$ACTUAL_PID" ]; then
    echo "   Found actual enforcer PID: $ACTUAL_PID"
    ENFORCER_PID=$ACTUAL_PID
else
    echo "   Using shell PID: $ENFORCER_PID"
fi

# Check if process is running
if ! kill -0 $ENFORCER_PID 2>/dev/null; then
    echo "   ERROR: Enforcer failed to start!"
    echo "   Checking log..."
    tail -10 trace_results/enforcer.log
    exit 1
fi

echo "2. Starting traces (will run for 60 seconds)..."

# System call tracing
echo "   - System calls (dtruss)..."
sudo dtruss -c -p $ENFORCER_PID > trace_results/syscalls.txt 2>&1 &
DTRUSS_PID=$!

# File system activity
echo "   - File system activity..."
sudo fs_usage -w -f filesys $ENFORCER_PID > trace_results/filesystem.txt 2>&1 &
FSUSAGE_PID=$!

# Network activity
echo "   - Network activity..."
nettop -p $ENFORCER_PID -l 1 > trace_results/network.txt 2>&1 &
NETTOP_PID=$!

# Process sampling
echo "   - Process sampling..."
sample $ENFORCER_PID 60 -f trace_results/sample.txt &
SAMPLE_PID=$!

# Resource monitoring
echo "   - Resource monitoring..."
top -pid $ENFORCER_PID -l 12 -s 5 > trace_results/resources.txt 2>&1 &
TOP_PID=$!

echo "3. Collecting data for 60 seconds..."
echo "   (You can watch activity in trace_results/ directory)"

# Wait for 60 seconds
for i in {1..60}; do
    echo -ne "   Progress: [$i/60]\r"
    sleep 1
    
    # Check if enforcer is still running
    if ! kill -0 $ENFORCER_PID 2>/dev/null; then
        echo -e "\n   WARNING: Enforcer process died at $i seconds"
        break
    fi
done

echo -e "\n4. Stopping traces..."

# Stop all monitoring processes
kill $DTRUSS_PID $FSUSAGE_PID $NETTOP_PID $SAMPLE_PID $TOP_PID 2>/dev/null
sudo pkill dtruss 2>/dev/null
sudo pkill fs_usage 2>/dev/null

# Stop the enforcer
echo "5. Stopping enforcer..."
kill $ENFORCER_PID 2>/dev/null

# Wait a moment for files to be written
sleep 2

echo "6. Analysis complete! Results in trace_results/:"
echo
ls -la trace_results/
echo

echo "=== Quick Analysis ==="

# System calls summary
if [ -f trace_results/syscalls.txt ]; then
    echo "System calls captured:"
    wc -l trace_results/syscalls.txt
    echo "Sample system calls:"
    grep -v "^#\|^SYSCALL\|printf\|probefunc" trace_results/syscalls.txt | head -5
    echo
fi

# File activity summary  
if [ -f trace_results/filesystem.txt ]; then
    echo "File operations captured:"
    wc -l trace_results/filesystem.txt
    echo "Sample file operations:"
    grep -v "fs_usage:" trace_results/filesystem.txt | head -5
    echo
fi

# Check if enforcer actually started
if [ -f trace_results/enforcer.log ]; then
    echo "Enforcer startup status:"
    if grep -q "error\|Error\|ERROR" trace_results/enforcer.log; then
        echo "   ERRORS found in log:"
        grep -i error trace_results/enforcer.log | head -3
    else
        echo "   Started successfully"
        echo "   Log size: $(wc -l < trace_results/enforcer.log) lines"
    fi
    echo
fi

echo "=== Next Steps ==="
echo "1. Review trace_results/syscalls.txt for system call patterns"
echo "2. Check trace_results/filesystem.txt for database I/O"
echo "3. Look at trace_results/sample.txt for CPU usage breakdown"
echo "4. Examine trace_results/network.txt for network activity"

echo
echo "To re-run: ./trace_enforcer.sh" 