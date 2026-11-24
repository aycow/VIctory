//! Title memory at addresses a program computes ([`GLOBAL`]): pointers it
//! loads, picks by control flow or builds, and the nodes of a BVH it walks.
//! Such an address names no root parameter, so the host binds the address
//! map ([`crate::emit::address_map`]) and a read looks the address up in it
//! (a binary search over its ranges) and loads through the raw view the
//! range names. An address no range covers reads zero.

use super::*;
use crate::emit::address_map::{ENTRY, HEADER};
use crate::resource::GLOBAL;

/// A raw UAV (`RWByteAddressBuffer`): the map's views are UAVs so that the
/// same view serves the program's stores.
const RAW_UAV: (u32, u32) = (11 | 0x1000, 0);

/// Steps of the map search: up to 2^16 ranges.
const SEARCH_STEPS: u32 = 16;

/// A byte offset past every view (views hold at most 512 MiB): accesses
/// there read zero and write nothing, even a node's dwords after it.
const OUTSIDE: u32 = 0x8000_0000;

#[derive(Clone, Copy)]
struct MapCursor {
    index: V,
    count: V,
    last: V,
}

#[derive(Clone, Copy)]
struct MappedBytes {
    view: V,
    offset: V,
    available: V,
}

impl Lower<'_> {
    /// Whether the access at `site` reads title memory through the map.
    pub(super) fn is_global(&self, site: Site) -> bool {
        self.scalar.get(&site).is_some_and(|a| a.param == GLOBAL)
    }

    /// `lo | hi << 32`, an i64.
    pub(super) fn wide(&mut self, lo: V, hi: V) -> V {
        let c = self.cur;
        let t = self.s.m.int(64);
        self.s.flags |= flags::INT64;
        let lo = self.s.m.cast(c, Cast::ZExt, lo, t);
        let hi = self.s.m.cast(c, Cast::ZExt, hi, t);
        let k = self.s.m.int_const(t, 32);
        let hi = self.s.m.bin(c, Bin::Shl, hi, k);
        self.s.m.bin(c, Bin::Or, hi, lo)
    }

    /// `x` (an i32) widened to i64, sign-extended when `signed`.
    pub(super) fn widen(&mut self, x: V, signed: bool) -> V {
        let t = self.s.m.int(64);
        let op = if signed { Cast::SExt } else { Cast::ZExt };
        self.s.m.cast(self.cur, op, x, t)
    }

    /// `n` (at most 4) dwords of the address map from byte `at`.
    fn map_dwords(&mut self, at: V, n: u32) -> Vec<V> {
        let h = self.s.address_map_handle();
        self.raw_dwords(h, at, n)
    }

    /// `n` (at most 4) dwords of raw buffer `h` from byte `at`.
    pub(super) fn raw_dwords(&mut self, h: V, at: V, n: u32) -> Vec<V> {
        debug_assert!((1..=4).contains(&n));
        let c = self.cur;
        let (u, mask, al) = (
            self.s.m.undef(self.s.t.i32),
            self.s.m.i8((1 << n) - 1),
            self.s.m.i32(4),
        );
        let r = self.s.op(
            c,
            "rawBufferLoad.f32",
            Attr::ReadOnly,
            self.s.t.res_ret,
            139,
            &[h, at, u, mask, al],
        );
        (0..n)
            .map(|k| {
                let x = self.s.m.extract(self.cur, r, k);
                self.as_int(x)
            })
            .collect()
    }

    /// Looks up title address `va` (an i64) in the address map.
    ///
    /// Straight-line code: a binary search of [`SEARCH_STEPS`] steps, each
    /// probe clamped to the entries the map has. A loop per access made the
    /// ray-tracing passes' control flow too large for the host's DXIL
    /// conversion to finish.
    fn map_lookup(&mut self, va: V) -> (MapCursor, MappedBytes) {
        let zero = self.s.m.i32(0);
        let one = self.s.m.i32(1);
        let count = self.map_dwords(zero, 1)[0];
        let c = self.cur;
        // The last entry: the map holds at least one even when `count` is 0.
        let has = self.s.m.cmp(c, pred::NE, count, zero);
        let less = self.s.m.bin(c, Bin::Sub, count, one);
        let last = self.s.m.select(c, has, less, zero);
        self.map_lookup_from(
            va,
            MapCursor {
                index: zero,
                count,
                last,
            },
            SEARCH_STEPS,
        )
    }

    /// From a previous entry, find the last range starting at or below `va`.
    fn map_lookup_from(
        &mut self,
        va: V,
        mut cursor: MapCursor,
        steps: u32,
    ) -> (MapCursor, MappedBytes) {
        for k in (0..steps).rev() {
            let c = self.cur;
            let step = self.s.m.i32(1 << k);
            let probe = self.s.m.bin(c, Bin::Add, cursor.index, step);
            let within = self.s.m.cmp(c, pred::IULT, probe, cursor.count);
            let at = self.s.m.select(c, within, probe, cursor.last);
            let start = self.entry_dwords(at, 0, 2);
            let start = self.wide(start[0], start[1]);
            let c = self.cur;
            let below = self.s.m.cmp(c, pred::IULE, start, va);
            let take = self.s.m.bin(c, Bin::And, within, below);
            cursor.index = self.s.m.select(c, take, probe, cursor.index);
        }
        let e = self.entry_dwords(cursor.index, 0, 4);
        let view_offset = self.entry_dwords(cursor.index, 16, 1)[0];
        let c = self.cur;
        let start = self.wide(e[0], e[1]);
        let rel = self.s.m.bin(c, Bin::Sub, va, start);
        let size = self.widen(e[2], false);
        let after = self.s.m.cmp(c, pred::IUGE, va, start);
        let inside = self.s.m.cmp(c, pred::IULT, rel, size);
        let hit = self.s.m.bin(c, Bin::And, after, inside);
        let i32t = self.s.t.i32;
        let rel = self.s.m.cast(c, Cast::Trunc, rel, i32t);
        let offset = self.s.m.bin(c, Bin::Add, rel, view_offset);
        let remaining = self.s.m.bin(c, Bin::Sub, e[2], rel);
        let zero = self.s.m.i32(0);
        let three = self.s.m.i32(3);
        let low = self.s.m.bin(c, Bin::And, offset, three);
        let aligned = self.s.m.cmp(c, pred::EQ, low, zero);
        let hit = self.s.m.bin(c, Bin::And, hit, aligned);
        let has = self.s.m.cmp(c, pred::NE, cursor.count, zero);
        let hit = self.s.m.bin(c, Bin::And, hit, has);
        let available = self.s.m.select(c, hit, remaining, zero);
        (
            cursor,
            MappedBytes {
                view: e[3],
                offset,
                available,
            },
        )
    }

    /// Advance one dword. Starts are unique integer addresses, so at most
    /// four starts can lie between two adjacent dword addresses.
    fn next_mapped_word(&mut self, address: V, cursor: MapCursor) -> (V, MapCursor, MappedBytes) {
        let c = self.cur;
        let t = self.s.m.int(64);
        let four = self.s.m.int_const(t, 4);
        let next = self.s.m.bin(c, Bin::Add, address, four);
        let wrapped = self.s.m.cmp(c, pred::IULT, next, address);
        let end = self.s.m.int_const(t, -1);
        let next = self.s.m.select(c, wrapped, end, next);
        let (cursor, location) = self.map_lookup_from(next, cursor, 3);
        (next, cursor, location)
    }

    fn mapped_word(&mut self, location: MappedBytes) -> V {
        let bytes = self.s.m.i32(4);
        self.s
            .m
            .cmp(self.cur, pred::IUGE, location.available, bytes)
    }

    fn map_offset(&mut self, location: MappedBytes, fits: V) -> V {
        let outside = self.s.m.i32(OUTSIDE);
        self.s.m.select(self.cur, fits, location.offset, outside)
    }

    /// Dwords `first / 4..` (`n`, at most 4) of map entry `index`.
    fn entry_dwords(&mut self, index: V, first: u32, n: u32) -> Vec<V> {
        let c = self.cur;
        let size = self.s.m.i32(ENTRY);
        let at = self.s.m.bin(c, Bin::Mul, index, size);
        let base = self.s.m.i32(HEADER + first);
        let at = self.s.m.bin(c, Bin::Add, at, base);
        self.map_dwords(at, n)
    }

    /// `n` dwords of title memory from address `va` (an i64), zero where
    /// no range of the map covers `va`. `uniform`: the same address in
    /// every lane (a scalar load). A dword must fit one range, but the
    /// dwords in a vector can be supplied by different ranges.
    pub(super) fn global_read(&mut self, va: V, n: u32, uniform: bool) -> Vec<V> {
        let (mut cursor, mut location) = self.map_lookup(va);
        let mut address = va;
        let mut results = Vec::with_capacity(n as usize);
        for i in 0..n {
            if i != 0 {
                (address, cursor, location) = self.next_mapped_word(address, cursor);
            }
            let fits = self.mapped_word(location);
            let byte = self.map_offset(location, fits);
            let h = self
                .s
                .heap_handle(self.cur, location.view, false, !uniform, RAW_UAV);
            let word = self.raw_dwords(h, byte, 1)[0];
            results.push(word);
        }
        results
    }

    /// `byte`, or [`OUTSIDE`] in lanes where `guard` does not hold.
    fn unless_guarded(&mut self, guard: Option<V>, byte: V) -> V {
        match guard {
            Some(g) => {
                let away = self.s.m.i32(OUTSIDE);
                self.s.m.select(self.cur, g, byte, away)
            }
            None => byte,
        }
    }

    /// `s_load_dword*` from an address the program computes.
    pub(super) fn global_smem(&mut self, inst: &Inst) -> Result<(), Why> {
        let Fields::Smem {
            base,
            data,
            offset,
            soffset,
            ..
        } = &inst.fields
        else {
            unreachable!()
        };
        let count = match inst.name().strip_prefix("s_load_dword") {
            Some("") => 1,
            Some("x2") => 2,
            Some("x4") => 4,
            Some("x8") => 8,
            Some("x16") => 16,
            _ => return Err("a scalar access to title memory"),
        };
        let (Operand::Scalar { code: b, .. }, Some(Operand::Scalar { code: d, .. })) =
            (*base, *data)
        else {
            return Err("a scalar load from title memory");
        };
        let lo = inst.read(Reg::S(b)).ok_or("an unread address")?;
        let hi = inst.read(Reg::S(b + 1)).ok_or("an unread address")?;
        let (lo, hi) = (self.i32_of(lo)?, self.i32_of(hi)?);
        let mut va = self.wide(lo, hi);
        let c = self.cur;
        let imm = self.s.m.i32(*offset as u32);
        let imm = self.widen(imm, true);
        va = self.s.m.bin(c, Bin::Add, va, imm);
        if !matches!(soffset, Operand::Scalar { code: NULL, .. }) {
            let so = self.operand(inst, soffset)?;
            let so = self.widen(so, false);
            va = self.s.m.bin(self.cur, Bin::Add, va, so);
        }
        // Scalar loads ignore the address's two low bits.
        let t = self.s.m.int(64);
        let aligned = self.s.m.int_const(t, !3);
        va = self.s.m.bin(self.cur, Bin::And, va, aligned);
        let vals = self.global_read(va, count, true);
        for (k, x) in vals.into_iter().enumerate() {
            self.write(inst, Reg::S(d + k as u8), x, None);
        }
        Ok(())
    }

    /// The address a `global_*` instruction accesses: its SGPR base plus
    /// its VGPR offset, or its VGPR pair, plus its immediate (an i64).
    pub(super) fn global_address(&mut self, inst: &Inst) -> Result<V, Why> {
        let Fields::Flat {
            addr,
            saddr,
            offset,
            ..
        } = &inst.fields
        else {
            unreachable!()
        };
        let Some(Operand::Vector { reg, .. }) = *addr else {
            return Err("a global address not in VGPRs");
        };
        let vreg = |l: &mut Self, r: u8| -> Result<V, Why> {
            let v = inst.read(Reg::V(r)).ok_or("an unread address")?;
            l.i32_of(v)
        };
        let va = match *saddr {
            Some(Operand::Scalar { code, .. }) if code != NULL => {
                let lo = inst.read(Reg::S(code)).ok_or("an unread address")?;
                let hi = inst.read(Reg::S(code + 1)).ok_or("an unread address")?;
                let (lo, hi) = (self.i32_of(lo)?, self.i32_of(hi)?);
                let base = self.wide(lo, hi);
                let v = vreg(self, reg)?;
                let v = self.widen(v, false);
                self.s.m.bin(self.cur, Bin::Add, base, v)
            }
            _ => {
                let (lo, hi) = (vreg(self, reg)?, vreg(self, reg + 1)?);
                self.wide(lo, hi)
            }
        };
        let imm = self.s.m.i32(*offset as i32 as u32);
        let imm = self.widen(imm, true);
        Ok(self.s.m.bin(self.cur, Bin::Add, va, imm))
    }

    /// `global_load_*` from an address the program computes.
    pub(super) fn global_load(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        let Fields::Flat { dst, .. } = &inst.fields else {
            unreachable!()
        };
        let (n, sub) = match inst.name().strip_prefix("global_load_") {
            Some("dword") => (1, None),
            Some("dwordx2") => (2, None),
            Some("dwordx3") => (3, None),
            Some("dwordx4") => (4, None),
            Some("ubyte") => (1, Some((8, false))),
            Some("sbyte") => (1, Some((8, true))),
            Some("ushort") => (1, Some((16, false))),
            Some("sshort") => (1, Some((16, true))),
            _ => return Err("a global access to title memory"),
        };
        let Some(Operand::Vector { reg, .. }) = *dst else {
            return Err("a global load result");
        };
        let guard = self.guard(b, i)?;
        let va = self.global_address(inst)?;
        let vals = match sub {
            None => self.global_read(va, n, false),
            Some((width, signed)) => {
                // shortcut: the field is taken not to cross a dword.
                let t = self.s.m.int(64);
                let aligned = self.s.m.int_const(t, !3);
                let at = self.s.m.bin(self.cur, Bin::And, va, aligned);
                let w = self.global_read(at, 1, false)[0];
                let c = self.cur;
                let i32t = self.s.t.i32;
                let low = self.s.m.cast(c, Cast::Trunc, va, i32t);
                let three = self.s.m.i32(3);
                let byte = self.s.m.bin(c, Bin::And, low, three);
                let sh = self.s.m.bin(c, Bin::Shl, byte, three);
                let x = self.s.m.bin(c, Bin::LShr, w, sh);
                let left = self.s.m.i32(32 - width);
                let x = self.s.m.bin(c, Bin::Shl, x, left);
                let op = if signed { Bin::AShr } else { Bin::LShr };
                vec![self.s.m.bin(c, op, x, left)]
            }
        };
        for (k, x) in vals.into_iter().enumerate() {
            self.write(inst, Reg::V(reg + k as u8), x, guard);
        }
        Ok(())
    }

    /// The VGPR dwords `n` from `reg` that `inst` reads.
    fn data_dwords(&mut self, inst: &Inst, reg: u8, n: u32) -> Result<Vec<V>, Why> {
        (0..n)
            .map(|k| {
                let v = inst
                    .read(Reg::V(reg + k as u8))
                    .ok_or("an unread store operand")?;
                self.i32_of(v)
            })
            .collect()
    }

    /// `global_store_dword*` to an address the program computes. Lanes
    /// whose address no range covers store nothing.
    pub(super) fn global_store(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        let Fields::Flat { data, .. } = &inst.fields else {
            unreachable!()
        };
        let n = match inst.name().strip_prefix("global_store_") {
            Some("dword") => 1,
            Some("dwordx2") => 2,
            Some("dwordx3") => 3,
            Some("dwordx4") => 4,
            _ => return Err("a byte or half store to title memory"),
        };
        let Some(Operand::Vector { reg, .. }) = *data else {
            return Err("a global store's data");
        };
        let vals = self.data_dwords(inst, reg, n)?;
        let guard = self.guard(b, i)?;
        let va = self.global_address(inst)?;
        let (mut cursor, mut location) = self.map_lookup(va);
        let mut address = va;
        for (i, value) in vals.into_iter().enumerate() {
            if i != 0 {
                (address, cursor, location) = self.next_mapped_word(address, cursor);
            }
            let fits = self.mapped_word(location);
            // Lanes that miss, or do not run it, store outside the view: nothing.
            let byte = self.map_offset(location, fits);
            let byte = self.unless_guarded(guard, byte);
            self.store_mapped(location.view, byte, value);
        }
        Ok(())
    }

    fn store_mapped(&mut self, view: V, byte: V, value: V) {
        let h = self.s.heap_handle(self.cur, view, false, true, RAW_UAV);
        let iu = self.s.m.undef(self.s.t.i32);
        let args = [
            h,
            byte,
            iu,
            value,
            iu,
            iu,
            iu,
            self.s.m.i8(1),
            self.s.m.i32(4),
        ];
        let void = self.s.t.void;
        self.s.op(
            self.cur,
            "rawBufferStore.i32",
            Attr::NoUnwind,
            void,
            140,
            &args,
        );
    }

    /// `global_atomic_*` (32-bit) at an address the program computes. A
    /// lane whose address no range covers changes nothing and gets 0.
    pub(super) fn global_atomic(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        let Fields::Flat { data, dst, .. } = &inst.fields else {
            unreachable!()
        };
        // DXIL's atomic operations; cmpswap is u32::MAX.
        let op = match inst.name().strip_prefix("global_atomic_") {
            Some("add") => 0,
            Some("and") => 1,
            Some("or") => 2,
            Some("xor") => 3,
            Some("smin") => 4,
            Some("smax") => 5,
            Some("umin") => 6,
            Some("umax") => 7,
            Some("swap") => 8,
            Some("sub") => u32::MAX - 1,
            Some("cmpswap") => u32::MAX,
            _ => return Err("a global atomic of that kind"),
        };
        let Some(Operand::Vector { reg, .. }) = *data else {
            return Err("a global atomic's data");
        };
        let vals = self.data_dwords(inst, reg, if op == u32::MAX { 2 } else { 1 })?;
        let guard = self.guard(b, i)?;
        let va = self.global_address(inst)?;
        // A guest atomic may not cross a boundary or start unaligned.
        let (_, location) = self.map_lookup(va);
        let fits = self.mapped_word(location);
        let t = self.s.m.int(64);
        let mask = self.s.m.int_const(t, 3);
        let zero = self.s.m.int_const(t, 0);
        let low = self.s.m.bin(self.cur, Bin::And, va, mask);
        let aligned = self.s.m.cmp(self.cur, pred::EQ, low, zero);
        let fits = self.s.m.bin(self.cur, Bin::And, fits, aligned);
        let byte = self.map_offset(location, fits);
        let byte = self.unless_guarded(guard, byte);
        let h = self
            .s
            .heap_handle(self.cur, location.view, false, true, RAW_UAV);
        let iu = self.s.m.undef(self.s.t.i32);
        let i32t = self.s.t.i32;
        let r = if op == u32::MAX {
            // Data is (new, compare).
            let args = [h, byte, iu, iu, vals[1], vals[0]];
            self.s.op(
                self.cur,
                "atomicCompareExchange.i32",
                Attr::NoUnwind,
                i32t,
                79,
                &args,
            )
        } else {
            let (op, x) = if op == u32::MAX - 1 {
                let zero = self.s.m.i32(0);
                (0, self.s.m.bin(self.cur, Bin::Sub, zero, vals[0]))
            } else {
                (op, vals[0])
            };
            let opv = self.s.m.i32(op);
            let args = [h, opv, byte, iu, iu, x];
            self.s
                .op(self.cur, "atomicBinOp.i32", Attr::NoUnwind, i32t, 78, &args)
        };
        if let Some(Operand::Vector { reg, .. }) = *dst {
            self.write(inst, Reg::V(reg), r, guard);
        }
        Ok(())
    }

    /// The console words of the `count` dwords the descriptor load at
    /// `site` reads, from the descriptor words the host binds: a table's
    /// user data is the byte offset of its first descriptor in the view
    /// heap (32 bytes each, as the console's), the heap's own base 0.
    pub(super) fn descriptor_words(&mut self, site: Site, count: u32) -> Result<Vec<V>, Why> {
        let a = *self
            .scalar
            .get(&site)
            .ok_or("an untracked descriptor load")?;
        let rp = *self
            .x
            .native
            .layout
            .get(a.param)
            .ok_or("a descriptor read as data from no table")?;
        let samplers = self
            .x
            .native
            .used
            .iter()
            .any(|u| u.slot == rp.slot && u.kind == ResourceType::Sampler);
        let base = match rp.root {
            RootType::Table if !samplers => match self.user(a.param, 0) {
                Val::I32(x) => x,
                Val::Missing(why) => return Err(why),
                Val::Bit(_) => return Err("a table offset"),
            },
            RootType::ResourceHeap => self.s.m.i32(0),
            _ => return Err("a sampler descriptor read as data"),
        };
        let c = self.cur;
        let off = self.s.m.i32(a.offset);
        let mut byte = self.s.m.bin(c, Bin::Add, base, off);
        if let Some(d) = a.dynamic {
            let d = self.i32_of(d)?;
            byte = self.s.m.bin(self.cur, Bin::Add, byte, d);
        }
        let h = self.s.descriptor_words_handle();
        let mut out = Vec::with_capacity(count as usize);
        let mut k = 0;
        while k < count {
            let chunk = (count - k).min(4);
            let four = self.s.m.i32(4 * k);
            let at = self.s.m.bin(self.cur, Bin::Add, byte, four);
            out.extend(self.raw_dwords(h, at, chunk));
            k += chunk;
        }
        Ok(out)
    }
}
