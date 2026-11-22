#!/bin/sh
# S35 on Linux with OpenZFS, as root: requests and their checks, times,
# what an app sees at its quota (plain, under Landlock, through a bind in
# a mount namespace as the helper's minimal root has it), busy datasets,
# and `zfs` under the helper's own fence.
#
#   run-linux.sh BIN POOL_PARENT      e.g. run-linux.sh target/release/s35 tank/bulkhead
#
# Makes POOL_PARENT/s35 (mounted at /srv/s35) and removes it at the end.
set -u
BIN=$(realpath "$1")
P=$2/s35
MP=/srv/s35
ZFS=$(command -v zfs)
APP=60935
R=/run/s35root
req() { echo "\$ req $*"; "$BIN" req "$P" "$ZFS" "$@"; }

echo "# S35 on $(uname -srm), $(zfs version | head -1), $(date -u +%FT%TZ)"
echo "load: $(cut -d' ' -f1-3 /proc/loadavg)"

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
req create -o 1
req create big 999999999999999
req quota orders 0

echo; echo "## Times (50 rounds)"
"$BIN" bench "$P" "$ZFS" 50

echo; echo "## At the quota, as the app's user"
chown $APP:$APP $MP/orders && chmod 0770 $MP/orders
"$BIN" fill $MP/orders $APP
echo "(at the quota, a snapshot taken, a file removed: refquota counts what the"
echo " dataset refers to, not what its snapshots keep)"
zfs snapshot "$P/orders@full"; rm -f $MP/orders/fill-1; sync; sleep 6
dd if=/dev/urandom of=$MP/orders/after-snap bs=1M count=1 status=none conv=fsync && echo "write after remove, snapshot kept: ok"
zfs get -H -p -o property,value used,referenced,usedbysnapshots "$P/orders"
zfs destroy "$P/orders@full"
rm -f $MP/orders/*

echo; echo "## The quota lowered below what is used, then raised at the quota"
dd if=/dev/urandom of=$MP/orders/forty bs=1M count=40 status=none; sync
req quota orders 16777216
zfs get -H -o property,value used,referenced,refquota "$P/orders"
dd if=/dev/urandom of=$MP/orders/more bs=1M count=1 status=none conv=fsync 2>&1 | tail -1
req quota orders 134217728
dd if=/dev/urandom of=$MP/orders/more bs=1M count=1 status=none conv=fsync && echo "write after raise: ok"
req quota orders 67108864
rm -f $MP/orders/*

echo; echo "## Landlock on the mountpoint alone"
"$BIN" landlock $MP/orders $APP
rm -f $MP/orders/*

echo; echo "## Through a bind in a mount namespace (the helper's minimal root)"
mkdir -p $R
unshare -m --propagation private sh -c "
  mount -t tmpfs -o mode=0755 none $R && mkdir $R/data &&
  mount --bind $MP/orders $R/data && mount -o remount,bind,nosuid,nodev,noexec $R/data &&
  '$BIN' fill $R/data $APP"
rm -f $MP/orders/*

echo; echo "## exec=off"
cp /bin/true $MP/orders/t && chmod 0755 $MP/orders/t
$MP/orders/t && echo "executed" || echo "refused to execute"
rm -f $MP/orders/t

echo; echo "## Busy: an instance holds it bound in its mount namespace, a file open"
echo a > $MP/orders/a
unshare -m --propagation private sh -c "
  mount -t tmpfs none $R && mkdir $R/data && mount --bind $MP/orders $R/data &&
  exec 3>>$R/data/held && echo held >&3 && exec sleep 40" &
HOLDER=$!
sleep 1
req snap orders pre-1
echo b > $MP/orders/b
req rollback orders pre-1
ls $MP/orders
req quota orders 33554432
req destroy orders
zfs list -H -o name,mounted "$P/orders" 2>&1
echo "(the instance still writes through its bind:)"
nsenter -t $(pgrep -P $HOLDER sleep || echo $HOLDER) -m sh -c "echo late >> $R/data/held && echo ok"
zfs mount "$P/orders" && echo "mounted again on the host" && cat $MP/orders/held
kill $HOLDER; wait $HOLDER 2>/dev/null
sleep 1
zfs list -H -o name,mounted "$P/orders" 2>&1
req destroy orders
req create orders 67108864

echo; echo "## Busy on the host: a process with its working directory in it"
(cd $MP/orders && exec sleep 20) &
HOLDER=$!
sleep 1
req destroy orders
kill $HOLDER; wait $HOLDER 2>/dev/null
req destroy orders

echo; echo "## zfs under the helper's fence (log mode: each call off its list is logged)"
dmesg --clear 2>/dev/null || true
for args in "create -o refquota=67108864 -o exec=off -o setuid=off -o devices=off $P/fenced" \
            "set refquota=134217728 $P/fenced" "snapshot $P/fenced@t" "rollback $P/fenced@t" \
            "list -H -p -r $P" "destroy -r $P/fenced"; do
  "$BIN" fenced log -- $ZFS $args >/dev/null; echo "log  zfs ${args%% *}: status $?"
done
dmesg | grep 'type=1326' | grep -o 'comm="[^"]*".*syscall=[0-9]*' | sed 's/ exe=.*syscall=/ syscall=/' | sort | uniq -c
echo "(trap mode: as the helper runs it)"
for args in "create -o refquota=67108864 -o exec=off -o setuid=off -o devices=off $P/fenced" \
            "set refquota=134217728 $P/fenced" "snapshot $P/fenced@t" "rollback $P/fenced@t" \
            "list -H -p -r $P" "destroy -r $P/fenced"; do
  "$BIN" fenced trap -- $ZFS $args >/dev/null 2>&1; echo "trap zfs ${args%% *}: status $?"
done
zfs destroy -r "$P/fenced" 2>/dev/null
"$BIN" fenced trap -- $ZFS create -o refquota=67108864 $P/fenced >/dev/null 2>&1
zfs list -H -o name,mounted "$P/fenced"; zfs destroy -r "$P/fenced"
echo "(controls: unshare is off the list)"
"$BIN" fenced trap -- /usr/bin/unshare -m /bin/true >/dev/null 2>&1; echo "trap unshare: status $? (159: SIGSYS)"
dmesg --clear 2>/dev/null || true
"$BIN" fenced log -- /usr/bin/unshare -m /bin/true; echo "log  unshare: status $?"
dmesg | grep -c 'type=1326' | sed 's/^/kernel lines for it: /'

echo; echo "## Clean up"
zfs destroy -r "$P" && echo "removed $P"
rmdir $R $MP 2>/dev/null
