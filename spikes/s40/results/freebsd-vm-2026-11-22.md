# S40 run: FreeBSD 15.0 VM (amd64, 4 vCPUs) on the 16-core machine

`FreeBSD 15.0-RELEASE amd64, 2026-11-22T23:18:26Z, load { 0.42 0.23 0.14 }`. A function check: the times move with the host, by several µs. Whole output: `raw/freebsd-vm-2026-11-22.txt`.

## Benchmarks

| round | fence | TCP connect and close (p50 / p99, rate) | UDP 64 B | bulk TCP |
| --- | --- | --- | --- | --- |
| 1 | fence none, 2 rules, lo0 | 17.9 / 25.1 µs, 55,053/s | 1350 ns | 2637 MB/s |
| 1 | fence none, 2 rules, lo1 | 16.3 / 23.8 µs, 65,656/s | 950 ns | 2571 MB/s |
| 1 | sockets made by the launcher, passed | 29.1 / 36.6 µs, 35,294/s | 1893 ns | - |
| 2 | fence none, 2 rules, lo0 | 17.8 / 24.9 µs, 55,732/s | 1439 ns | 2543 MB/s |
| 2 | fence none, 2 rules, lo1 | 16.4 / 23.6 µs, 64,321/s | 1238 ns | 2602 MB/s |
| 2 | sockets made by the launcher, passed | 29.3 / 36.7 µs, 34,932/s | 1744 ns | - |
| 1 | fence pf, 2 rules | 23.9 / 35.4 µs, 40,756/s | 1211 ns | 1703 MB/s |
| 1 | fence pf, 16 rules | 24.4 / 34.1 µs, 40,008/s | 1284 ns | 2437 MB/s |
| 1 | fence pf, 64 rules | 23.1 / 32.3 µs, 50,700/s | 1464 ns | 2827 MB/s |
| 2 | fence pf, 2 rules | 23.0 / 37.3 µs, 41,030/s | 1304 ns | 2415 MB/s |
| 2 | fence pf, 16 rules | 22.1 / 31.4 µs, 49,000/s | 1232 ns | 2364 MB/s |
| 2 | fence pf, 64 rules | 25.2 / 35.0 µs, 39,978/s | 1658 ns | 2367 MB/s |
| 1 | fence none, ipfw loaded | 16.2 / 24.0 µs, 61,589/s | 1257 ns | 2390 MB/s |
| 1 | fence ipfw, 2 rules | 8.7 / 19.7 µs, 100,802/s | 1654 ns | 2427 MB/s |
| 1 | fence ipfw, 16 rules | 19.2 / 28.4 µs, 59,226/s | 1647 ns | 2406 MB/s |
| 1 | fence ipfw, 64 rules | 21.3 / 32.5 µs, 47,224/s | 907 ns | 3004 MB/s |
| 2 | fence none, ipfw loaded | 18.0 / 24.7 µs, 54,019/s | 1269 ns | 2522 MB/s |
| 2 | fence ipfw, 2 rules | 18.0 / 26.0 µs, 63,664/s | 1136 ns | 2571 MB/s |
| 2 | fence ipfw, 16 rules | 17.5 / 25.3 µs, 57,802/s | 1412 ns | 2572 MB/s |
| 2 | fence ipfw, 64 rules | 21.5 / 36.9 µs, 44,583/s | 1058 ns | 2444 MB/s |

## MQTT (rumqttc and mosquitto)

- **none:**
  - broker 127.0.0.1:1884 (not in rules): refused errno 61
  - connect to 127.0.0.1:1883: 197.194µs and 125.457µs
  - QoS 0, 64 B: 100000 of 100000 received, 351371 per second
  - QoS 1, 64 B: 20000 of 20000 acknowledged, 133879 per second
- **pf:**
  - broker 10.40.0.1:1884 (not in rules): refused errno 13
  - connect to 10.40.0.1:1883: 256.476µs and 167.898µs
  - QoS 0, 64 B: 100000 of 100000 received, 327775 per second
  - QoS 1, 64 B: 20000 of 20000 acknowledged, 112993 per second
- **ipfw:**
  - broker 127.0.0.1:1884 (not in rules): refused errno 13
  - connect to 127.0.0.1:1883: 256.436µs and 163.229µs
  - QoS 0, 64 B: 100000 of 100000 received, 353705 per second
  - QoS 1, 64 B: 20000 of 20000 acknowledged, 155011 per second

## Probe

| case | none | pf | ipfw |
| --- | --- | --- | --- |
| tcp 127.0.0.1:7001 (allowed) | ok |  | ok |
| tcp 127.0.0.1:7002 (port not in rules) | ok |  | refused: EACCES |
| tcp 127.0.0.2:7001 (address not in rules) | ok |  | refused: EACCES |
| tcp 127.0.0.1:7100 (broad rule; guard if loopback) | ok |  | refused: EACCES |
| tcp 169.254.169.254:7100 (broad rule, link-local) | refused: connection timed out | refused: EACCES | refused: EACCES |
| tcp 0.0.0.0:7100 (broad rule, unspecified) | refused: ENETUNREACH | refused: ENETUNREACH | refused: ENETUNREACH |
| tcp [::ffff:127.0.0.1]:7001 (mapped, allowed) | ok |  | ok |
| tcp [::ffff:127.0.0.1]:7002 (mapped, port not in rul | es) ok |  | es) refused: EACCES |
| udp sendto 127.0.0.1:7001 (allowed) | ok 1 B |  | ok 1 B |
| udp sendto 127.0.0.1:7002 (port not in rules) | ok 1 B |  | refused: EACCES |
| udp sendto 127.0.0.2:7001 (address not in rules) | ok 1 B |  | refused: EACCES |
| udp sendto [::ffff:127.0.0.1]:7002 (mapped, port not | in rules) ok 1 B |  | in rules) refused: EACCES |
| udp connect 127.0.0.1:7002 (port not in rules) | ok |  | ok |
| udp connected to 127.0.0.1:7001, sendto 127.0.0.1:70 | 02 refused: EISCONN |  | 02 refused: EISCONN |
| tcp bind :9001 (in rules) | ok | ok | ok |
| tcp bind :9100 (not in rules) | ok | ok | ok |
| tcp bind :0 (ephemeral) | ok | ok | ok |
| udp bind :9001 (in rules) | ok | ok | ok |
| udp bind :9100 (not in rules) | ok | ok | ok |
| listen :9002, connect 127.0.0.1:9002, accept | ok |  | ok |
| socket AF_INET SOCK_RAW | refused: EPERM | refused: EPERM | refused: EPERM |
| socket PF_ROUTE SOCK_RAW | ok | ok | ok |
| getaddrinfo localhost | ok  | ok [127.0.0.1:1, [::1]:1] | ok  |
| getaddrinfo example.com (DNS) | ok 4 addresses | refused: Name could not be resolved at this time | refused: Name could not be resolved at this time |
| udp bind :9003, datagram from 127.0.0.2 (outside the | rules) ok 22 B |  | rules) ok 22 B |
| tcp 10.40.0.1:7001 (allowed) |  | ok |  |
| tcp 10.40.0.1:7002 (port not in rules) |  | refused: EACCES |  |
| tcp 10.40.0.3:7001 (address not in rules) |  | refused: EACCES |  |
| tcp 10.40.0.1:7100 (broad rule; guard if loopback) |  | ok |  |
| tcp [::ffff:10.40.0.1]:7001 (mapped, allowed) |  | refused: EPROTONOSUPPORT |  |
| tcp [::ffff:10.40.0.1]:7002 (mapped, port not in rul |  | es) refused: EPROTONOSUPPORT |  |
| udp sendto 10.40.0.1:7001 (allowed) |  | ok 1 B |  |
| udp sendto 10.40.0.1:7002 (port not in rules) |  | refused: EACCES |  |
| udp sendto 10.40.0.3:7001 (address not in rules) |  | refused: EACCES |  |
| udp sendto [::ffff:10.40.0.1]:7002 (mapped, port not |  | in rules) refused: EPROTONOSUPPORT |  |
| udp connect 10.40.0.1:7002 (port not in rules) |  | ok |  |
| udp connected to 10.40.0.1:7001, sendto 10.40.0.1:70 |  | 02 refused: EISCONN |  |
| listen :9002, connect 10.40.0.2:9002, accept |  | ok |  |
| udp bind :9003, datagram from 10.40.0.3 (outside the |  | rules) ok 22 B |  |
