# S35: ZFS datasets as app volumes

Spike S35 of `docs/ROADMAP.md` ("ZFS volumes"). It asks whether the privileged helper can give each app volume a ZFS dataset of its own, with a quota, through the `zfs` command. It also asks what an app sees at its quota, and whether the volume is still reached the same way through the runner's Landlock rule, the helper's minimal root (a bind in a mount namespace) and the helper's jail (a nullfs view). It decides the requests the helper takes, the dataset layout and its lifecycle, for ZV1.

Run 2026-11-22, as root:
- **Linux:** the Raspberry Pi 5 (aarch64, Ubuntu 26.04, Linux 7.0, OpenZFS 2.4.1), idle (load 0.1). The pool `tank` is on its NVMe drive.
- **FreeBSD:** the FreeBSD 15.0 VM (amd64, 4 vCPUs, OpenZFS 2.4.0-rc4, ZFS root `zroot`) on the 16-core machine, which was running TLC on 12 cores. Its times are function checks only.

Results: `results/pi5-linux-2026-11-22.md` and `results/freebsd-vm-2026-11-22.md`, each one run of the script below.

## What was built

One binary, `s35`, with no async runtime and `libc` as its only dependency, plus, on Linux, `bulkhead-priv` (for its seccomp allow-list), `seccompiler` and `landlock`:

- **`src/zfs.rs`** holds the helper's side as proposed. It covers the requests, the checks against the helper's own configuration, the `zfs` command line each request becomes, and `zfs list -H -p` parsed.
- **`s35 req`** runs one request; **`s35 bench`** times each request over 50 rounds on fresh datasets.
- **`s35 fill DIR [UID]`** is an app at its quota. As the app's user, it writes 1 MiB pieces of incompressible data into 16 MiB files, each `fsync`ed, until a write is refused. It then tries one more file, removes one, and writes again. **`s35 landlock`** does the same under a Landlock rule on the volume alone, as the runner gives one.
- **`s35 fenced trap|log -- CMD`** (Linux) runs a command under the helper's own fence, as `bulkhead-priv`'s `fence_self` sets it: `no_new_privs`, its nine capabilities and its seccomp allow-list (`bulkhead_priv::seccomp::program`). A `zfs` child of the helper would inherit all three.
- **`scripts/run-linux.sh`, `scripts/run-freebsd.sh`** run every scenario below.

```sh
# on the Pi (as root), and on the FreeBSD VM with run-freebsd.sh ... zroot
cargo build --release
sh scripts/run-linux.sh target/release/s35 tank/bulkhead
```

### The requests

The node names a volume, never a dataset. The helper puts the volume under the one parent its own configuration gives. Nothing from the node reaches the command line except a checked name, a checked snapshot tag and numbers.

| request | `zfs` | the helper checks |
| --- | --- | --- |
| `Create { volume, refquota, reservation }` | `create -o exec=off -o setuid=off -o devices=off -o canmount=on -o refquota=N [-o refreservation=N] PARENT/VOLUME` | name `[a-z0-9-]{1,64}`, not starting with `-`; quota 1 to its `max_quota`; reservation at most the quota |
| `SetQuota { volume, refquota }` | `set refquota=N PARENT/VOLUME` | as above |
| `Snapshot { volume, tag }` | `snapshot PARENT/VOLUME@TAG` | tag as a name |
| `Rollback { volume, tag }` | `rollback PARENT/VOLUME@TAG`, without `-r`, so only to the latest snapshot | as above |
| `DestroySnapshot { volume, tag }` | `destroy PARENT/VOLUME@TAG` | as above |
| `Destroy { volume }` | `destroy -r PARENT/VOLUME` (its snapshots; never `-R`, clones) | as above |
| `List` | `list -H -p -r -t filesystem,snapshot -o name,used,referenced,refquota,refreservation,mountpoint,mounted PARENT` | none |

The name rule is narrower than ZFS's, which also allows `_ . :` and spaces. That way no name can be read as an option, a path component, a snapshot (`@`) or a bookmark (`#`). `zfs` runs by absolute path, with an empty environment and a fixed `PATH`.

## Findings

- **The `zfs` command is fast enough.** On the Pi, each run costs about 9 ms before it does anything: `zfs list` takes 8.9 ms, against 0.8 ms for `/bin/true`. That is libzfs opening the pool. Medians over 50 rounds, with p90 in brackets:

  | | create with a quota | set quota | snapshot | rollback | destroy a snapshot | destroy |
  | --- | --- | --- | --- | --- | --- | --- |
  | Pi | 34 ms (70) | 9.4 ms (9.9) | 10.5 ms (11.9) | 17 ms (18) | 10 ms (11) | 17 ms (37) |

  The worst of 50 was 48 to 115 ms, and up to 337 ms in an earlier run of the same script, when a transaction group was being written. The VM took 3 to 4 times the Pi's times on a busy host. These requests come at a deploy, an upgrade or a quota change, never per message, so a few tens of milliseconds don't matter. This confirms the command over libzfs (CDDL), as the roadmap planned.
- **One parser for both systems.** `zfs list -H -p` prints the same columns and forms on OpenZFS 2.4 on Linux and on FreeBSD: tab-separated, sizes in bytes, `0` for no quota, and `-` in a snapshot's `refquota`, `mountpoint` and `mounted`. `parse_list` reads both, and `zfs_tests.rs` holds the sample.
- **What an app sees at its quota** is the same on both systems, and the same through all three fences:
  - **The refusal:** a write is refused with `EDQUOT` (122 on Linux, 69 on FreeBSD), after 63 MiB of a 64 MiB `refquota`. The last MiB is metadata.
  - **No overshoot:** with each 16 MiB file `fsync`ed, the writes never went past the quota.
  - **`statvfs`:** it reports the quota as the filesystem's size and 0 bytes available, so `df` inside an app shows its quota.
  - **Recovery:** once a file is removed, writes go on.
  - **Fences checked:** the plain path, a Landlock rule on the mountpoint alone (`FullyEnforced`, with a write outside it refused with `EACCES`), a bind into a tmpfs root in a mount namespace (Linux, as the helper's minimal root), and a nullfs view in a jail (FreeBSD, as the helper's jail root).
- **`refquota`, not `quota`.** At the quota, with a snapshot taken, removing a 16 MiB file let the next write through on both systems. The snapshot still kept those 16 MiB (`usedbysnapshots`). A snapshot around an upgrade (ZV3) therefore never leaves an app unable to free space, which it would under `quota`, since `quota` counts snapshots.
- **A quota can't be lowered below what is used.** `zfs set refquota` refuses with "size is less than current used or reserved space" on both systems. An upgrade asking for less than the volume holds fails cleanly at that step. A raise takes effect at once, even for an app that is at its quota.
- **The fixed properties hold.** `exec=off` refuses to run a program copied into the volume (`EACCES`), on both systems, under the runner's own rule against executing from a volume. `setuid=off` and `devices=off` are accepted on both (FreeBSD's device nodes live in devfs alone, so `devices` changes nothing there). They are set on each volume whatever the parent says.
- **The layout:** a parent with `canmount=off` and a mountpoint, its children mounted below it at `MOUNTPOINT/VOLUME` by inheritance. So the helper takes no path from anyone. It reads the mountpoint back with `List` and gives that to the node. The Pi's `tank/bulkhead` has `mountpoint=none`: the operator gives the parent a mountpoint, or its children are not mounted.
- **A new dataset's root belongs to root, mode 0755.** PS2's `Share` gives a read-write volume to its app only if the node owns it or the app does already. So `Create` makes the root the app's itself (its user and group, 0770, as `Share` would), since the helper holds the app's user.
- **Snapshots and rollbacks work while an instance holds the volume.** That includes a bind in an instance's mount namespace or a nullfs view in a jail, with a file open. A rollback changes the files under a running instance, though: a file written after the snapshot disappeared from under it. ZV3 should roll back only once the app's instances have stopped, which a failed upgrade's revert does anyway.
- **Destroying a volume an instance holds fails, differently on each system:**
  - **Linux:** with a bind in another mount namespace, `zfs destroy` unmounts the dataset on the host first. It then fails with "dataset is busy" and leaves it unmounted on the host, while the instance still writes through its bind. `zfs mount` puts it back.
  - **FreeBSD:** the jail's nullfs view makes the unmount fail ("pool or dataset is busy"), and the dataset stays mounted.
  - **Both:** a process with its working directory in the volume on the host stops the unmount the same way.

  So `Destroy` is refused while any instance of the app runs (the helper knows its instances). On a failure the helper runs `zfs mount` again.
- **`zfs` runs under the helper's fence unchanged.** On Linux, all six requests passed under the helper's seccomp allow-list in trap mode, as the helper runs. In log mode the kernel logged no call off the list. A control, `unshare`, was trapped (status 159) and logged once, so the filter was in force. The nine capabilities are enough: as root, `zfs` inherits the bounding set, and the dataset was made and mounted. On FreeBSD, the six passed under `no_new_privs` (`proccontrol -m nonewprivs`), the helper's whole fence there. The helper's allow-list needs nothing added for ZFS.

## What it decides, for ZV1

- **Requests:** the seven above, in `bulkhead-priv-wire`. The helper's configuration gains `zfs.parent`, `zfs.max_quota` and the program's path. The node's `[volumes.zfs] max_quota` stays a ceiling below the helper's.
- **Layout:** a dataset per app volume, shared by the app's instances, under a parent the operator makes with `canmount=off` and a mountpoint. Only one level below the parent was tried. A volume's dataset is named after its app and volume, `PARENT/APP/VOLUME` or `PARENT/APP-VOLUME`, since two apps may name their volumes alike. That choice is ZV1's to make.
- **Quota:** `refquota`, so snapshots are not counted against the app. The parent's own `quota`, which is the operator's, bounds the total. Lowering below what is used is refused by ZFS, and the node reports it.
- **Lifecycle:**
  - **Create:** at deploy, with the root given to the app's user.
  - **Undeploy:** keeps the dataset.
  - **`bulkhead volume destroy`:** refused while an instance of the app runs, with a remount if a destroy fails.
  - **Rollback (ZV3):** only with the app's instances stopped.
  - **zvols:** not offered.
- **Not measured:**
  - `refreservation`;
  - a parent at its own quota;
  - many datasets, as their count grows;
  - `zfs` on x86-64 Linux;
  - ZFS on the Ubuntu VM, which has no pool yet.
