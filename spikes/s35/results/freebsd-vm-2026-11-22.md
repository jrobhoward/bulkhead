# S35 on FreeBSD 15.0-RELEASE amd64, zfs-2.4.0-rc4-FreeBSD_g099f69ff5, 2026-11-22T14:03:31Z
load: { 0.13 0.34 0.19 }

## The parent: canmount=off, its children mounted below it
$ req create orders 67108864
ok 95.8 ms
$ req list
ok 4.6 ms
  Row { volume: None, tag: None, used: 196608, referenced: 98304, refquota: 0, refreservation: 0, mountpoint: Some("/srv/s35"), mounted: false }
  Row { volume: Some("orders"), tag: None, used: 98304, referenced: 98304, refquota: 67108864, refreservation: 0, mountpoint: Some("/srv/s35/orders"), mounted: true }
drwxr-xr-x  3 root wheel 3 Nov 22 08:03 /srv/s35
drwxr-xr-x  2 root wheel 2 Nov 22 08:03 /srv/s35/orders
exec	off
setuid	off
devices	off
refquota	64M
mountpoint	/srv/s35/orders
canmount	on

## The helper's checks
$ req create ../x 1
refused: volume "../x": [a-z0-9-]{1,64}, not starting with -
$ req create a@b 1
refused: volume "a@b": [a-z0-9-]{1,64}, not starting with -
$ req quota orders 0
refused: refquota 0: 1 to 53687091200

## Times (50 rounds; a VM on a busy host: function only)
| request | median ms | p90 ms | max ms |
| --- | --- | --- | --- |
| create | 102.5 | 112.1 | 440.5 |
| quota | 39.5 | 45.5 | 122.7 |
| snap | 31.8 | 37.2 | 95.1 |
| rollback | 57.9 | 63.6 | 173.1 |
| unsnap | 31.7 | 37.1 | 158.9 |
| list | 7.6 | 8.2 | 10.0 |
| destroy | 66.3 | 74.5 | 172.7 |
| /bin/true | 0.1 | 0.1 | 0.2 |

## At the quota, as the app's user
statvfs before: size 64.0 MiB, available 63.9 MiB
written=66060288 bytes (63.0 MiB) in 6.141100446s
refused: write at 66060288 bytes: EDQUOT (69: Disc quota exceeded (os error 69))
statvfs at quota: size 64.0 MiB, available 0.0 MiB
a further 1 MiB file: EDQUOT (69: Disc quota exceeded (os error 69))
remove fill-0: ok
statvfs after remove: size 64.0 MiB, available 16.0 MiB
a 1 MiB file after remove: ok
(at the quota, a snapshot taken, a file removed: refquota counts what the
 dataset refers to, not what its snapshots keep)
write after remove, snapshot kept: ok
used	52166656
referenced	35299328
usedbysnapshots	16867328

## The quota lowered below what is used
$ req quota orders 16777216
failed 4.0 ms: cannot set property for 'zroot/s35/orders': size is less than current used or reserved space

## Through a nullfs view in a jail (the helper's jail root)
statvfs before: size 64.0 MiB, available 63.9 MiB
written=66060288 bytes (63.0 MiB) in 5.982873334s
refused: write at 66060288 bytes: EDQUOT (69: Disc quota exceeded (os error 69))
statvfs at quota: size 64.0 MiB, available 0.0 MiB
a further 1 MiB file: EDQUOT (69: Disc quota exceeded (os error 69))
remove fill-0: ok
statvfs after remove: size 64.0 MiB, available 16.0 MiB
a 1 MiB file after remove: ok

## exec=off
scripts/run-freebsd.sh: /srv/s35/orders/t: Permission denied
refused to execute

## Busy: a jail holds a nullfs view of it, a file open
$ req snap orders pre-1
ok 39.1 ms
$ req rollback orders pre-1
ok 69.0 ms
a
held
$ req destroy orders
failed 28.1 ms: cannot unmount '/srv/s35/orders': pool or dataset is busy
zroot/s35/orders	yes
$ req destroy orders
ok 64.9 ms
$ req create orders 67108864
ok 89.2 ms

## Busy on the host: a process with its working directory in it
$ req destroy orders
failed 6.3 ms: cannot unmount '/srv/s35/orders': pool or dataset is busy
zroot/s35/orders	yes
$ req destroy orders
ok 64.8 ms

## zfs under no_new_privs
zfs create: status 0
zfs set: status 0
zfs snapshot: status 0
zfs rollback: status 0
zfs list: status 0
zfs destroy: status 0

## Clean up
removed zroot/s35
