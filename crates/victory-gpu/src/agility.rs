//! App-local D3D12 Agility SDK exports for the GPU unit test executable.
#![allow(non_upper_case_globals)]

/// SDK version exported by Microsoft.Direct3D.D3D12 package 1.620.2.
#[unsafe(no_mangle)]
pub static D3D12SDKVersion: u32 = 620;

/// A C string pointer D3D12 reads once at device creation.
#[repr(transparent)]
pub struct SdkPath(*const std::ffi::c_char);

// SAFETY: points at a static, immutable C string.
unsafe impl Sync for SdkPath {}

/// Unit test executables are in <profile>/deps/, with the SDK in <profile>/D3D12/.
#[unsafe(no_mangle)]
pub static D3D12SDKPath: SdkPath = SdkPath(c"..\\D3D12\\".as_ptr());
