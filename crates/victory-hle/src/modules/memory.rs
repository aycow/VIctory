//! Guest Win32 allocation calls that affect title memory copied by the GPU.
#![allow(non_snake_case)]

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_DECOMMIT, MEM_FREE, MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION,
    VirtualAlloc as host_alloc, VirtualFree as host_free, VirtualQuery,
};
use windows_sys::core::BOOL;

use crate::{HleModule, hle_export};

pub(super) fn preserve_last_error<T>(operation: impl FnOnce() -> T) -> T {
    // SAFETY: the Win32 last error slot belongs to the current thread.
    let error = unsafe { GetLastError() };
    let result = operation();
    // SAFETY: restore the same thread's saved error value.
    unsafe { SetLastError(error) };
    result
}

pub(super) fn allocation_completed(
    requested: *const c_void,
    allocated: *mut c_void,
    size: usize,
    kind: u32,
) {
    if allocated.is_null() || size == 0 || kind & (MEM_RESERVE | MEM_COMMIT) == 0 {
        return;
    }
    let start = allocated as usize;
    // The host can round a requested address down. Its original end still
    // determines which pages the request touched.
    if let Some(end) = (requested as usize).max(start).checked_add(size) {
        victory_gpu::title_memory_allocation_changed(start, end - start);
    }
}

/// The whole allocation, including regions with different commit states or
/// protections. Query before freeing it, while the reservation still exists.
fn reservation_len(address: *mut c_void) -> Option<usize> {
    let base = address as usize;
    let mut end = base;
    loop {
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: the query only inspects the address and fills this structure.
        let n = unsafe { VirtualQuery(end as *const _, &mut info, size_of_val(&info)) };
        if n != size_of_val(&info) || info.State == MEM_FREE || info.AllocationBase != address {
            break;
        }
        let next = (info.BaseAddress as usize).checked_add(info.RegionSize)?;
        if next <= end {
            return None;
        }
        end = next;
    }
    (end > base).then_some(end - base)
}

pub extern "system" fn VirtualAlloc(
    address: *const c_void,
    size: usize,
    kind: u32,
    protect: u32,
) -> *mut c_void {
    // SAFETY: pass the title's own allocation request directly to the host.
    let allocated = unsafe { host_alloc(address, size, kind, protect) };
    preserve_last_error(|| allocation_completed(address, allocated, size, kind));
    allocated
}

pub extern "system" fn VirtualFree(address: *mut c_void, size: usize, kind: u32) -> BOOL {
    let len = if size == 0 && kind & (MEM_RELEASE | MEM_DECOMMIT) != 0 {
        preserve_last_error(|| reservation_len(address))
    } else {
        Some(size)
    };
    // SAFETY: pass the title's own free request directly to the host.
    let freed = unsafe { host_free(address, size, kind) };
    if freed != 0
        && let Some(len) = len
    {
        preserve_last_error(|| victory_gpu::title_memory_allocation_changed(address as usize, len));
    }
    freed
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "memory",
        dlls: &["api-ms-win-core-memory-*", "kernel32.dll", "kernelbase.dll"],
        exports: vec![hle_export!(VirtualAlloc), hle_export!(VirtualFree)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HleResolver, HostPolicy};
    use victory_loader::Provenance;
    use victory_pe::ImportSymbol;
    use windows_sys::Win32::System::Memory::{PAGE_NOACCESS, PAGE_READWRITE};

    #[test]
    fn allocation_imports_use_the_guest_hooks() {
        let resolver = HleResolver::new(HostPolicy::none());
        for dll in [
            "api-ms-win-core-memory-l1-1-0.dll",
            "kernel32.dll",
            "kernelbase.dll",
        ] {
            for name in ["VirtualAlloc", "VirtualFree"] {
                let symbol = ImportSymbol::Name {
                    hint: 0,
                    name: name.into(),
                };
                assert_eq!(resolver.classify(dll, &symbol), Some(Provenance::Hle));
            }
        }
    }

    #[test]
    fn size_zero_includes_separate_commit_regions() {
        const PAGE: usize = 4096;
        const SIZE: usize = 3 * 64 * 1024;
        let base = VirtualAlloc(std::ptr::null(), SIZE, MEM_RESERVE, PAGE_NOACCESS);
        assert!(!base.is_null());
        let last = base.cast::<u8>().wrapping_add(SIZE - PAGE).cast();
        for page in [base, last] {
            assert_eq!(VirtualAlloc(page, PAGE, MEM_COMMIT, PAGE_READWRITE), page);
            // SAFETY: the page has just been committed and is owned by this test.
            unsafe { page.cast::<u8>().write(7) };
        }
        assert_eq!(reservation_len(base), Some(SIZE));
        assert_ne!(VirtualFree(base, 0, MEM_DECOMMIT), 0);
        assert_eq!(reservation_len(base), Some(SIZE));
        for page in [base, last] {
            assert_eq!(VirtualAlloc(page, PAGE, MEM_COMMIT, PAGE_READWRITE), page);
            // SAFETY: the page has just been recommitted and starts zeroed.
            assert_eq!(unsafe { page.cast::<u8>().read() }, 0);
        }
        assert_ne!(VirtualFree(base, 0, MEM_RELEASE), 0);
        assert_eq!(reservation_len(base), None);
    }

    #[test]
    fn allocation_hooks_keep_host_failure_errors() {
        let address = std::ptr::null_mut();
        // SAFETY: this invalid request has no pointers to dereference.
        let direct = unsafe { host_alloc(address, 0, MEM_RESERVE, PAGE_READWRITE) };
        // SAFETY: read this thread's Win32 error immediately after the call.
        let error = unsafe { GetLastError() };
        assert!(direct.is_null());
        let hooked = VirtualAlloc(address, 0, MEM_RESERVE, PAGE_READWRITE);
        // SAFETY: read this thread's Win32 error immediately after the hook.
        let hooked_error = unsafe { GetLastError() };
        assert!(hooked.is_null());
        assert_eq!(hooked_error, error);

        // SAFETY: the host rejects this null release address.
        let direct = unsafe { host_free(address, 0, MEM_RELEASE) };
        // SAFETY: read this thread's Win32 error immediately after the call.
        let error = unsafe { GetLastError() };
        assert_eq!(direct, 0);
        let hooked = VirtualFree(address, 0, MEM_RELEASE);
        // SAFETY: read this thread's Win32 error immediately after the hook.
        let hooked_error = unsafe { GetLastError() };
        assert_eq!(hooked, 0);
        assert_eq!(hooked_error, error);
    }
}
