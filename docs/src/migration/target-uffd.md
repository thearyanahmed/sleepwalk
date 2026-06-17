# Target side & UFFD lazy restore

The target half lives in `hostd::migrate::receive_and_restore`. Its job: receive the
snapshot, stand up the [userfaultfd](https://man7.org/linux/man-pages/man2/userfaultfd.2.html)
(UFFD) page server, load the snapshot with a UFFD memory backend, and resume the VM —
returning a live `RunningVm` for the daemon to register.

## Why lazy restore at all

A naive restore reads the *entire* guest RAM back into memory before resuming. The
freeze window then scales with guest size — an 8 GB VM freezes far longer than a
512 MB one. Lazy restore breaks that link:

> Resume the VM with **empty** memory, and fault each page in on **first touch**.

The freeze window shrinks from "copy all of RAM" to "copy nothing"; pages arrive on
demand, so the freeze is **independent of guest RAM size** — the core of objective O2.

## The restore sequence

```rust
// 1. Receive the files (mem.snap + state.snap, plus optional net.json / vsock.txt)
let files = recv_snapshot(listener, &work).await?;

// 2. Re-plumb the network BEFORE loading (the snapshot names a host tap)
if net_path.exists() { create_tap(&net.tap)?; }

// 3. Stand up the UFFD page server on its own OS thread
let handler = UffdRestoreHandler::bind(&uffd_sock)?;
let serve = std::thread::spawn(move || handler.serve(&mem_thread, &stop_thread));

// 4. Spawn target Firecracker, load with a UFFD memory backend, resume immediately
fc.load_snapshot(SnapshotSource {
    state_file: work.join("state.snap"),
    backend: MemBackend::Uffd { socket: uffd_sock },  // not "read all RAM"
    resume: true,                                      // resume NOW, empty memory
}).await?;

// 5. Give the guest a beat to fault its first pages and prove it resumed
tokio::time::sleep(Duration::from_millis(300)).await;

// 6. Gratuitous ARP so the overlay relearns the VM's MAC immediately
if let Some(net) = &net { net::announce(net)?; }
```

### Re-plumb before load

A networked VM's snapshot names a host tap device (`host_dev_name`) that Firecracker
re-binds *on load*. So the tap must already exist on the target — created under the
**same name**, on the overlay bridge, so the guest keeps its MAC/IP. This happens
before `load_snapshot`, not after.

### The page server gets its own thread

Page-fault latency sits on the guest's critical path: when the guest touches a missing
page, its vCPU thread blocks until the page arrives. So the server gets a dedicated OS
thread that blocks on the uffd and serves faults as they arrive — the shape mirrors
Firecracker's reference handler. A `stop` `AtomicBool` lets the restore path shut the
thread down cleanly on teardown or on a failed load.

## Inside the page server (`hostd::uffd`)

This module owns **the only `unsafe` in `hostd`** — it touches raw file descriptors
and the userfaultfd ioctls.

- **`PageSource` trait** — anything that can supply the bytes for a faulted page.
  `fill(offset, page)` fills exactly one page, or returns `false` for a *hole* (a page
  with no backing content, which the server maps as zeros).
- **`FilePageSource`** — the production source, backed by `mem.snap`. It uses
  **positioned reads** (`pread` / `read_at`) so concurrent faults never contend on a
  shared file cursor. A short final page is zero-filled at the tail so the guest never
  sees stale buffer bytes; a read past the end of the snapshot is a hole.
- The server reads the page from the source and hands it to the kernel with
  `UFFDIO_COPY`; the kernel unblocks the faulting guest thread and it continues.

### Error handling that refuses to guess

Two faults are treated as **logic errors, not served**, because serving them would
hide a bug:

- `OutOfRange` — a fault outside the registered region.
- `NoRegion` — a fault address that matched none of the memory regions Firecracker
  declared in its handshake.

A malformed UFFD handshake from Firecracker is also surfaced (`Handshake`) rather than
guessed at.

## Failure cleanup

If `load_snapshot` fails, the restore path unwinds tidily: kill the half-started
Firecracker, set `stop` and join the page-server thread, destroy the re-plumbed tap,
and remove the work dir. No orphan threads, no orphan taps.

## The result

A live `RunningVm` owning the Firecracker process *and* the page-server thread (with
its `stop` flag), plus the carried network identity. The daemon registers it into the
fleet; the VM is now on the target, faulting its working set in as it runs.
