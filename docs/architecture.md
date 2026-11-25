# Architecture and invariants

The firmware starts the hypervisor directly in HS mode. The board publishes
its immutable topology and exclusive resource assignments, then calls
`pauli_core::boot(board, &BOOT_STORAGE)`. There is no native task scheduler to
start first. One dispatcher implementation serves the boot CPU and all
remote owners. Only the boot CPU runs the monitor.

## Dependencies

```text
boards
  |-- pauli-arch-riscv --> pauli-core (architecture contracts)
  |-- pauli-drivers ----> pauli-hal
  '-- pauli-core -------> pauli-config, pauli-control, pauli-devices, pauli-hal
```

Protocol crates are `no_std`, without board or ISA dependencies. The host
`xtask` may depend on protocol crates; the reverse is never allowed.
`GuestBoardLayout` is the virtual layout shared by the MMIO dispatcher and
the DTB generator. It is not a hint for a native bus scan.

## Physical versus guest memory

| Ownership | Enforced at |
| --- | --- |
| Firmware, payload, stacks and FDT reservations | Board boot resource allocator |
| Immutable Image/initrd/DTB templates | Board loader; copied, never mapped into a VM |
| One exclusive RAM range and page-table arena per VM | Board assignment and stage-2 construction |
| Host VirtIO descriptors and bounce buffers | Native drivers; no DMA to guest RAM |
| Emulated registers | Guest MMIO decoding and bounded device dispatch |

Guest physical addresses can coincide between VMs; the host backing cannot.
Ordinary 4 KiB mappings reject W+X. The current RV64 Linux boot ABI maps
explicitly owned 2 MiB RAM superpages, where Linux owns finer VS permissions.
It never maps native memory or devices. The Wave8 OoO embedded fixture uses a
private RX page. Guest I/O transfers use checked volatile access to guest
RAM, not host references aliased with a running guest.

## Lifecycle and locks

```text
VM:   Joined --publish--> Ready --stop/reset/fault--> Stopping --join--> Joined
vCPU: Stopped --start--> Runnable <--> Running --WFI--> Waiting --IRQ--> Runnable
                             ^---- control reply -- Blocked <---+
I/O:  Active --close admission--> Draining --final flush--> Flushing --> Joined
```

Fault dominates automatic reset; `vm stop` cancels automatic restart. A
joined VM is either `Stopped` or `Faulted`. Cold restart may restore image
bytes only after owner admission is closed, every visited physical CPU
has retired translations, and I/O producers have joined. An I/O join
failure is reported and does not grant permission to reuse the RAM.

Provisioning validates the kernel, optional initrd and DTB extents before
writing any guest byte. The restore plan is tied to that physical assignment.
On first load or cold restart it copies each template and zeros only the gaps
(including kernel BSS and the tail), covering RAM exactly once. Bulk volatile
access is confined to this inactive window; live guest I/O keeps its exact-width
volatile accesses. Stopping or joining a VM alone does not erase its memory.

VirtIO uses the same firmware write boundaries as stage 2. Devices may read
the complete guest assignment and write only the region between the firmware
image pages and the trailing DTB. Two fixed boot guests can exchange Ethernet
frames through bounded HS-owned queues. Their RAM remains private.
[Inter-VM transport](inter-vm-transport.md)

The manager publishes VM epochs and commits joins; vCPU owners can request
stop or fault. Global management does not lock around device service,
console writes or guest entry. Per-vCPU
metadata guards are short: take the context, unlock, run, then store it back.
Where nested, the device owner precedes its IRQ bank or shared link/driver;
never take device state from an IRQ-bank or native-driver callback. Mask
local interrupts for native context/guest-file ownership, release banks
before yielding to another owner, and never hold a business lock across
the guest world switch.

Placement is fixed for the lifetime of a VM. Each hart processes a group of
four domains at a time (one placement word on RV64). It services local mail
before guest entry, releasing the native IRQ mask between groups. Round-robin
wraps across the live groups; it does not wrap early within the first group.
A slot can be recycled after the scan: pin its current domain and recheck
the physical owner before admission. A stale or missing entry is skipped.

A stalled vCPU can service its mailbox without borrowing a hardware bank.
For guest IPI and RFENCE, the sender publishes the request (including the
fence ticket and waiter) under the target mailbox lock, then drops it before
calling the physical kick. Firmware return and receiver progress are not
ordered: the receiver must be able to drain the mailbox while the sender
is still in the kick. RFENCE success is based on completion of the actual
target-side fence, never on a successful kick alone.

For WFI, the owner samples the external line to decide whether to wake. That
sample is not an injection: at each actual entry the owner samples the line
and loads or merges the interrupt file under one VM IRQ lock, then releases
the lock before tracing or running the guest. A failed load still takes the
same bank cleanup path before retirement.

A timed WFI also publishes the vCPU's enabled timer compare as a hint to
its physical owner. The ordinary mailbox pass gathers the earliest two
future compares on that hart, so a woken vCPU can exclude its own. A running
peer's absolute slice can end at that compare. If the earliest peer compare
falls within a regular grant, nominate that waiter for the next dispatch.
After draining mail, the physical owner checks the live parked compare under
lifecycle admission. Only the exact nominated compare can take a turn ahead
of the round-robin cursor. The normal interrupt check decides whether to wake;
there is no synthetic interrupt. If the handler consumes the event and parks
early, a waiting vCPU with the **same absolute compare** may use the remainder
of this one turn. The first admission fixes its end; every guest still obeys
its own quantum and any earlier slice bound. A running handler, a still-pending
compare or an exhausted budget ends sharing. No list of vCPUs is retained:
the existing pinned-owner mail scan finds a matching sibling, which must
revalidate its live compare on its own admission. Regular rotation resumes
from the saved cursor. The last serviced vCPU waits
behind other runnable lanes, but can run again immediately if it is alone.
That record includes the VM epoch, so a cold replacement gets its own turn.

If a peer compare cuts a fresh turn, a continuously runnable vCPU protects
only the unused part of its quantum. With a 300 us budget and 100 us actually
used, another compare cannot cut the next turn before 200 us, but the guest
may still use the full 300 us if nothing else is due. A native interruption
during repayment spends only the elapsed budget; repayment does not create
new credit. Voluntary WFI, blocking and retirement discard the balance. The
charge is monotonic wall time from admission to return (including in-slice
emulation), not guest instruction time; a turn with no entry spends nothing.

Wake, retirement and cold start clear the slot's old hint. A concurrent
lifecycle change can at most shorten a grant from a scan already in progress:
the snapshot holds neither a VM reference nor authority to wake a new epoch.
Timers already due when first observed cannot nominate themselves. Other
harts keep their own slice budgets. Distinct compares do not share a turn
even if both have since expired. This is not an earliest-deadline scheduler
or a latency SLA.

Sampling a PLIC line is a yes/no query: the first enabled, pending source
above the context threshold suffices. Only a guest claim must select the
highest priority (lowest ID on a tie); the claim ignores that threshold.
Neither operation needs a cached winner. To measure their costs separately,
run `cargo test --release -p pauli-arch-riscv --lib sample_plic_cost -- --ignored --nocapture`
from this workspace. This host microbenchmark does not measure guest IRQ latency.

A device producer compares the affected vCPU lines immediately before and
after a controller mutation, under the same IRQ lock. It does not reuse a
pending snapshot from a previous poll: a hardware IMSIC owner can drain its
inbox between two MSI writes. Both rising and falling edges matter (a PLIC
claim can withdraw a line from a running sibling). Only the vCPU making a
synchronous guest access resamples its own line without needing a kick.
After releasing the lock, the host maps changed vCPUs to physical owners,
sending one IPI per affected owner; a rejected IPI remains pending for that
owner. Lifecycle operations still notify every owner.

Console input samples the VM device clock after acquiring its I/O lock, just
like timed MMIO and device service. Before a new byte restarts the FIFO
timeout, any expired level is presented to the IRQ bank; a full FIFO may also
raise a line-status interrupt even though the new byte is rejected. The
management owner delivers pending wake notifications after dropping the locks.
RX does not require a subsequent poll to publish an immediately eligible IRQ.

`BootStorage` holds boot objects and the first directory group. Further
stable pin cells and placement words grow in host-only pages; only pointer
directory storage is sized for the board's ID ceiling. The residual allocator
tracks free runs in their own first pages. It coalesces adjacent returns;
neither firmware's fixed reservation table nor a global heap is on this path.
Boot entries are permanent; managed entries have reader pins, not locks held
across guest execution.
An unpublished construction cannot run. Destroy requires a joined domain,
retired translations and no remaining pins; only after dropping the page
tables can the board's exact RAM/arena reservation return to its pool.
Every replacement receives a fresh epoch and freshly restored private RAM.
The management service removes inbound as well as outgoing grants on destroy.
Grant pages grow on the management path, before creating the child. A board
without a domain pool retains its inline grant budget and fails without effects.
A shaped child retains its generated DTB in host-only memory beside its private
page-table arena, not in shared template storage or mutable guest RAM. Reusing
the slot retires both. The ISA supplies the same guest description encoder to
the launcher and the runtime; no CPU/IRQ discovery logic is in the dispatcher.
A default dispatcher uses
`NoopTracer`: no callback state, dynamic dispatch, or additional clock reads.
An integration can supply an owner-local tracer; it must not allocate,
block or print in its callbacks.

### Reading per-owner HS counters

Valence collects `PAULI_HS_STATS` at boot. On QEMU virt, build with
`--features selftest` to collect the same counters. Halt all harts and use
the matching ELF to dump 8,256 bytes starting at `PAULI_HS_STATS`. Run
`python tools/hs-stats.py dump.bin` to show each registered owner and the
aggregate counts. The decoder also accepts a 128-byte v1 capture with its
original tick counters. [Capture commands](../tools/hs-stats.py)

The v2 header is 64 bytes. Its masks use logical owner indices. Each
128-byte record identifies the physical hart, which may differ from the
owner index. A hart ID of `u64::MAX` marks an unregistered slot. A trimmed
capture ends at byte `64 + 128 * host_cpus`. Individual words are atomic.
For consistent totals, stop all harts between callbacks.

| Counter | What increments it |
| --- | --- |
| `vm_exits` | One completed guest exit. Its classification contributes to one of the six exit buckets below. |
| `wfi_exits`, `mmio_exits` | A decoded guest WFI or emulated MMIO access. |
| `sbi_timer_exits` | A guest SBI set-timer call accepted by the hypervisor. |
| `sstc_timer_exits` | An HS supervisor timer interrupt during guest execution on an owner using native `stimecmp`. |
| `ext_irqs`, `other_exits` | A native external interrupt during guest execution, or any remaining guest exit, respectively. |
| `vtimer_irqs` | A software-injected virtual timer level changes from clear to pending. Direct timer deliveries inside VS are invisible to HS. |
| `idle_wfi` | A native owner returns from idle WFI. |
| `mail_sends`, `mail_recvs` | The issuing vCPU collects a successful send reply or a receive reply containing a message. A zero-byte message counts. |

`last_scause`, `last_sepc` and `last_stval` belong to the last exit on
each owner. The decoder prints them per hart and sums only event counts.

### Online statistics

The guest-control SBI extension uses FID 2 with six zero arguments to
discover protocol features. Bit 0 identifies `STATS_READ` and bit 1
identifies the event interface. Discovery is independent of collection,
per-VM device configuration and guest authority. A nonzero probe argument
returns `INVALID_PARAM`.

On FID 1, operation 11 takes the six scalars
`[1, 11, 0, section | (owner << 8) | (word << 16), 0, 0]`.
Section 0 reads header words 0–7 and requires owner 0. Section 1 reads
words 0–15 of logical owners 0–63. Higher selector bits are reserved.
HS first checks the encoding, then authenticates the active caller epoch
and its host-scoped telemetry grant. Only then does it check that the
board collects statistics and that the owner exists in the host topology.
The online view returns zero for owner words 9–11 and for reserved words.
The JTAG table retains the original trap registers.

The Linux header and owner ioctls each assemble one bounded record. Every
word is an independent atomic read. Even two equal reads of `vm_exits`
cannot establish that mail and idle counters came from the same instant.
Use halted captures when validating cross-field relationships.

### Control interrupts and guest WFI

An opted-in guest has a level-high control interrupt on virtual source 12.
The guest configures its PLIC context or chooses an APLIC target and IMSIC
interrupt identity. On both topologies the device tree describes the wired
source. Its node has no MMIO region.

Under the management SBI EID, FID 3 operates synchronously on the active
caller's own event state. Arguments and results are XLEN-wide scalars.

| Call | Arguments a0 through a5 | Success value in a1 |
| --- | --- | --- |
| TAKE | `[0, 0, 0, 0, 0, 0]` | All pending reasons, cleared in the same operation |
| MASK | `[1, mask, 0, 0, 0, 0]` | Previous mask |

The initial mask is zero. Bit 0 subscribes to MAIL, bit 1 to LOG and
bit 2 to STATE. Bit 3 (RESCAN) is a return-only reason. On RESCAN, recheck
the predicates covered by the subscription. The mask controls which hints
can raise the wired interrupt. Invalid commands, mask bits or reserved
arguments return `INVALID_PARAM`. A guest without the device receives
`NOT_SUPPORTED` for a well-formed event call. If a host stop closes an
opted-in VM before the call takes effect, the call returns `INVALID_STATE`.

TAKE works with interrupts masked. Multiple arrivals can produce one MAIL
bit. A guest should drain the relevant peer inboxes after a wake. An empty
TAKE succeeds, including after a forwarded MSI or a claim has outlived
the hint. Taking a hint leaves the payload available to MAIL_RECV. If a
guest polls and receives the message first, that message's hint is removed.

The management owner commits the copied message before publishing its
hint. A send from A to B can notify B only if B also holds MAIL on A's
current epoch. A later grant does not notify B about an earlier send.
Retiring an epoch or revoking a receive grant retracts its interest.
Publication and TAKE serialize the event state and interrupt level under
the recipient device lock followed by the interrupt-controller lock.
Physical owner kicks happen after both locks are released.

LOG requires a grant for the exact target epoch. An append marks console
history dirty even when the host terminal FIFO is full. The monitor takes
that flag once per harvest. Each producer tracks interested recipients by
boot slot, so sending notifications to every eligible recipient takes at
most four deliveries. STATE requires INSPECT and follows
observable lifecycle and retirement changes. Neither reason identifies the
target. A recipient with several peers rechecks its authorized predicates.
Revocation retracts the affected hint and wakes a subscribed recipient with
RESCAN. If several peers contributed to a coalesced class, RESCAN preserves
the wake for other outstanding work. Cold activation clears the recipient's
mask and pending hints.

## Bootstrap without a management OS

The firmware/launcher supplies the first manifest, Image, initrd and DTB
before any guest runs. HS binds assigned block devices but does not mount
guest filesystems. A management VM can provision children and assign explicitly
delegated volumes through the optional epoch-scoped guest-control interface. Boot
disks are not candidates; a separate grant covers each (template, disk) pair.
The published child owns the volume until joined destruction. Its DMA buffers
are always driver-owned; persistent notification cells outlive guest RAM.
A failed write/flush retirement never authorizes handoff. The manager is not
required to load itself, retire a peer, or operate the `hv>` terminal.

Boot VMs retain fixed references. Runtime VMs live in their own physical
extents; a small published slot holds only a reader pin and pointer. Dispatch
walks assigned vCPU bits, not every possible VM ID. Management streams live
records; its RPC queue limits outstanding work rather than addressable vCPUs.
Each lazily allocated directory page stores placement lanes for the discovered
native owners only. An unassigned worker rechecks that fact with interrupts
masked and parks without a periodic tick; starting a guest kicks its owners.
Worker stacks are reserved together from host-only RAM before the domain pool
is handed off. The boot hart never consumes a second stack.
A child is charged to its authenticated sponsor until destruction. Publishing
charges and releasing them use the same live set as dispatch, so a failed build,
cold restart or rejected teardown cannot lose a charge.
