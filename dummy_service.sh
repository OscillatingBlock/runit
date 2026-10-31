#!/bin/bash
echo "Dummy service started with PID $$"

# Run for 5 seconds, then exit naturally
for i in {1..1000}; do
  echo "Service running... step $i"
  sleep 1
done

echo "Dummy service exiting naturally!"
exit 0
