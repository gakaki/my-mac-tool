# Cross-Mac sync benchmarks

Measured on 2026-11-02 from the local Apple M4 Mac to an Apple M4 Mac at
`g@192.168.32.6:22`. Both Macs were on the same LAN and passwordless SSH was
already configured.

## 1,000-file fixture

- 1,000 empty regular files, 40 directories

| Engine | Configuration | Time | Result |
| --- | ---: | ---: | --- |
| tar over SSH | 1 stream | 0.28 s | 1,000/1,000 files |
| openrsync | 1 process | 0.51 s | 1,000/1,000 files |
| openrsync | 4 processes | 0.45 s | 1,000/1,000 files |
| parsync | 4 SFTP workers | 30.77 s | 1,000/1,000 files |
| parsync | 8 SFTP workers | 19.77 s | 1,000/1,000 files |
| msy 0.6.15 | 10 SSH workers | 5.55 s | 1,000/1,000 files |
| rclone 1.75.2 SFTP | 16 transfers, 32 SFTP concurrency | 17.79 s | 1,000/1,000 files |
| rsy 1.2.0 SSH | 8 workers | stopped after 97.53 s | 0/1,000 files |

## 20,000-file fixture

- 20,000 empty regular files
- 80 directories
- Purpose: measure per-file metadata and SSH/SFTP overhead, not disk throughput

| Engine | Configuration | Time | Result |
| --- | ---: | ---: | --- |
| tar over SSH | 1 stream | 1.10 s | 20,000/20,000 files |
| openrsync | 1 process | 3.75 s | 20,000/20,000 files |
| openrsync | 4 processes | 2.53 s | 20,000/20,000 files |
| parsync | 8 SFTP workers | >127.07 s | stopped after 8,071/20,000 files |

## 20,000-file content fixture over SSH/SFTP

- 20,000 regular files containing 4 KiB each (80 MB logical payload)
- 80 directories
- The remote host left the network during the rclone run, so that result is not
  a completed throughput measurement.

| Engine | Configuration | Time | Result |
| --- | ---: | ---: | --- |
| tar over SSH | 1 stream | 4.45 s | 20,000/20,000 files |
| msy 0.6.15 | 10 SSH workers | 99.35 s | 20,000/20,000 files |
| rclone 1.75.2 SFTP | 16 transfers, 32 SFTP concurrency | >112.36 s | interrupted after remote disconnect |
| rsy 1.2.0 | 8 workers | not run | SSH protocol stalled on the smaller fixture |

msy's remote mode requires both `sy` and `sy-remote` on the remote host. Its
reported transfer duration for the 20,000-file test was invalid (`0 ms` and
`1773.95 TB/s`), so only the measured wall time is used here.

## Rust copy-tool investigation

### msy and sy

`msy` 0.6.15 is the renamed/forked `sy` implementation published by
[pepa65/msy](https://github.com/pepa65/msy). The package is native Rust, uses
parallel workers and can preserve timestamps, permissions, xattrs, ACLs and
hard links when configured. At the time of measurement it had 3 GitHub stars
and 519 downloads. The upstream [nijaru/sy](https://github.com/nijaru/sy) had
40 stars. Both projects share the same package features and the `sy` binary.

The crates.io source needed a one-line `unsafe extern "C"` compatibility patch
before it compiled with Rust 1.99. No project code was changed for the test.
The remote test installed temporary `sy` and `sy-remote` binaries in the remote
user's Cargo bin directory. The build host changed networks and made the remote
Mac unreachable before those two temporary binaries could be removed. msy was
correct but much slower than a single tar stream for a large metadata-heavy
tree.

### rsy

The matching project is [pranavgundu/rsy](https://github.com/pranavgundu/rsy),
a native Rust local-copy tool with SSH/daemon target syntax. It had 1 star and
zero forks. Its published Linux benchmark is reproducible locally on APFS:
20,000 x 4 KiB took 0.75 s versus 2.84 s for openrsync (3.79x). This is not a
network-copy result: the source and destination were on the same APFS volume,
so the tool used clonefile and did not copy 80 MB of file data.

The SSH test used the official macOS arm64 binary and its matching remote
binary. It opened an SSH server process but transferred zero files in 97.53 s
with both sides idle. The advertised remote mode is therefore not usable for
this Mac-to-Mac workflow until that protocol bug is fixed.

### tsync

[karrick/tsync](https://github.com/karrick/tsync) is Go, not a Rust project or
a Rust rewrite. It is a direct gobsp stream and can be reimplemented in Rust,
but doing so would only reproduce its tradeoffs:

- 1,000 empty files: 0.39 s, versus 0.38 s for tar.
- 20,000 empty files: 1.06 s, versus 1.18 s for tar.
- 20,000 x 4 KiB files: 4.00 s, versus 4.45 s for tar.
- 256 MiB single file: 12.83 s versus 13.81 s for tar, but 812,318,720 bytes
  peak receiver RSS versus 1,998,848 bytes for tar.
- It buffers each complete file at both sender and receiver, does not preserve
  xattrs or ACLs, does not preserve hard links, and does not restore symlink
  mode/mtime. Its xxHash64 detects corruption but is not a cryptographic
  integrity check.

The 7-10% small-file gain does not justify replacing a stable system tar with a
two-star experimental protocol that loses macOS metadata and has pathological
large-file memory use.

### rclone

rclone is mature and excellent for cloud/object storage, but its SFTP backend
adds per-file protocol work. On the same LAN it took 17.79 s for 1,000 empty
files compared with 0.28 s for tar and 5.55 s for msy. A 20,000-file run was
interrupted after 112.36 s when the remote Mac left the network, so no complete
20,000-file rclone speed is claimed.

## fast_rsync algorithm-only test

`dropbox/fast_rsync` 0.2.0 was built in release mode and measured on the local
Apple M4 with a 4,096-byte block size and an 8-byte MD4 hash. The benchmark
created deterministic pseudo-random data, then verified that applying each
delta reproduced the edited input exactly.

| Operation | Input | Time | Output / throughput |
| --- | ---: | ---: | ---: |
| signature | 4 MiB | 1.916 ms | 12,300 bytes; 2.04 GiB/s |
| diff, 64 KiB replacement | 4 MiB | 3.067 ms | 69,657-byte delta |
| apply | 4 MiB | 0.092 ms | 42.48 GiB/s |
| signature | 64 MiB | 32.384 ms | 1.93 GiB/s |
| diff, 1 MiB replacement | 64 MiB | 54.105 ms | 1,048,601-byte delta |
| apply | 64 MiB | 3.484 ms | 17.94 GiB/s |

This is an in-memory rsync algorithm benchmark, not an SSH copy. `fast_rsync`
does not implement file traversal, SSH, directory metadata, progress, resume or
a receiver protocol. Its legacy MD4 signature is also not a security boundary;
the project explicitly says callers must verify the patched output with a real
hash such as SHA-256.

The openrsync single-process run used about 190 MB peak RSS. The tar stream run
used about 233 MB peak RSS. The 4-process openrsync test used about 4.66 seconds
of aggregate CPU time.

The real Desktop inventory contains 1,226,352 files, 146,839 directories and
61.7 GB. A complete NUL-safe inventory scan took 32.568 seconds and produced
1,389,867 lines (208 MB).

## Decision

- Use one tar stream for the first mirror. It avoids repeated remote metadata
  scans and was 2.3x faster than four openrsync processes on the small-file
  fixture.
- Record a local completion marker after the stream succeeds. Later runs use
  path-contiguous, byte-aware openrsync batches for incremental transfer.
- Keep parsync available as an evaluated alternative, but never make it the
  default: its per-file SFTP behavior is much slower for large file counts.
- Do not pre-create the 146,839 destination directories serially from Rust.
- Stream archive bytes into progress so the UI does not sit at 0% while tar is
  indexing and extracting directories.

## Cable and AirDrop findings

Apple documents Thunderbolt Bridge as a network service, while an ordinary
USB 3.2 Type-C data cable does not expose an IP network between two Macs. A
cable-to-Ethernet path works through the normal Ethernet network service. For a
direct Mac-to-Mac network, both ends need compatible Thunderbolt/USB4 hardware
and a Thunderbolt cable, with Thunderbolt Bridge enabled on both Macs.

AirDrop remains useful for a small selection of files, but Apple explicitly
recommends other sync methods for many files. The GUI therefore opens AirDrop
and reveals the completed local folder for manual sharing, while large mirrors
continue over SSH, LAN Ethernet or Thunderbolt Bridge.

References:

- https://support.apple.com/en-us/102613
- https://support.apple.com/guide/mac-help/use-airdrop-to-send-items-to-nearby-devices-mh35868/mac
