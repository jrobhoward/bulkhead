#!/bin/sh
# The privileged helper end to end on a Linux test machine (ROADMAP PS1),
# as root, never on a development machine: bulkhead-priv under systemd
# (examples/systemd/bulkhead-priv.service) starts the node, which deploys
# the board example; the app must run under a user of its own, in its own
# cgroup, PID namespace and minimal root, and the board is served on
# port 80 too, bound by the helper for the node, which must hold no
# capabilities (ROADMAP PS3b). With Postgres installed
# (/usr/lib/postgresql/*/bin) it also deploys the tally example on a
# datasource (ROADMAP DA3): a scratch cluster as `postgres`, reached first
# over its Unix socket with peer authentication (the node connects, as
# `bulkhead`), then, after a restart, over TCP by name with a password the
# datasource's driver reads from its file. An app granted HTTP egress
# (the SDK fixture, ROADMAP DA4) fetches the board's route through its
# egress driver: the node must make the connection, not the driver.
# A fake Modbus device is polled by the Modbus driver (ROADMAP DA5),
# its updates going to the same app; the node must open the device's
# connection, not the driver. It installs a `bulkhead` user,
# binaries under /usr/local, /etc/bulkhead and the unit, and removes them
# all at the end unless KEEP=1.
#
#   cargo build -p bulkhead -p bulkhead-priv -p example-board   # as a user
#   cargo build -p bulkhead-runner
#   cargo build -p bulkhead-resolve -p bulkhead-ingress -p bulkhead-admin -p bulkhead-metrics
#   cargo build -p bulkhead-sql -p example-tally
#   cargo build -p bulkhead-egress -p bulkhead-fixture-sdk
#   cargo build -p bulkhead-modbus --bins --example fake_device
#   sudo sh scripts/priv-e2e-systemd.sh "$PWD"
set -eu
SRC=${1:?checkout}
T=$SRC/target/debug
id bulkhead >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin bulkhead
NUID=$(id -u bulkhead); NGID=$(id -g bulkhead)
install -m 0755 $T/bulkhead-priv /usr/local/sbin/bulkhead-priv
install -m 0755 $T/bulkhead /usr/local/bin/bulkhead
install -d -m 0755 /usr/local/libexec/bulkhead
install -m 0755 $T/bulkhead-runner /usr/local/libexec/bulkhead/bulkhead-runner
install -m 0755 $T/bulkhead-modbus /usr/local/libexec/bulkhead/bulkhead-modbus
install -m 0755 $T/bulkhead-modbus-server /usr/local/libexec/bulkhead/bulkhead-modbus-server
# A fake Modbus device for the node's Modbus driver to poll.
$T/examples/fake_device 127.0.0.1:5021 >/dev/null 2>&1 & FAKE=$!
install -m 0755 $T/bulkhead-resolve /usr/local/libexec/bulkhead/bulkhead-resolve
install -m 0755 $T/bulkhead-ingress /usr/local/libexec/bulkhead/bulkhead-ingress
install -m 0755 $T/bulkhead-admin /usr/local/libexec/bulkhead/bulkhead-admin
install -m 0755 $T/bulkhead-metrics /usr/local/libexec/bulkhead/bulkhead-metrics
install -m 0755 $T/bulkhead-sql /usr/local/libexec/bulkhead/bulkhead-sql
install -m 0755 $T/bulkhead-egress /usr/local/libexec/bulkhead/bulkhead-egress
install -d -m 0755 /etc/bulkhead
systemctl stop bulkhead 2>/dev/null || true
rm -rf /var/lib/bulkhead
install -d -m 0700 -o bulkhead -g bulkhead /var/lib/bulkhead
W=/var/lib/bulkhead-e2e; rm -rf $W; install -d -m 0755 $W
cd $W
# The remote admin API (ROADMAP DA7): the admin key and the operators file,
# the node's, in a directory the helper binds into the admin driver's root;
# an operator's key for the CLI.
K=/var/lib/bulkhead-admin; rm -rf $K; install -d -m 0700 -o bulkhead -g bulkhead $K
/usr/local/bin/bulkhead cluster keygen $K/admin.key >/dev/null
/usr/local/bin/bulkhead cluster keygen $W/operator.key >/dev/null
ADMIN_KEY=$(/usr/local/bin/bulkhead cluster pubkey $K/admin.key)
printf '[operators]\ne2e = ["%s"]\n' "$(/usr/local/bin/bulkhead cluster pubkey $W/operator.key)" > $K/operators.toml
chown bulkhead:bulkhead $K/admin.key $K/operators.toml; chmod 0600 $K/admin.key $K/operators.toml
/usr/local/bin/bulkhead keygen signer --no-password >/dev/null
SIGNER=$(tail -n 1 signer.pub)
cp $SRC/examples/board/bulkhead.toml board.toml
/usr/local/bin/bulkhead bundle board.toml $T/example-board --key signer.key --out board.rbp >/dev/null
chmod 0644 board.rbp
# The SDK fixture as an app allowed one server, the board's route.
cat > fetcher.toml <<'TOML'
[app]
name = "fetcher"
version = "1.0.0"

[appserver.capabilities."bulkhead:http-egress"]
allow = ["http://127.0.0.1:8080"]
TOML
/usr/local/bin/bulkhead bundle fetcher.toml $T/bulkhead-fixture-sdk --key signer.key --out fetcher.rbp >/dev/null
chmod 0644 fetcher.rbp
cat > /etc/bulkhead/policy.toml <<'TOML'
[apps.board]
grant = ["bulkhead:http-routes", "bulkhead:keyvalue", "bulkhead:logging"]
buckets = ["board"]

[apps.fetcher]
grant = ["bulkhead:http-egress"]

[apps.tally]
grant = ["bulkhead:http-routes", "bulkhead:sql", "bulkhead:logging"]
datasources = ["main"]
TOML
# A scratch Postgres for the datasource (ROADMAP DA3): peer authentication
# on its socket, a password over TCP; the password in a file of the node's
# that the helper shares with the datasource's driver alone.
PGBIN=$(ls -d /usr/lib/postgresql/*/bin 2>/dev/null | sort -V | tail -1)
P=/var/lib/bulkhead-e2e-pg; PGPORT=5544
pg_down() { [ -d $P/data ] && runuser -u postgres -- $PGBIN/pg_ctl -D $P/data -m fast stop >/dev/null 2>&1 || true; }
pg_down; rm -rf $P
DATASOURCE=""
if [ -n "$PGBIN" ]; then
    install -d -m 0755 -o postgres -g postgres $P
    runuser -u postgres -- $PGBIN/initdb -D $P/data -U postgres --auth-local=peer --auth-host=scram-sha-256 >/dev/null
    runuser -u postgres -- $PGBIN/pg_ctl -D $P/data -l $P/log -w \
        -o "-k $P -p $PGPORT -c listen_addresses=127.0.0.1" start >/dev/null
    runuser -u postgres -- psql -q -h $P -p $PGPORT -d postgres \
        -c "CREATE ROLE bulkhead LOGIN PASSWORD 'e2e-secret'" -c "CREATE DATABASE tally OWNER bulkhead"
    printf 'e2e-secret\n' > $K/pg.password
    chown bulkhead:bulkhead $K/pg.password; chmod 0600 $K/pg.password
    cp $SRC/examples/tally/bulkhead.toml tally.toml
    /usr/local/bin/bulkhead bundle tally.toml $T/example-tally --key signer.key --out tally.rbp >/dev/null
    chmod 0644 tally.rbp
    DATASOURCE="[datasources.main]
host = \"$P\"
port = $PGPORT
dbname = \"tally\"
user = \"bulkhead\"
"
fi
cat > /etc/bulkhead/node.toml <<TOML
node = 1
data_dir = "/var/lib/bulkhead"
trusted_keys = ["$SIGNER"]
policy = "/etc/bulkhead/policy.toml"

[http.main]
address = "127.0.0.1:8080"

# A port below 1024: the node, holding no capabilities, has the helper
# bind it (ROADMAP PS3b; port 80 in priv.conf's ports).
[http.low]
address = "127.0.0.1:80"

[metrics]
address = "127.0.0.1:9101"

[admin]
address = "127.0.0.1:7443"
key = "$K/admin.key"
operators = "$K/operators.toml"

[buckets.board]
durability = "disk"

[secrets.pg]
file = "$K/pg.password"

$DATASOURCE
[extensions."bulkhead-modbus:modbus".devices.pump-1]
address = "127.0.0.1:5021"
poll_ms = 200
deliver_to = "fetcher"
holding = [{ start = 0, count = 4 }]

# The node's own drivers (bulkhead-resolve, bulkhead-ingress, ...) beside the
# helper's runner, in every instance's root.
[native]
runner = "/usr/local/libexec/bulkhead/bulkhead-runner"

[priv]
TOML
cat > /etc/bulkhead/priv.conf <<CONF
node = /usr/local/bin/bulkhead
node_args = node --config /etc/bulkhead/node.toml
node_uid = $NUID
node_gid = $NGID
runner = /usr/local/libexec/bulkhead/bulkhead-runner
uids = 60000-60999
state = /var/lib/bulkhead-priv
binds = /var/lib/bulkhead, $K
memory_max = 2GiB
pids_max = 1024
ports = 80
cgroup = /sys/fs/cgroup/system.slice/bulkhead.service
CONF
chmod 0600 /etc/bulkhead/priv.conf
install -m 0644 $SRC/examples/systemd/bulkhead-priv.service /etc/systemd/system/bulkhead.service
systemctl daemon-reload
systemctl restart bulkhead
for i in $(seq 100); do [ -S /var/lib/bulkhead/admin.sock ] && break; sleep 0.1; done
echo "== processes"
ps -eo user:12,uid,pid,ppid,args --forest | grep -E "bulkhead|example-board" | grep -v grep
cli() { runuser -u bulkhead -- /usr/local/bin/bulkhead --socket /var/lib/bulkhead/admin.sock "$@"; }
echo "== deploy"
cli deploy $W/board.rbp
for i in $(seq 100); do curl -sf http://127.0.0.1:8080/board >/dev/null && break; sleep 0.1; done
echo "== board"; curl -s http://127.0.0.1:8080/board; echo
curl -s -X POST http://127.0.0.1:8080/board >/dev/null
curl -s http://127.0.0.1:8080/board; echo
echo "== metrics (bulkhead-metrics)"; curl -s http://127.0.0.1:9101/metrics | grep 'bulkhead_instances{app="board"}'
echo "== port 80, bound by the helper (ROADMAP PS3b)"
curl -sf http://127.0.0.1:80/board || { echo "FAILED: nothing served on port 80"; exit 1; }; echo
NODE=$(pgrep -x bulkhead | head -1)
CAPS=$(awk '/^CapEff/ {print $2}' /proc/$NODE/status)
echo "the node's effective capabilities: $CAPS"
[ "$CAPS" = 0000000000000000 ] || { echo "FAILED: the node holds capabilities"; exit 1; }
echo "== remote admin (bulkhead-admin, signed requests)"
/usr/local/bin/bulkhead --remote 127.0.0.1:7443 --node-key "$ADMIN_KEY" --key $W/operator.key list
echo "== HTTP egress through the app's driver (ROADMAP DA4)"
cli deploy $W/fetcher.rbp
for i in $(seq 100); do pgrep -f "bulkhead-egress" >/dev/null && break; sleep 0.1; done
ps -eo user:12,uid,pid,args | grep "bulkhead-egress" | grep -v grep
NODE=$(pgrep -x bulkhead | head -1); EGR=$(pgrep -f "bulkhead-egress" | head -1)
echo "the driver's view: uid $(awk '/^Uid/ {print $2}' /proc/$EGR/status), root $(ls /proc/$EGR/root/ | tr '\n' ' ')"
if command -v strace >/dev/null; then
    strace -f -qq -e trace=connect -p $NODE -o $W/egress-node.strace & S1=$!
    strace -f -qq -e trace=connect -p $EGR -o $W/egress-driver.strace & S2=$!
    sleep 0.5
fi
echo "allowed: $(cli call fetcher 'fetch GET http://127.0.0.1:8080/board')"
echo "outside allow: $(cli call fetcher 'fetch GET http://127.0.0.1:9101/metrics')"
if command -v strace >/dev/null; then
    sleep 0.3; kill $S1 $S2; wait $S1 $S2 2>/dev/null || true
    echo "connects to :8080 by the node: $(grep -c 'htons(8080)' $W/egress-node.strace), to :9101: $(grep -c 'htons(9101)' $W/egress-node.strace), by the driver: $(grep -c connect $W/egress-driver.strace)"
fi
echo "== Modbus through its driver (ROADMAP DA5)"
ps -eo user:12,uid,pid,args | grep "libexec/bulkhead/bulkhead-modbus" | grep -v grep
MOD=$(pgrep -f "libexec/bulkhead/bulkhead-modbus" | head -1)
echo "the driver's view: uid $(awk '/^Uid/ {print $2}' /proc/$MOD/status), root $(ls /proc/$MOD/root/ | tr '\n' ' ')"
# The fixture keeps a CBOR cast as `<binary>`.
echo "updates received: $(cli call fetcher received | tr ',' '\n' | grep -c binary)"
if command -v strace >/dev/null; then
    strace -f -qq -e trace=connect -p $NODE -o $W/modbus-node.strace & S1=$!
    strace -f -qq -e trace=connect -p $MOD -o $W/modbus-driver.strace & S2=$!
    sleep 0.5
    # The device goes away and comes back: the driver asks for a new
    # connection.
    kill $FAKE; sleep 0.5
    $T/examples/fake_device 127.0.0.1:5021 >/dev/null 2>&1 & FAKE=$!
    sleep 2
    kill $S1 $S2; wait $S1 $S2 2>/dev/null || true
    echo "connects to :5021 by the node: $(grep -c 'htons(5021)' $W/modbus-node.strace), by the driver: $(grep -c connect $W/modbus-driver.strace)"
fi
cli stop fetcher
sleep 0.5
echo "egress drivers after the app stopped: $(pgrep -f bulkhead-egress | wc -l)"
if [ -n "$PGBIN" ]; then
    echo "== tally on the datasource, over its socket (peer authentication)"
    cli deploy $W/tally.rbp
    for i in $(seq 100); do curl -sf http://127.0.0.1:8080/tally >/dev/null && break; sleep 0.1; done
    curl -s -X POST http://127.0.0.1:8080/tally >/dev/null
    curl -s -X POST -d 4 http://127.0.0.1:8080/tally; echo
    runuser -u postgres -- psql -Atq -h $P -p $PGPORT -d postgres \
        -c "SELECT 'session: ' || usename || ' ' || coalesce(client_addr::text, 'socket') || ' ' || application_name FROM pg_stat_activity WHERE datname = 'tally'"
    echo "== the datasource's driver"
    ps -eo user:12,uid,pid,args | grep "bulkhead-sql" | grep -v grep
    DRV=$(pgrep -f "bulkhead-sql" | head -1)
    echo "its view: uid $(awk '/^Uid/ {print $2}' /proc/$DRV/status), root $(ls /proc/$DRV/root/ | tr '\n' ' ')"
    echo "== the datasource over TCP by name, its password from a file"
    sed -i "s|^host = \"$P\"|host = \"localhost\"\npassword_secret = \"pg\"|" /etc/bulkhead/node.toml
    systemctl restart bulkhead
    # The driver starts before the apps, its pool empty: trace both before
    # the first statement opens a connection.
    for i in $(seq 100); do pgrep -f "bulkhead-sql" >/dev/null && break; sleep 0.1; done
    NODE=$(pgrep -x bulkhead | head -1); DRV=$(pgrep -f "bulkhead-sql" | head -1)
    if command -v strace >/dev/null; then
        strace -f -qq -e trace=connect -p $NODE -o $W/node.strace & S1=$!
        strace -f -qq -e trace=connect -p $DRV -o $W/driver.strace & S2=$!
        sleep 0.5
    fi
    for i in $(seq 100); do curl -sf http://127.0.0.1:8080/tally >/dev/null && break; sleep 0.2; done
    echo "count after the restart: $(curl -s http://127.0.0.1:8080/tally)"
    if command -v strace >/dev/null; then
        sleep 0.3; kill $S1 $S2; wait $S1 $S2 2>/dev/null || true
        echo "connects to :$PGPORT by the node: $(grep -c "htons($PGPORT)" $W/node.strace), by the driver: $(grep -c connect $W/driver.strace)"
    fi
    runuser -u postgres -- psql -Atq -h $P -p $PGPORT -d postgres \
        -c "SELECT 'session: ' || usename || ' ' || coalesce(client_addr::text, 'socket') FROM pg_stat_activity WHERE datname = 'tally'"
fi
echo "== status"; cli status board
# The board's process, by its cgroup: the node's drivers start first, so
# its user is not the first of the range.
APP=$(cat /sys/fs/cgroup/system.slice/bulkhead.service/apps/board/i*/cgroup.procs | head -1)
AUID=$(awk '/^Uid/ {print $2}' /proc/$APP/status)
echo "== processes with the app"
ps -eo user:12,uid,gid,pid,args | grep -E "bulkhead-priv|bulkhead node" | grep -v grep
ps -o user:12,uid,gid,pid,args -p $APP | tail -1
echo "== the app's view"
echo "uid/gid: $(awk '/^Uid|^Gid/ {print $2}' /proc/$APP/status | tr '\n' ' ')"
echo "cgroup:  $(cat /proc/$APP/cgroup)"
echo "pid ns:  $(readlink /proc/$APP/ns/pid) (node: $(readlink /proc/$(pgrep -x bulkhead | head -1)/ns/pid))"
echo "root:    $(ls /proc/$APP/root/)"
echo "== what the app's user reaches (ROADMAP PS2)"
D=/var/lib/bulkhead
stat -c '%a %U:%G %n' $D $D/state.json $D/kv $D/bundles $D/compiled $D/compiled/board $D/priv
as_app() { setpriv --reuid=$AUID --regid=$AUID --clear-groups "$@"; }
for p in $D/state.json $D/kv $D/bundles/board; do
    if as_app test -r "$p"; then echo "READABLE by the app: $p"; else echo "refused to the app: $p"; fi
done
if as_app test -r $D/compiled/board -a -x $D/compiled/board; then echo "the app reaches its program's directory"; else echo "NOT REACHABLE by the app: its program's directory"; fi
echo "helper caps: $(grep -E 'CapEff|CapBnd' /proc/$(pgrep -x bulkhead-priv)/status | tr '\n' ' ')"
echo "node caps:   $(grep -E 'CapEff' /proc/$(pgrep -x bulkhead | head -1)/status)"
echo "== stop"
systemctl stop bulkhead
sleep 1
systemctl status bulkhead --no-pager | head -3 || true
echo "left: $(ps -eo uid= | awk '$1>=60000 && $1<=60999' | wc -l) app processes, $(pgrep -x bulkhead | wc -l) nodes"
journalctl -u bulkhead --no-pager -n 40 | grep -E "bulkhead-priv|error|ERROR|WARN" | tail -15
if [ "${KEEP:-0}" != 1 ]; then
    rm -f /etc/systemd/system/bulkhead.service && systemctl daemon-reload
    rm -f /usr/local/sbin/bulkhead-priv /usr/local/bin/bulkhead
    pg_down
    kill $FAKE 2>/dev/null || true
    rm -rf /usr/local/libexec/bulkhead /etc/bulkhead /var/lib/bulkhead /var/lib/bulkhead-priv $W $K $P
    userdel bulkhead
    echo "== removed"
fi
