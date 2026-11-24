use super::*;
use victory_formats::texture::{Desc, Dimension, Layout, Swizzle};

struct Input(usize);

impl Drop for Input {
    fn drop(&mut self) {
        crate::watch::tests::free(self.0);
    }
}

fn tracked_staging(host: &crate::host::Host, width: u32) -> (Arc<Staging>, Input) {
    let layout = Layout::new(&Desc {
        dimension: Dimension::Tex2d,
        width,
        height: width,
        depth: 1,
        mips: 1,
        element_bytes: 4,
        block: 1,
        samples: 1,
        swizzle: Swizzle::SW_64KB_D,
    })
    .unwrap();
    let size = layout.size as usize;
    let input = Input(crate::watch::tests::pool(size.div_ceil(crate::watch::PAGE)));
    let size_upload = u64::from(width) * u64::from(width) * 4;
    let (buffer, data) = host.upload_buffer(size_upload).unwrap();
    let staging = Arc::new(Staging {
        buffer,
        #[cfg(feature = "diagnostics")]
        size: size_upload,
        #[cfg(feature = "diagnostics")]
        _capacity: crate::memstat::Charge::new(&crate::memstat::TEXTURE_UPLOAD, size_upload),
        data,
        footprints: vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: 0,
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
                Width: width,
                Height: width,
                Depth: 1,
                RowPitch: width * 4,
            },
        }],
        rows: vec![width],
        layout,
        mips: 1,
        memory: input.0 as *const u8,
        tracked: Some(crate::watch::track(input.0, size, true).unwrap()),
        last_hash: Mutex::default(),
    });
    #[cfg(feature = "diagnostics")]
    track_staging(&staging);
    (staging, input)
}

#[test]
#[ignore = "requires a native D3D12 upload buffer"]
fn tracked_textures_share_the_cpu_input_but_keep_their_own_uploads() {
    let host = host().expect("a native D3D12 test device");
    let textures: Vec<_> = [512, 512, 512, 512, 512, 512, 1024, 1024]
        .into_iter()
        .map(|width| tracked_staging(host, width))
        .collect();
    for (i, (staging, input)) in textures.iter().enumerate() {
        // SAFETY: the owned title allocation spans the layout.
        unsafe { (input.0 as *mut u8).write_bytes((i + 7) as u8, staging.layout.size as usize) };
        assert!(staging.refresh());
    }
    let baseline_capacity: usize = textures.iter().map(|(s, _)| s.layout.size as usize).sum();
    let capacity = READ_SCRATCH.lock().unwrap().capacity();
    assert_eq!((baseline_capacity, capacity), (14 << 20, 4 << 20));
    for (i, (staging, _)) in textures.iter().enumerate() {
        let upload_len = staging.footprints[0].Footprint.RowPitch * staging.rows[0];
        // SAFETY: this upload is initialized and no GPU or CPU work is writing it.
        let upload = unsafe { std::slice::from_raw_parts(staging.data, upload_len as usize) };
        assert!(upload.iter().all(|&byte| byte == (i + 7) as u8));
        assert!(!staging.refresh(), "unchanged texture {i}");
    }
    let (first, input) = &textures[0];
    let last_texel = first.layout.offset(0, 511, 511, 0).unwrap() as usize;
    // SAFETY: this is a byte of the owned title allocation. Its tracker
    // observes the CPU write and allows it through.
    unsafe { ((input.0 + last_texel) as *mut u8).write_volatile(0x7c) };
    assert!(first.refresh());
    // SAFETY: the upload is initialized and this is the last texel's first byte.
    assert_eq!(unsafe { *first.data.add((512 * 512 - 1) * 4) }, 0x7c);
    // SAFETY: a distinct texture's initialized upload was not refreshed.
    assert_eq!(unsafe { *textures[7].0.data }, 14);

    let (missing, input) = &textures[7];
    let tile = missing.layout.offset(0, 1023, 1023, 0).unwrap() as usize;
    let page = (input.0 + tile) & !4095;
    use windows_sys::Win32::System::Memory::{MEM_DECOMMIT, VirtualFree};
    // SAFETY: decommit one Windows page of our title allocation.
    assert_ne!(unsafe { VirtualFree(page as *mut _, 4096, MEM_DECOMMIT) }, 0);
    crate::watch::title_memory_remapped(page, 4096);
    assert!(missing.refresh());
    // SAFETY: both uploads are initialized and no GPU work is using them.
    unsafe {
        assert_eq!(*missing.data.add((1024 * 1024 - 1) * 4), 0);
        assert_eq!(*missing.data, 14);
        assert_eq!(*first.data.add((512 * 512 - 1) * 4), 0x7c);
    }
    eprintln!("tracked texture CPU inputs: {baseline_capacity} bytes per texture total, {capacity} bytes shared");

    #[cfg(feature = "diagnostics")]
    {
        assert_eq!(cached_staging_bytes(), Some((baseline_capacity as u64, capacity)));
        if crate::memstat::enabled() {
            let read_capacity = (capacity as u64, capacity as u64);
            assert_eq!(crate::memstat::texture_read_capacity(), read_capacity);
        }
        let busy = READ_SCRATCH.lock().unwrap();
        assert_eq!(cached_staging_bytes(), None, "a busy read buffer omits the sample");
        drop(busy);

        // Stand in for a recording that still owns one upload after its
        // texture owners release all eight allocations.
        let in_flight = textures[7].0.clone();
        drop(textures);
        assert_eq!(cached_staging_bytes(), Some((4 << 20, capacity)));
        drop(in_flight);
        assert_eq!(cached_staging_bytes(), Some((0, capacity)));
        if crate::memstat::enabled() {
            let read_capacity = (capacity as u64, capacity as u64);
            assert_eq!(crate::memstat::texture_read_capacity(), read_capacity);
        }
    }
}
