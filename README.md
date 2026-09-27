# KVM from scratch in Rust

A learning project built one working milestone at a time. The current program
opens KVM, checks the required capabilities, creates a VM with 2 MiB of guest RAM,
and runs a tiny x86 guest on one vCPU. The guest executes `mov ax, 42; hlt`, and the
VMM checks that it halted with `RAX == 42`.

## Step 1: reach the KVM API

Goal: understand the first userspace/kernel interaction and print:

```text
KVM API version: 12
```

### Run on Linux

Use an x86_64 Linux machine with read/write access to `/dev/kvm`. Steps 1-3 used
architecture-independent operations; step 4 adds x86 CPU state and instructions.
If Linux runs inside a VM, the outer hypervisor must expose the virtualization
support needed by KVM. This program cannot run directly on macOS.

From this directory:

```sh
uname -srm
ls -l /dev/kvm
cargo run --locked --target-dir target
```

An absent `/dev/kvm` or insufficient permissions prevents this exercise from
reaching the ioctl. Preserve the exact error when diagnosing a failure.

### Follow the boundary

1. `OpenOptions` opens the KVM device with read/write access. The resulting
   `File` owns the descriptor and closes it on drop.
2. `as_raw_fd()` exposes the descriptor number without transferring ownership.
3. `libc::ioctl` enters the kernel with a request number and an argument.
4. `KVM_GET_API_VERSION` returns `12` directly. It does not write to a pointer.
5. On syscall failure, libc returns `-1` and sets `errno`. Read it immediately
   with `io::Error::last_os_error()`.

For the supported architectures, Linux defines this request as
`_IO(0xae, 0x00)`, which encodes to `0xae00`. The literal is intentionally visible
for this first exercise; we will use ABI bindings when structures become involved.
The `as _` conversion follows libc's request parameter type, which differs between
GNU libc and musl. The third argument is explicitly an unsigned C long with value
zero, matching the kernel handler's expectation.

The stable ABI version is not a feature level. Later we will query individual
capabilities with `KVM_CHECK_EXTENSION`. Successful output here does not establish
that a guest can execute.

### Inspect the other side

In the Linux 5.10 source tree, follow:

- `include/uapi/linux/kvm.h`: `KVMIO`, `KVM_GET_API_VERSION`, `KVM_API_VERSION`.
- `include/uapi/asm-generic/ioctl.h`: how `_IO` encodes the request.
- `virt/kvm/kvm_main.c`: the `KVM_GET_API_VERSION` case in `kvm_dev_ioctl`.

Reference: [KVM API documentation](https://docs.kernel.org/virt/kvm/api.html#kvm-get-api-version).

### Experiment before moving on

Predict the result of changing the ioctl's third argument from zero to one.
Try it on Linux, compare the error with `kvm_dev_ioctl`, then restore zero.

Explain why dropping `kvm` before using a saved raw descriptor would invalidate
our safety reasoning: the number does not keep the file open and may be reused.

## Step 2: create an empty VM

Goal: create a kernel VM object and take ownership of its descriptor in Rust.
The first two output lines look like this (the fd number may differ):

```text
KVM API version: 12
Created empty VM: fd 4
```

`KVM_CREATE_VM` is a system ioctl issued on the `/dev/kvm` descriptor. Its
request number is `_IO(0xae, 0x01)`, or `0xae01`. Its third argument is a scalar
machine type; we use zero for the default. On ARM64 this also selects the legacy
default 40-bit guest physical address limit; choosing other limits requires
capability checks, which we will cover later.

Unlike the version query, a successful return value is a new file descriptor.
The returned number belongs to the calling process's fd table. Descriptor zero
is valid, so the error check is `== -1`, not `<= 0`.

`OwnedFd::from_raw_fd` adopts the descriptor; it does not duplicate it. Its unsafe
contract requires a valid, open descriptor that we can exclusively own. Checking
the ioctl result and adopting it exactly once satisfies that contract here.
Never also call `libc::close(vm_fd)` while `vm` owns it: the duplicate close could
affect an unrelated file if the descriptor number has been reused.

The two objects have different responsibilities:

```text
File owning /dev/kvm -> subsystem operations, including creating VMs
OwnedFd owning VM    -> operations on this VM, including registering RAM
```

The `/dev/kvm` descriptor is not an owning parent of the VM descriptor. In the
kernel, the VM fd refers to an anonymous-inode file whose private data points to
the VM. Closing the last VM reference releases the VM's resources. `OwnedFd`
handles descriptor cleanup without a custom `Drop` implementation. Later, vCPU
descriptors will also keep references to the VM.

### Follow the kernel lifetime

In `virt/kvm/kvm_main.c`, read these functions in order:

1. `kvm_dev_ioctl`: dispatches `KVM_CREATE_VM`.
2. `kvm_dev_ioctl_create_vm`: creates the VM, obtains a descriptor, associates
   an anonymous-inode file with the VM, and installs it in the fd table.
3. `kvm_vm_release`: drops the VM reference when the file is released.
4. `kvm_put_kvm`: destroys the VM when its reference count reaches zero.

Reference: [KVM_CREATE_VM](https://docs.kernel.org/virt/kvm/api.html#kvm-create-vm).

### Observe the boundary

After building on Linux, use `strace` if available:

```sh
strace -e trace=ioctl,close target/debug/kvm-from-scratch
```

Find both ioctls, identify their shared input descriptor, and identify the new
descriptor returned by `KVM_CREATE_VM`. Then find the closes caused by Rust's
owners going out of scope. Other close calls may come from program startup.

Before moving on, explain why a successful `KVM_CREATE_VM` cannot execute guest
instructions yet, and why wrapping the same fd twice in `OwnedFd` would be wrong.

## Step 3: register guest physical RAM

Goal: back guest physical addresses `[0, 0x200000)` with a 2 MiB mapping in our
process. The final output line looks like this (the host address will differ):

```text
Registered slot 0: GPA 0x0..0x200000 -> HVA 0x7f1234000000 (2097152 bytes)
```

### Two address spaces

`MemoryMapping::anonymous` calls `mmap` with `PROT_READ | PROT_WRITE` and
`MAP_PRIVATE | MAP_ANONYMOUS`. The kernel chooses a page-aligned host virtual
address. Anonymous memory initially reads as zero; this does not mean every
physical page is allocated eagerly. A 2 MiB mapping also does not guarantee
huge-page backing.

The mapping length is a multiple of the supported architectures' Linux base page
sizes, including ARM64's 16 KiB and 64 KiB configurations.

The guest uses guest physical addresses (GPAs). Our process uses host virtual
addresses (HVAs). Registering slot 0 tells KVM how these ranges correspond:

```text
Guest physical range:  [0, 0x200000)
Host virtual range:   [H, H + 0x200000)

For an address inside this slot:
    HVA = H + (GPA - guest_phys_addr)

Example:
    GPA 0x1234 corresponds to HVA H + 0x1234.
```

This formula describes how KVM locates the host backing memory. The HVA is not a
host physical address and is not installed as a physical address in guest page
tables. With hardware-assisted two-stage translation, the CPU translates guest
virtual to guest physical to host physical addresses; HVA is used by the host
software to find the backing pages.

`MAP_PRIVATE` concerns the host mapping's semantics; it does not create an
independent copy for the guest. Once configured, the guest uses the backing pages
associated with this mapping.

### The registration descriptor

We first query `KVM_CAP_USER_MEMORY` with `KVM_CHECK_EXTENSION`. Unlike the basic
version and VM-creation operations, this API is described by a capability.

`kvm-bindings` supplies the C-compatible `kvm_userspace_memory_region` layout.
The program still issues the ioctl directly. `libc::_IOW` encodes the structure's
size and the request's direction, producing `0x4020ae46` on our supported targets.
The write direction means userspace supplies a descriptor to the kernel; it does
not describe guest write permissions.

| Field | Value | Meaning |
| --- | --- | --- |
| `slot` | `0` | Identifies this region within the default KVM address space |
| `flags` | `0` | Ordinary writable RAM, without dirty logging |
| `guest_phys_addr` | `0` | First guest physical byte backed by this region |
| `memory_size` | `2097152` | Region length in bytes |
| `userspace_addr` | Address returned by `mmap` | Beginning of the host backing mapping |

The ioctl receives a pointer to this descriptor. KVM copies the descriptor and
stores the region's metadata; it does not copy 2 MiB into a separate guest buffer.
The local descriptor can go away after the call. The backing mapping must remain
valid while we allow the VM to use that RAM.

### Ownership and teardown

`MemoryMapping` owns the mapping and calls `munmap` when dropped. It exposes a raw
pointer rather than constructing Rust references into memory a future guest
could access concurrently.

In `main`, `memory` is declared before `vm`. Reverse local drop order gives:

```text
close(vm fd) -> munmap(guest backing memory) -> close(/dev/kvm fd)
```

This ordering applies to both normal completion and early returns. The vCPU and
its control mapping added in step 4 are local to `x86::run` and are released
before that function returns. There are no duplicated descriptors or background
threads extending their lifetimes.

### Follow the kernel path

In `virt/kvm/kvm_main.c`, follow:

1. `kvm_vm_ioctl`: copies the descriptor from userspace.
2. `kvm_vm_ioctl_set_memory_region`: checks the user slot ID.
3. `kvm_set_memory_region`: serializes updates using `slots_lock`.
4. `__kvm_set_memory_region`: validates alignment, address ranges, and overlaps;
   records `base_gfn`, `npages`, and `userspace_addr` in a memory slot.

Then read `__gfn_to_hva_memslot` in `include/linux/kvm_host.h`. Its address
calculation is the page-based version of the formula above.

Reference: [KVM_SET_USER_MEMORY_REGION](https://www.kernel.org/doc/html/v5.10/virt/kvm/api.html#kvm-set-user-memory-region).

### Experiment

Temporarily change `guest_phys_addr` to `1`. Predict the result, run the program,
and find the matching validation in `__kvm_set_memory_region`. Restore zero.

After a successful run, inspect allocation, registration, and cleanup with:

```sh
strace -yy -e trace=mmap,munmap,ioctl,close target/debug/kvm-from-scratch
```

Identify the 2 MiB mapping, its address in the registration ioctl, and the VM close
before that mapping's `munmap`. Startup may create other unrelated mappings.

## Step 4: execute the first guest instructions

Goal: configure one x86 vCPU, execute four bytes of guest code, observe a halt
exit, and read the resulting register value.

The x86 setup lives in `src/x86.rs`. Allocation cleanup is shared by guest RAM
and the vCPU control area through `src/memory_mapping.rs`.

### Load a program into RAM

Before creating the VM, `main` copies these bytes into the host mapping at offset
`0x1000`. A compile-time check verifies that they fit in our RAM allocation.

```text
GPA       Bytes        16-bit instruction
0x1000    b8 2a 00     mov ax, 42
0x1003    f4           hlt
```

The immediate is little-endian. We initialize the general-purpose registers to
zero, so writing `AX` also gives the full `RAX` register the value 42.

This code uses no stack, interrupts, devices, or operating system. Guest execution
does not require `PROT_EXEC` on the host mapping: those bytes are fetched through
the guest's memory translation, not executed as code in the VMM's address space.

### Create a vCPU and map the control area

`KVM_CREATE_VCPU` is issued on the VM fd with guest CPU ID zero. The ID does not
pin execution to host CPU zero, and creation does not spawn a host thread that
runs the guest. Our existing thread will execute the `KVM_RUN` ioctl.

The vCPU fd is then mapped at offset zero using `MAP_SHARED`. The mapping's length
comes from `KVM_GET_VCPU_MMAP_SIZE`, issued on the subsystem fd. We check that it
is large enough for `kvm_run`; the complete mapping can include additional pages.

| Mapping | Backing | Purpose |
| --- | --- | --- |
| Guest RAM | Anonymous userspace memory | Guest instructions and data |
| vCPU control area | Kernel pages exposed through the vCPU fd | Exchange run parameters and exit information |

The `kvm_run` area is not registered as a guest RAM slot. We keep a raw pointer
to it and do not hold a Rust reference across `KVM_RUN` while the kernel can
modify the shared state.

For Intel compatibility, we check `KVM_CAP_SET_TSS_ADDR` and reserve the three-page
GPA range `[0xfffbd000, 0xfffc0000)` using `KVM_SET_TSS_ADDR`. This is outside our
guest RAM and follows the default identity-map page at `0xfffbc000`. These are
KVM compatibility regions; our guest must not access them. No IRQ chip is created.

### Set the instruction address and execution mode

The newly created x86 vCPU has reset state. We read its special registers with
`KVM_GET_SREGS`, retain the reset real-mode configuration, and set:

```text
CS.base     = 0
CS.selector = 0
RIP         = 0x1000
RFLAGS      = 0x2
```

Changing `CS` moves execution away from the reset-vector segment. In this setup,
segmentation produces linear address `CS.base + IP`, and paging is disabled, so
the first instruction is fetched from GPA `0x1000`. The retained segment state
selects 16-bit instruction decoding even though the host and Rust binary are
x86_64. `RFLAGS` bit 1 is architecturally reserved as one; interrupts remain off.

### Run, inspect the exit, and read registers

`KVM_RUN` blocks the calling thread while it runs the vCPU or waits inside KVM.
Its syscall result and the guest exit reason are separate:

- A `-1` result reports a syscall error through `errno`. We retry `EINTR` and do
  not inspect potentially stale exit data from that failed invocation.
- A successful return makes `kvm_run.exit_reason` available for dispatch.

We retry `KVM_EXIT_INTR`, accept `KVM_EXIT_HLT`, and report other exits as errors.
Entry failures include the hardware failure reason; internal errors include
their suberror code. With this VM's configuration, no in-kernel IRQ chip, and
interrupts disabled, the guest's `hlt` returns control to userspace.

After the halt, `KVM_GET_REGS` reads the vCPU register state. The program succeeds
only when `RAX == 42`, producing a final line:

```text
Guest halted: RAX = 42
```

The kernel returns this register value; the VMM has not interpreted the `mov`
instruction itself. A userspace KVM exit also does not imply that every hardware
VM exit is returned to userspace; KVM handles many exits internally.

The full cleanup order is now:

```text
unmap(kvm_run) -> close(vCPU fd) -> close(VM fd) -> unmap(RAM) -> close(KVM fd)
```

In Linux 5.10, follow `kvm_vm_ioctl_create_vcpu`, `kvm_vcpu_fault`, and the
`KVM_RUN` case in `kvm_vcpu_ioctl` in `virt/kvm/kvm_main.c`. For the halt policy,
read `kvm_vcpu_halt` in `arch/x86/kvm/x86.c`.

References: [KVM_RUN](https://www.kernel.org/doc/html/v5.10/virt/kvm/api.html#kvm-run),
[KVM_SET_TSS_ADDR](https://www.kernel.org/doc/html/v5.10/virt/kvm/api.html#kvm-set-tss-addr).

### Experiment

Change the immediate byte `0x2a` in `GUEST_CODE` to `0x2b`, leaving the expected
value unchanged. The guest should halt with 43, and the VMM should report a result
mismatch. Restore `0x2a` afterward. This distinguishes successful guest execution
from the application-level result check.

## Verification

```sh
cargo fmt --check
cargo check --locked --tests --target-dir target
cargo test --locked --target-dir target
```

For a macOS development host with the Linux target already installed, this can
type-check the Linux code without linking or executing it:

```sh
cargo check --locked --tests --target x86_64-unknown-linux-gnu --target-dir target
```

The three `MemoryMapping` tests need no `/dev/kvm`. They exercise zero
initialization and writes at both ends of an anonymous mapping, preservation of
the OS error for a zero-length allocation, and visibility of writes between a
shared mapping and its backing file. They do not verify KVM registration or
guest access.

The ignored integration test runs the complete VMM on Linux x86_64, requires
read/write access to `/dev/kvm`, and fails if the guest does not finish within
10 seconds. Run it explicitly:

```sh
cargo test --locked --target-dir target --test guest_execution -- --ignored
```

The meaningful runtime checks are the actual ioctls on Linux. No mock test
substitutes for those interactions. A successful version query followed by a
failed VM creation is possible; API availability and VM creation are separate
checks.

Next milestone: execute an `out` instruction and handle `KVM_EXIT_IO` in Rust.
