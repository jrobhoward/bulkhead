#!/bin/sh
# S35 on FreeBSD, as root: requests and their checks, times (a VM: function
# only), what an app sees at its quota (plain, and through a nullfs view in
# a jail as the helper builds one), busy datasets, and `zfs` under
# no_new_privs (the helper's fence on FreeBSD).
#
#   run-freebsd.sh BIN POOL_PARENT      e.g. run-freebsd.sh target/release/s35 zroot
#
# Makes POOL_PARENT/s35 (mounted at /srv/s35) and removes it at the end.
set -u
BIN=$(realpath "$1")
BINDIR=$(dirname "$BIN")
P=$2/s35
MP=/srv/s35
ZFS=/sbin/zfs
APP=60935
R=/var/run/s35root
req() { echo "\$ req $*"; "$BIN" req "$P" "$ZFS" "$@"; }

# A jail root as bulkhead-priv's freebsd.rs builds it: tmpfs, read-only
# nullfs views of the libraries and the program's directory, the volume
# read-write without exec or set-user-id; and /bin, which the helper's
# has not, for the shell that holds a file open below.
jail_up() {
  mkdir -p $R && mount -t tmpfs -o mode=0755 tmpfs $R &&
  for d in /bin /lib /libexec /usr/lib; do mkdir -p $R$d && mount -t nullfs -o ro $d $R$d; done &&
  mkdir -p $R/s35 $R/data $R/dev &&
  mount -t nullfs -o ro "$BINDIR" $R/s35 &&
  mount -t nullfs -o rw,noexec,nosuid $MP/orders $R/data &&
  jail -c name=s35 path=$R persist
}
jail_down() {
  jail -r s35 2>/dev/null
  for d in data s35 usr/lib libexec lib bin; do umount $R/$d 2>/dev/null; done
  umount $R 2>/dev/null
}

echo "# S35 on $(uname -srm), $(zfs version | head -1), $(date -u +%FT%TZ)"
echo "load: $(sysctl -n vm.loadavg)"

echo; echo "## The parent: canmount=off, its children mounted below it"
zfs destroy -r "$P" 2>/dev/null
zfs create -o canmount=off -o mountpoint=$MP "$P"
req create orders 67108864
req list
ls -ld $MP $MP/orders
zfs get -H -o property,value exec,setuid,devices,refquota,mountpoint,canmount "$P/orders"

echo; echo "## The helper's checks"
req create ../x 1
req create a@b 1
req quota orders 0

echo; echo "## Times (50 rounds; a VM on a busy host: function only)"
"$BIN" bench "$P" "$ZFS" 50

echo; echo "## At the quota, as the app's user"
chown $APP:$APP $MP/orders && chmod 0770 $MP/orders
"$BIN" fill $MP/orders $APP
echo "(at the quota, a snapshot taken, a file removed: refquota counts what the"
echo " dataset refers to, not what its snapshots keep)"
zfs snapshot "$P/orders@full"; rm -f $MP/orders/fill-1; sync; sleep 6
dd if=/dev/random of=$MP/orders/after-snap bs=1048576 count=1 status=none conv=fsync && echo "write after remove, snapshot kept: ok"
zfs get -H -p -o property,value used,referenced,usedbysnapshots "$P/orders"
zfs destroy "$P/orders@full"
rm -f $MP/orders/*

echo; echo "## The quota lowered below what is used"
dd if=/dev/random of=$MP/orders/forty bs=1048576 count=40 status=none; sync
req quota orders 16777216
rm -f $MP/orders/*

echo; echo "## Through a nullfs view in a jail (the helper's jail root)"
jail_up && jexec s35 /s35/s35 fill /data $APP
jail_down
rm -f $MP/orders/*

echo; echo "## exec=off"
cp /usr/bin/true $MP/orders/t && chmod 0755 $MP/orders/t
$MP/orders/t && echo "executed" || echo "refused to execute"
rm -f $MP/orders/t

echo; echo "## Busy: a jail holds a nullfs view of it, a file open"
echo a > $MP/orders/a
jail_up
jexec s35 /bin/sh -c 'exec 3>>/data/held; echo held >&3; exec /bin/sleep 40' &
sleep 1
req snap orders pre-1
echo b > $MP/orders/b
req rollback orders pre-1
ls $MP/orders
req destroy orders
zfs list -H -o name,mounted "$P/orders" 2>&1
jail_down
req destroy orders
req create orders 67108864

echo; echo "## Busy on the host: a process with its working directory in it"
(cd $MP/orders && exec sleep 20) &
HOLDER=$!
sleep 1
req destroy orders
zfs list -H -o name,mounted "$P/orders" 2>&1
kill $HOLDER; wait $HOLDER 2>/dev/null
req destroy orders

echo; echo "## zfs under no_new_privs"
for args in "create -o refquota=67108864 -o exec=off -o setuid=off -o devices=off $P/fenced" \
            "set refquota=134217728 $P/fenced" "snapshot $P/fenced@t" "rollback $P/fenced@t" \
            "list -H -p -r $P" "destroy -r $P/fenced"; do
  proccontrol -m nonewprivs -s enable $ZFS $args >/dev/null; echo "zfs ${args%% *}: status $?"
done

echo; echo "## Clean up"
zfs destroy -r "$P" && echo "removed $P"
rmdir $R $MP 2>/dev/null
