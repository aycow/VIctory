//! CPU-only size and time scaling of live arithmetic vs live control-flow
//! joins. Used to distinguish frontend scaling from untested driver costs.

use std::time::Instant;

use crate::native::{ComputeIo, StageIo};
use crate::{Container, HwStage, NativeProgram, RootParam, RootType, io, ir, resource, structure};

fn synthetic(levels: usize, joins: bool) -> NativeProgram {
    let mut words = vec![
        0xbe81_0380u32, // s_mov_b32 s1, 0
        0xf408_0200,
        0xfa00_0000, // s_load_dwordx4 s[8:11], s[0:1], 0
        0x7e04_0300, // v_mov_b32 v2, v0
    ];
    for level in 0..levels {
        if joins {
            words.extend([
                0xbf06_8002 + ((level as u32 & 63) << 8), // s_cmp_eq_u32 s2, constant
                0xbf85_0002,                              // s_cbranch_scc1 alternate
                0x4a04_0481,                              // v_add_nc_u32 v2, 1, v2
                0xbf82_0001,                              // s_branch join
                0x4a04_0482,                              // v_add_nc_u32 v2, 2, v2
            ]);
        } else {
            words.extend([0x4a04_0481; 5]);
        }
    }
    words.extend([
        0x3406_0082, // v_lshlrev_b32 v3, 2, v0
        0xe070_1000,
        0x8002_0203, // buffer_store_dword v2, v3, s[8:11], 0 offen
        0xbf81_0000, // s_endpgm
    ]);
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 4,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: Vec::new(),
        // Two user SGPRs and group X in s2.
        io: StageIo::Cs(ComputeIo {
            rsrc2: 2 << 1 | 1 << 7,
            threads: [64, 1, 1],
        }),
        code: words.iter().flat_map(|word| word.to_le_bytes()).collect(),
    }
}

fn stages(native: &NativeProgram) -> (usize, usize, [u128; 4]) {
    let mut stamps = [Instant::now(); 5];
    let p = ir::build(&native.code, native.wave64, native.vgprs).unwrap();
    stamps[1] = Instant::now();
    let st = structure::structure(&p).unwrap();
    stamps[2] = Instant::now();
    let resources = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, native).unwrap();
    stamps[3] = Instant::now();
    let c = Container {
        digest: [0; 16],
        version: (1, 0),
        parts: Vec::new(),
    };
    let host = super::emit(&c, native, &p, &st, &io, &resources).unwrap();
    stamps[4] = Instant::now();
    (
        p.blocks.len(),
        host.dxil.len(),
        std::array::from_fn(|i| (stamps[i + 1] - stamps[i]).as_micros()),
    )
}

#[test]
#[ignore = "local, CPU-only translation probe"]
fn compare_live_cfg_join_and_arithmetic_size_scaling() {
    for levels in [64, 256, 1024, 4096] {
        for joins in [false, true] {
            let native = synthetic(levels, joins);
            let (blocks, dxil_bytes, _) = stages(&native);
            let mut values: [Vec<u128>; 4] = Default::default();
            for _ in 0..5 {
                let (count, bytes, stages) = stages(&native);
                assert_eq!((count, bytes), (blocks, dxil_bytes));
                for (n, us) in stages.into_iter().enumerate() {
                    values[n].push(us);
                }
            }
            let medians = values.map(|mut values| {
                values.sort_unstable();
                values[values.len() / 2]
            });
            eprintln!(
                "joins={joins} levels={levels} native_words={} blocks={blocks} dxil_bytes={dxil_bytes} median_us={medians:?}",
                native.code.len() / 4
            );
        }
    }
}
