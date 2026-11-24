//! Allocation for runtime-owned, shader-writable indirect budget words.
//! Each slot is a separate resource, so buffer state and alias
//! activation are scoped to one list's allocation.

use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

use super::Host;

const BUDGET_BYTES: u64 = 256;
const SLOT_BYTES: u64 = D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT as u64;
const SLOT_COUNT: u64 = u64::BITS as u64;

pub(super) struct Arena {
    heap: ID3D12Heap,
    desc: D3D12_RESOURCE_DESC,
    free: Mutex<u64>,
}

pub(super) struct BudgetBuffer {
    pub(super) resource: ID3D12Resource,
    pub(super) data: usize,
    // Fields are dropped in order. Release the placed resource before
    // allowing another resource to occupy this slot.
    slot: Option<Slot>,
}

struct Slot {
    arena: Arc<Arena>,
    index: u32,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut free = self.arena.free.lock().unwrap_or_else(|e| e.into_inner());
        let bit = 1u64 << self.index;
        debug_assert_eq!(*free & bit, 0);
        *free |= bit;
    }
}

impl Arena {
    fn new(device: &ID3D12Device) -> windows::core::Result<Arc<Self>> {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: BUDGET_BYTES,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        // SAFETY: a complete, valid buffer description.
        let required = unsafe { device.GetResourceAllocationInfo(0, &[desc]) };
        if required.SizeInBytes > SLOT_BYTES
            || required.Alignment == 0
            || !SLOT_BYTES.is_multiple_of(required.Alignment)
        {
            return Err(windows::core::Error::empty());
        }
        let heap_desc = D3D12_HEAP_DESC {
            SizeInBytes: SLOT_BYTES * SLOT_COUNT,
            Properties: D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_CUSTOM,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
                MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                CreationNodeMask: 1,
                VisibleNodeMask: 1,
            },
            Alignment: SLOT_BYTES,
            Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
        };
        let mut heap = None;
        // SAFETY: a buffer-only heap description with the validated alignment.
        unsafe { device.CreateHeap(&heap_desc, &mut heap)? };
        Ok(Arc::new(Self {
            heap: heap.ok_or_else(windows::core::Error::empty)?,
            desc,
            free: Mutex::new(u64::MAX),
        }))
    }

    fn allocate(self: &Arc<Self>, device: &ID3D12Device) -> Option<BudgetBuffer> {
        let index = {
            let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());
            if *free == 0 {
                return None;
            }
            let index = free.trailing_zeros();
            *free &= !(1u64 << index);
            index
        };
        let slot = Slot {
            arena: self.clone(),
            index,
        };
        let offset = u64::from(index) * SLOT_BYTES;
        let mut resource = None;
        // SAFETY: the slot is held exclusively and the resource fits it.
        unsafe {
            device
                .CreatePlacedResource(
                    &self.heap,
                    offset,
                    &self.desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut resource,
                )
                .ok()?;
        }
        let resource: ID3D12Resource = resource?;
        let mut data = std::ptr::null_mut();
        // SAFETY: the live resource was allocated in CPU-visible memory.
        unsafe { resource.Map(0, None, Some(&mut data)).ok()? };
        if data.is_null() {
            return None;
        }
        Some(BudgetBuffer {
            resource,
            data: data as usize,
            slot: Some(slot),
        })
    }
}

impl BudgetBuffer {
    pub(super) fn is_placed(&self) -> bool {
        self.slot.is_some()
    }

    #[cfg(all(test, feature = "diagnostics"))]
    pub(super) fn slot_index(&self) -> Option<u32> {
        self.slot.as_ref().map(|slot| slot.index)
    }
}

impl Host {
    pub(super) fn budget_buffer(&self) -> Option<BudgetBuffer> {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        let enabled = *ENABLED.get_or_init(|| {
            cfg!(feature = "diagnostics")
                && std::env::var("VICTORY_SHARED_OWNED_UAV").as_deref() == Ok("1")
        });
        if enabled
            && let Some(arena) = self.budget_arena.get_or_init(|| {
                Arena::new(&self.device)
                    .inspect_err(|error| tracing::info!(%error, "shared UAV arena unavailable"))
                    .ok()
            })
            && let Some(buffer) = arena.allocate(&self.device)
        {
            return Some(buffer);
        }
        let (resource, data) = self.shared_buffer(BUDGET_BYTES)?;
        Some(BudgetBuffer {
            resource,
            data: data as usize,
            slot: None,
        })
    }
}
