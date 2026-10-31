#!/usr/bin/env bash
# End-to-end test for the runit init system + client.
# Covers: status, stop (ok + repeat), reload, restart, crash->backoff->max_restart,
# hot-load, hot-unload, shutdown.

set -u
BASE=/tmp/opencode/runit-test
SOCK=$BASE/runit.sock
CFG=$BASE/config
LOGS=$BASE/logs
PROJ=/home/aayush/projects/rustProjects/run_sv
CLIENT=$PROJ/target/debug/runit-client
INIT=$PROJ/target/debug/runit

header() {
  echo
  echo "===== $* ====="
}

rcli() {
  LISTENER_PATH=$SOCK $CLIENT "$@"
}

last_pid_for() { # $1 = service name -> pid from registry
  awk -v n="$1" '$2==n{p=$1} END{print p}' "$LOGS/pids"
}

pid_alive() { # $1 = pid -> 0 if the process exists (reaped check via ps)
  ps -p "$1" -o pid= >/dev/null 2>&1
}

cleanup() {
  kill -TERM "$INIT_PID" 2>/dev/null
  wait "$INIT_PID" 2>/dev/null
}
trap cleanup EXIT

header "0. setup"
pkill -x runit 2>/dev/null
sleep 0.3
rm -rf "$BASE"
mkdir -p "$CFG" "$LOGS"

# svc-a: long-running service, registers its pid
cat >"$CFG/svc-a-run.tmp" <<EOF
#!/bin/sh
echo "\$\$ svc-a \$(date +%s)" >> $LOGS/pids
while true; do sleep 1; done
EOF

# svc-b: sleeps 4s then exits 1 on purpose -> crash restart cycle
cat >"$CFG/svc-b-run.tmp" <<EOF
#!/bin/sh
echo "\$\$ svc-b \$(date +%s)" >> $LOGS/pids
sleep 4
exit 1
EOF

# svc-c: long-running, added live during the test
cat >"$CFG/svc-c-run.tmp" <<EOF
#!/bin/sh
echo "\$\$ svc-c \$(date +%s)" >> $LOGS/pids
while true; do sleep 1; done
EOF

mkdir -p "$CFG/svc-a" "$CFG/svc-b"
mv "$CFG/svc-a-run.tmp" "$CFG/svc-a/run"
mv "$CFG/svc-b-run.tmp" "$CFG/svc-b/run"
chmod +x "$CFG/svc-a/run" "$CFG/svc-b/run"

header "1. client with no init system running (connect error)"
rcli status
echo "exit=$?"

header "2. start init system"
CONFIG_DIR=$CFG MAX_RESTART=2 LISTENER_PATH=$SOCK nohup "$INIT" >"$BASE/init.log" 2>&1 &
INIT_PID=$!
sleep 1.5
echo "init pid=$INIT_PID"

header "3. status (expect 2 alive)"
rcli status
echo "exit=$?"
PID_A=$(last_pid_for svc-a)
PID_B=$(last_pid_for svc-b)
echo "pid_a=$PID_A pid_b=$PID_B"

header "4. stop svc-a (expect success, process really gone)"
rcli stop "$PID_A"
echo "exit=$?"
sleep 1
pid_alive "$PID_A" && echo "FAIL: $PID_A still alive" || echo "ok: $PID_A is gone"

header "5. stop svc-a again with same pid (expect init Error)"
rcli stop "$PID_A"
echo "exit=$?"

header "6. reload with the old (already stopped) pid — probing known reload bug"
rcli reload "$PID_A"
echo "exit=$?"
sleep 1
echo "pid_alive(old): $(pid_alive $PID_A && echo yes || echo no)"
echo "registry svc-a entries (expect NO new one from reload):"
grep svc-a "$LOGS/pids" || true

header "7. wait for tick-sync hot-load of svc-a (run file still on disk)"
sleep 6
echo "registry svc-a entries:"
cat "$LOGS/pids"
PID_A2=$(last_pid_for svc-a)
echo "new pid_a=$PID_A2 alive=$(pid_alive $PID_A2 && echo yes || echo no)"

header "8. restart command (expect success response)"
rcli restart
echo "exit=$?"

header "9. svc-b crash cycle: backoff restarts then max_restart gives up"
# timeline with sleep 4 + MAX_RESTART=2: deaths ~t4,~t9,~t13 -> 3rd death fails
sleep 14
grep -cE 'svc-b' "$LOGS/pids" | xargs echo "svc-b launches so far:"
grep -E 'scheduled to restart|exceeded max restart|New PID' "$BASE/init.log" | grep -F 'svc-b' | tail -8
echo "pid_alive(latest b): $(pid_alive $(last_pid_for svc-b) && echo yes || echo no)"

header "10. hot-load svc-c while init is running"
mkdir -p "$CFG/svc-c"
mv "$CFG/svc-c-run.tmp" "$CFG/svc-c/run"
chmod +x "$CFG/svc-c/run"
echo "svc-c dropped in, waiting <=2 sync ticks..."
sleep 7
rcli status
echo "exit=$?"

header "11. hot-unload: remove svc-c from disk, expect it killed on next sync"
rm -rf "$CFG/svc-c"
PID_C=$(last_pid_for svc-c)
sleep 7
pid_alive "$PID_C" && echo "FAIL: svc-c ($PID_C) still alive" || echo "ok: svc-c ($PID_C) is gone"

header "12. reload the live svc-a pid again (same bug, fresh pid)"
PID_A2=$(last_pid_for svc-a)
rcli reload "$PID_A2"
echo "exit=$?"
sleep 1
echo "pid_alive($PID_A2): $(pid_alive $PID_A2 && echo yes || echo no)"

header "13. shutdown (expect success; init process itself keeps running)"
rcli shutdown
echo "exit=$?"
sleep 1
echo "children of init that are still around:"
ps --ppid "$INIT_PID" -o pid=,cmd= || true
echo "registry:"
cat "$LOGS/pids"

header "14. SIGTERM to init process itself (expect clean exit 0)"
kill -TERM "$INIT_PID"
wait "$INIT_PID"
echo "init exit code=$?"

header "init.log tail (server-side view)"
tail -25 "$BASE/init.log"
