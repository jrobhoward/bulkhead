#!/bin/sh
# setup-linux.sh: as root, once per machine: mosquitto on 127.0.0.1:1883.
set -eu
command -v mosquitto >/dev/null || apt-get install -y mosquitto
printf 'listener 1883 127.0.0.1\nallow_anonymous true\n' > /etc/mosquitto/conf.d/s40.conf
systemctl restart mosquitto
