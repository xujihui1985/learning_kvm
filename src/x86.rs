use std::fs::File;
use std::io;
use std::mem::size_of;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use kvm_bindings::{
    kvm_regs, kvm_run, kvm_sregs, KVM_EXIT_FAIL_ENTRY, KVM_EXIT_HLT, KVM_EXIT_INTERNAL_ERROR,
    KVM_EXIT_INTR,
};

use crate::memory_mapping::MemoryMapping;

const KVM_GET_VCPU_MMAP_SIZE: u32 = 0xae04;
const KVM_CREATE_VCPU: u32 = 0xae41;
const KVM_SET_TSS_ADDR: u32 = 0xae47;
const KVM_RUN: u32 = 0xae80;
const KVM_GET_REGS: u32 = libc::_IOR::<kvm_regs>(0xae, 0x81) as u32;
const KVM_SET_REGS: u32 = libc::_IOW::<kvm_regs>(0xae, 0x82) as u32;
const KVM_GET_SREGS: u32 = libc::_IOR::<kvm_sregs>(0xae, 0x83) as u32;
const KVM_SET_SREGS: u32 = libc::_IOW::<kvm_sregs>(0xae, 0x84) as u32;

// Reserve three pages below 4 GiB, outside our 2 MiB RAM slot and the default
// identity-map page at 0xfffbc000. KVM uses this region for Intel compatibility.
const TSS_ADDRESS: libc::c_ulong = 0xfffb_d000;

pub(crate) fn run(kvm: &File, vm: &OwnedFd, entry: u64) -> io::Result<kvm_regs> {
    // SAFETY: The VM fd is live. This ioctl takes a scalar GPA, not a pointer.
    // The reserved range does not overlap our RAM or any device region.
    let result = unsafe { libc::ioctl(vm.as_raw_fd(), KVM_SET_TSS_ADDR as _, TSS_ADDRESS) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: This system ioctl takes no pointer; its unused argument is zero.
    let run_size = unsafe {
        libc::ioctl(
            kvm.as_raw_fd(),
            KVM_GET_VCPU_MMAP_SIZE as _,
            0 as libc::c_ulong,
        )
    };
    if run_size == -1 {
        return Err(io::Error::last_os_error());
    }
    if run_size < size_of::<kvm_run>() as i32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("KVM returned an insufficient vCPU mapping size: {run_size}"),
        ));
    }

    // SAFETY: The VM is live. The scalar argument is guest vCPU ID 0, not a
    // host CPU number or a pointer. This VM has no vCPU with that ID yet.
    let vcpu_fd = unsafe { libc::ioctl(vm.as_raw_fd(), KVM_CREATE_VCPU as _, 0 as libc::c_ulong) };
    if vcpu_fd == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: The ioctl returned a fresh fd, which is adopted exactly once.
    let vcpu = unsafe { OwnedFd::from_raw_fd(vcpu_fd) };
    println!("Created vCPU 0: fd {}", vcpu.as_raw_fd());

    // Declared after the vCPU owner so the mapping is unmapped before fd close.
    let run_mapping = MemoryMapping::shared(vcpu.as_fd(), run_size as usize)?;
    let run_state = run_mapping.as_ptr().cast::<kvm_run>();
    println!("Mapped kvm_run: {} bytes", run_mapping.len());

    let mut sregs = kvm_sregs::default();
    // SAFETY: `sregs` is a writable buffer with the required C ABI layout.
    let result = unsafe {
        libc::ioctl(
            vcpu.as_raw_fd(),
            KVM_GET_SREGS as _,
            &mut sregs as *mut kvm_sregs,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    // Keep the reset real-mode state, but move instruction fetches out of the
    // reset-vector segment and into the RAM we registered at GPA zero.
    sregs.cs.base = 0;
    sregs.cs.selector = 0;
    // SAFETY: `sregs` is fully initialized and readable for the entire call.
    let result = unsafe {
        libc::ioctl(
            vcpu.as_raw_fd(),
            KVM_SET_SREGS as _,
            &sregs as *const kvm_sregs,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }

    let mut regs = kvm_regs {
        rip: entry,
        rflags: 2, // Architectural reserved bit 1 must be set; interrupts stay off.
        ..Default::default()
    };
    // SAFETY: `regs` is a fully initialized input buffer with the KVM ABI layout.
    let result = unsafe {
        libc::ioctl(
            vcpu.as_raw_fd(),
            KVM_SET_REGS as _,
            &regs as *const kvm_regs,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }

    loop {
        // SAFETY: This thread owns the vCPU and its live control mapping. Guest
        // RAM remains mapped in the caller. No Rust references alias either
        // mapping, and this request's scalar argument is zero.
        let result = unsafe { libc::ioctl(vcpu.as_raw_fd(), KVM_RUN as _, 0 as libc::c_ulong) };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }

        // SAFETY: The mapping is aligned and large enough for kvm_run. KVM_RUN
        // has returned successfully, and no other thread can run this vCPU.
        // Read the shared field without holding a Rust reference across runs.
        let exit_reason = unsafe { std::ptr::addr_of!((*run_state).exit_reason).read_volatile() };
        match exit_reason {
            KVM_EXIT_HLT => break,
            KVM_EXIT_INTR => continue,
            KVM_EXIT_FAIL_ENTRY => {
                // SAFETY: This exit reason selects the fail_entry union member.
                let detail = unsafe { (*run_state).__bindgen_anon_1.fail_entry };
                return Err(io::Error::other(format!(
                    "KVM_EXIT_FAIL_ENTRY: hardware reason {:#x}",
                    detail.hardware_entry_failure_reason,
                )));
            }
            KVM_EXIT_INTERNAL_ERROR => {
                // SAFETY: This exit reason selects the internal union member.
                let detail = unsafe { (*run_state).__bindgen_anon_1.internal };
                return Err(io::Error::other(format!(
                    "KVM_EXIT_INTERNAL_ERROR: suberror {}",
                    detail.suberror,
                )));
            }
            reason => {
                return Err(io::Error::other(format!(
                    "unexpected KVM exit {reason}; expected KVM_EXIT_HLT",
                )));
            }
        }
    }

    // SAFETY: The vCPU is stopped and `regs` is a writable KVM ABI buffer.
    let result = unsafe {
        libc::ioctl(
            vcpu.as_raw_fd(),
            KVM_GET_REGS as _,
            &mut regs as *mut kvm_regs,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(regs)
}
