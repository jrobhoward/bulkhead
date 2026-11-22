#!/bin/sh
# setup-freebsd.sh: as root, once per machine (snapshot a VM first):
# mosquitto on 127.0.0.1 and 10.40.0.1, port 1883; lo1 with 10.40.0.1 (the
# host's target), 10.40.0.2 (the pf run's jail) and 10.40.0.3 (a target
# outside the rules); 127.0.0.2 on lo0.
set -eu
pkg info -e mosquitto || pkg install -y mosquitto
ifconfig lo1 >/dev/null 2>&1 || ifconfig lo1 create
for a in 10.40.0.1 10.40.0.2 10.40.0.3; do ifconfig lo1 inet "$a/32" alias; done
ifconfig lo0 inet 127.0.0.2/32 alias
printf 'listener 1883 127.0.0.1\nlistener 1883 10.40.0.1\nallow_anonymous true\n' > /usr/local/etc/mosquitto/s40.conf
pgrep -x mosquitto >/dev/null || mosquitto -d -c /usr/local/etc/mosquitto/s40.conf
