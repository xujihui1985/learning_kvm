#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("This guest execution exercise requires Linux x86_64 and access to /dev/kvm.");

mod memory_mapping;
mod x86;

use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use kvm_bindings::{kvm_userspace_memory_region, KVM_CAP_SET_TSS_ADDR, KVM_CAP_USER_MEMORY};

use memory_mapping::MemoryMapping;

// _IO(KVMIO, 0x00), where KVMIO = 0xae, on Linux x86_64 and aarch64.
const KVM_GET_API_VERSION: u32 = 0xae00;
// _IO(KVMIO, 0x01) on the same architectures.
const KVM_CREATE_VM: u32 = 0xae01;
const KVM_CHECK_EXTENSION: u32 = 0xae03;
// _IOW includes the size of the input structure in the request number.
const KVM_SET_USER_MEMORY_REGION: u32 =
    libc::_IOW::<kvm_userspace_memory_region>(0xae, 0x46) as u32;
const KVM_API_VERSION: i32 = 12;
const GUEST_RAM_SIZE: usize = 2 * 1024 * 1024;
const GUEST_ENTRY: usize = 0x1000;
// 16-bit real mode: mov ax, 42; hlt.
const GUEST_CODE: [u8; 4] = [0xb8, 0x2a, 0x00, 0xf4];
const _: () = assert!(GUEST_ENTRY + GUEST_CODE.len() <= GUEST_RAM_SIZE);

fn main() -> io::Result<()> {
    let kvm = OpenOptions::new().read(true).write(true).open("/dev/kvm")?;

    // SAFETY: `kvm` keeps the fd alive for this call. This request takes no
    // pointer argument; KVM requires its unused argument to be zero.
    let version = unsafe {
        libc::ioctl(
            kvm.as_raw_fd(),
            KVM_GET_API_VERSION as _,
            0 as libc::c_ulong,
        )
    };

    if version == -1 {
        return Err(io::Error::last_os_error());
    }
    if version != KVM_API_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("KVM API version {version}, expected {KVM_API_VERSION}"),
        ));
    }

    println!("KVM API version: {version}");

    for (capability, name) in [
        (KVM_CAP_USER_MEMORY, "KVM_CAP_USER_MEMORY"),
        (KVM_CAP_SET_TSS_ADDR, "KVM_CAP_SET_TSS_ADDR"),
    ] {
        // SAFETY: `kvm` keeps the fd alive. This request takes a scalar
        // capability identifier, not a userspace pointer.
        let supported = unsafe {
            libc::ioctl(
                kvm.as_raw_fd(),
                KVM_CHECK_EXTENSION as _,
                capability as libc::c_ulong,
            )
        };
        if supported == -1 {
            return Err(io::Error::last_os_error());
        }
        if supported == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("{name} is required"),
            ));
        }
    }

    // Locals drop in reverse declaration order: close the VM before unmapping
    // its RAM. `x86::run` drops its vCPU and run mapping before returning here.
    let memory = MemoryMapping::anonymous(GUEST_RAM_SIZE)?;
    // SAFETY: The compile-time bounds check keeps the code inside this writable
    // mapping. The source cannot overlap it, and no VM or vCPU can access it yet.
    unsafe {
        std::ptr::copy_nonoverlapping(
            GUEST_CODE.as_ptr(),
            memory.as_ptr().add(GUEST_ENTRY),
            GUEST_CODE.len(),
        );
    }

    // SAFETY: `kvm` keeps the fd alive. This request takes a scalar machine
    // type, not a pointer; zero selects the default VM type.
    let vm_fd = unsafe { libc::ioctl(kvm.as_raw_fd(), KVM_CREATE_VM as _, 0 as libc::c_ulong) };
    if vm_fd == -1 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: KVM_CREATE_VM returned a new, valid fd. Ownership is transferred
    // exactly once, and no other owner will close this descriptor.
    let vm = unsafe { OwnedFd::from_raw_fd(vm_fd) };
    println!("Created empty VM: fd {}", vm.as_raw_fd());

    let region = kvm_userspace_memory_region {
        slot: 0,
        flags: 0,
        guest_phys_addr: 0,
        memory_size: memory.len() as u64,
        userspace_addr: memory.as_ptr() as usize as u64,
    };

    // SAFETY: `region` has the KVM C ABI layout and remains readable throughout
    // the call. Its backing mapping is writable, page-aligned, and covers the
    // entire region. `memory` outlives the VM; no vCPU accesses it yet.
    let result = unsafe {
        libc::ioctl(
            vm.as_raw_fd(),
            KVM_SET_USER_MEMORY_REGION as _,
            &region as *const kvm_userspace_memory_region,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }

    println!(
        "Registered slot {}: GPA {:#x}..{:#x} -> HVA {:#x} ({} bytes)",
        region.slot,
        region.guest_phys_addr,
        region.guest_phys_addr + region.memory_size,
        region.userspace_addr,
        region.memory_size,
    );

    let regs = x86::run(&kvm, &vm, GUEST_ENTRY as u64)?;
    if regs.rax != 42 {
        return Err(io::Error::other(format!(
            "guest halted with RAX = {}, expected 42",
            regs.rax,
        )));
    }
    println!("Guest halted: RAX = {}", regs.rax);

    Ok(())
}
