#!/bin/bash
echo "[Child] Service started up with PID $$"
sleep 0.5
echo "[Child] Encountered an error! Crashing now..."
exit 42
