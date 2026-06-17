# Snapshot transfer

A snapshot is two files — `mem.snap` (the guest RAM dump) and `state.snap` (the vCPU +
device state) — plus, for a networked VM, small metadata files (`net.json`,
`vsock.txt`). `hostd::transfer` streams them over **any byte stream**: loopback, a unix
socket, or TCP between two hosts. The same code path serves the same-host dev runs and
the cross-droplet release runs.

## The wire framing

Each file is framed as:

```
┌──────────┬───────────────┬────────────┬──────────────┬──────────┐
│ name_len │     name      │   length   │     data     │  CRC32   │
│  u16     │   name_len B  │    u64     │  length B    │   u32    │
└──────────┴───────────────┴────────────┴──────────────┴──────────┘
```

A **zero-length name** marks the end of the stream. The receiver verifies the CRC32
before accepting a file; a mismatch is a hard `Checksum` error (corruption in transit),
not a silent accept.

## Chunked, so an 8 GB file never sits in RAM

File data moves in fixed **64 KiB chunks** (`CHUNK = 64 * 1024`). The sender reads a
chunk, updates the running CRC, and writes it; the receiver reads a chunk, updates its
own CRC, and writes it to disk. Peak memory is one chunk per direction regardless of
snapshot size — an 8 GB memory file never has to fit in RAM.

```rust
const CHUNK: usize = 64 * 1024;

let mut hasher = Hasher::new();          // crc32fast
let mut remaining = len;
while remaining > 0 {
    let want = remaining.min(CHUNK as u64) as usize;
    let n = src.read(&mut buf[..want]).await?;
    if n == 0 { return Err(Protocol("shorter than declared length")); }
    hasher.update(&buf[..n]);
    writer.write_all(&buf[..n]).await?;
    remaining -= n as u64;
}
// ... then the u32 CRC trailer
```

## What can go wrong

| Error | Cause |
|-------|-------|
| `Checksum { file, expected, got }` | A file's CRC32 did not match — corruption in transit. The migration aborts cleanly; the snapshot still exists on the source. |
| `Protocol(..)` | The stream did not follow the framing — a bad name length, or a file shorter than its declared length. |
| `Io(..)` | A file or socket I/O error. |

## Deliberate non-goals (v0)

- **Resumability is out of scope.** A failed transfer is retried *whole*. This is safe
  because the snapshot remains on the source until cutover — nothing is lost by
  restarting the transfer.
- The receiver has a bounded **accept timeout** (30 s) so a migration that *stood down*
  (the guest was busy) frees its listening port instead of leaking it.

## How it ties into the freeze window

On the [source side](source.md), `transfer` runs *after* `pause` + `create_snapshot`,
inside the freeze window, and its duration is recorded separately as
`SourceTiming::transfer` (distinct from `SourceTiming::snapshot`). On the
[target side](target-uffd.md), the matching `recv_snapshot` writes the files to the
work dir before the UFFD page server is stood up and the VM is resumed.
