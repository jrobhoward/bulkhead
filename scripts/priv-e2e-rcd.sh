#!/bin/sh
# The privileged helper's rc.d script on a FreeBSD test machine (ROADMAP
# PS1), as root, never on a development machine: rc starts bulkhead-priv
# (examples/freebsd/bulkhead), which starts the node; status, reload and
# stop go through it. No app is deployed here (tests/priv.rs runs apps
# under the helper). The metrics driver listens on port 80, which the
# helper binds for the node (ROADMAP PS3b). Two datasources (ROADMAP DA3) start a driver each,
# jailed under a user of its own, one with a password file shared with it;
# no statement runs without an app (tests/priv.rs logs one in), and no
# egress driver starts without one (tests/priv.rs runs one). It
# installs a `bulkhead` user, binaries under
# /usr/local and the rc script, and removes them all at the end unless
# KEEP=1.
#
#   cargo build -p bulkhead -p bulkhead-priv                   # as a user
#   cargo build -p bulkhead-runner
#   cargo build -p bulkhead-resolve -p bulkhead-admin -p bulkhead-metrics -p bulkhead-sql
#   sh scripts/priv-e2e-rcd.sh "$PWD"                           # as root
set -eu
SRC=${1:?checkout}
T=$SRC/target/debug
id bulkhead >/dev/null 2>&1 || pw useradd bulkhead -d /nonexistent -s /usr/sbin/nologin -c "Bulkhead node"
NUID=$(id -u bulkhead); NGID=$(id -g bulkhead)
install -m 0755 $T/bulkhead-priv /usr/local/sbin/bulkhead-priv
install -m 0755 $T/bulkhead /usr/local/bin/bulkhead
install -d -m 0755 /usr/local/libexec/bulkhead
install -m 0755 $T/bulkhead-runner /usr/local/libexec/bulkhead/bulkhead-runner
install -m 0755 $T/bulkhead-resolve /usr/local/libexec/bulkhead/bulkhead-resolve
install -m 0755 $T/bulkhead-admin /usr/local/libexec/bulkhead/bulkhead-admin
install -m 0755 $T/bulkhead-metrics /usr/local/libexec/bulkhead/bulkhead-metrics
install -m 0755 $T/bulkhead-sql /usr/local/libexec/bulkhead/bulkhead-sql
install -d -m 0755 /usr/local/etc/bulkhead
service bulkhead onestop 2>/dev/null || true
rm -rf /var/db/bulkhead; install -d -m 0700 -o bulkhead -g bulkhead /var/db/bulkhead
# The remote admin API (ROADMAP DA7): the admin key and the operators file,
# the node's, in a directory the helper binds into the admin driver's jail;
# an operator's key for the CLI.
K=/var/db/bulkhead-admin; rm -rf $K; install -d -m 0700 -o bulkhead -g bulkhead $K
W=/var/db/bulkhead-e2e; rm -rf $W; install -d -m 0700 $W
/usr/local/bin/bulkhead cluster keygen $K/admin.key >/dev/null
/usr/local/bin/bulkhead cluster keygen $W/operator.key >/dev/null
ADMIN_KEY=$(/usr/local/bin/bulkhead cluster pubkey $K/admin.key)
printf '[operators]\ne2e = ["%s"]\n' "$(/usr/local/bin/bulkhead cluster pubkey $W/operator.key)" > $K/operators.toml
chown bulkhead:bulkhead $K/admin.key $K/operators.toml; chmod 0600 $K/admin.key $K/operators.toml
printf 'e2e-secret\n' > $K/pg.password
chown bulkhead:bulkhead $K/pg.password; chmod 0600 $K/pg.password
cat > /usr/local/etc/bulkhead/node.toml <<TOML
node = 1
data_dir = "/var/db/bulkhead"

# A port below 1024: the node, not root, has the helper bind it (ROADMAP
# PS3b; port 80 in priv.conf's ports).
[metrics]
address = "127.0.0.1:80"

[admin]
address = "127.0.0.1:7443"
key = "$K/admin.key"
operators = "$K/operators.toml"

[secrets.pg]
file = "$K/pg.password"

# Each starts its driver, bulkhead-sql-<name>; its pool opens a connection
# on the first statement.
[datasources.main]
host = "/tmp"
dbname = "app"
user = "app"

[datasources.shop]
host = "localhost"
dbname = "shop"
user = "shop"
password_secret = "pg"

# The node's resolver (bulkhead-resolve) beside the helper's runner, in
# every instance's jail.
[native]
runner = "/usr/local/libexec/bulkhead/bulkhead-runner"

[priv]
TOML
cat > /usr/local/etc/bulkhead/priv.conf <<CONF
node = /usr/local/bin/bulkhead
node_args = node --config /usr/local/etc/bulkhead/node.toml
node_uid = $NUID
node_gid = $NGID
runner = /usr/local/libexec/bulkhead/bulkhead-runner
uids = 60000-60999
state = /var/db/bulkhead-priv
binds = /var/db/bulkhead, $K
memory_max = 2GiB
pids_max = 1024
ports = 80
CONF
chmod 0600 /usr/local/etc/bulkhead/priv.conf
install -m 0555 $SRC/examples/freebsd/bulkhead /usr/local/etc/rc.d/bulkhead
sysrc bulkhead_enable=YES >/dev/null
service bulkhead start
for i in $(seq 100); do [ -S /var/db/bulkhead/admin.sock ] && break; sleep 0.1; done
echo "== processes"
ps -ax -o user= -o pid= -o ppid= -o command= | grep -E "bulkhead" | grep -v -E "grep|ps1-rcd"
echo "== the datasources' drivers (ROADMAP DA3)"
ps -ax -o uid= -o jid= -o pid= -o command= | grep "bulkhead-sql" | grep -v grep
# A driver reads its password file under /driver/ as it starts: shop's
# running is the file read; the node would not start without it.
echo "drivers: $(pgrep -f "bulkhead-sql" | wc -l | tr -d ' ') (2 expected), \
users: $(ps -ax -o uid= -o command= | grep bulkhead-sql | grep -v grep | awk '{print $1}' | sort -u | wc -l | tr -d ' ')"
echo "== status"; service bulkhead status
echo "== what an app's user reaches of the node's (ROADMAP PS2; no app is deployed here)"
D=/var/db/bulkhead
for p in $D $D/state.json $D/kv $D/bundles $D/compiled $D/priv; do
    [ -e "$p" ] && stat -f '%Lp %Su:%Sg %N' "$p"
done
as_app() { /usr/sbin/chroot -u 60000 -g 60000 / "$@"; }
for p in $D/state.json $D/kv $D/bundles $D/creation; do
    [ -e "$p" ] || continue
    if as_app test -r "$p"; then echo "READABLE by the app: $p"; else echo "refused to the app: $p"; fi
done
echo "== admin as the node's user"
su -m bulkhead -c "/usr/local/bin/bulkhead --socket /var/db/bulkhead/admin.sock list" || true
echo "== metrics (bulkhead-metrics) on port 80, bound by the helper (ROADMAP PS3b)"
fetch -qo - http://127.0.0.1:80/metrics | head -3
fetch -qo /dev/null http://127.0.0.1:80/metrics || { echo "FAILED: nothing served on port 80"; exit 1; }
echo "the node's user: $(ps -o uid= -p "$(pgrep -x bulkhead | head -1)" | tr -d ' ') (not 0)"
echo "== remote admin (bulkhead-admin, signed requests)"
/usr/local/bin/bulkhead --remote 127.0.0.1:7443 --node-key "$ADMIN_KEY" --key $W/operator.key list
echo "== reload"; service bulkhead reload; sleep 1
echo "== stop"; service bulkhead stop; sleep 1
echo "left: $(pgrep -x bulkhead | wc -l | tr -d ' ') nodes, $(pgrep -x bulkhead-priv | wc -l | tr -d ' ') helpers"
echo "== log"; tail -12 /var/log/bulkhead.log
sysrc -x bulkhead_enable
if [ "${KEEP:-0}" != 1 ]; then
    rm -f /usr/local/etc/rc.d/bulkhead /usr/local/sbin/bulkhead-priv /usr/local/bin/bulkhead /var/log/bulkhead.log
    rm -rf /usr/local/libexec/bulkhead /usr/local/etc/bulkhead /var/db/bulkhead /var/db/bulkhead-priv $K $W
    pw userdel bulkhead
    echo "== removed"
fi
