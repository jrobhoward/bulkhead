#!/bin/sh
# cleanup-linux.sh: as root: what setup and a stopped run leave behind.
nft delete table inet s40 2>/dev/null
for d in /sys/fs/cgroup/s40/apps/* /sys/fs/cgroup/s40/apps /sys/fs/cgroup/s40; do
  [ -d "$d" ] && rmdir "$d"
done
rm -rf /opt/s40
rm -f /etc/mosquitto/conf.d/s40.conf
systemctl disable --now mosquitto 2>/dev/null
true
