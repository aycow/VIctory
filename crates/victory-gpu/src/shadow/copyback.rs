//! Byte ownership during copyback. Only bytes changed by the GPU may be
//! written back, even when title memory matched the snapshot before merging.

const OS_PAGE: usize = 4096;

/// Copy one part of a tracked page. A hole only excludes the inaccessible
/// OS pages, so valid writes on either side still reach the title.
pub(super) fn piece(
    address: usize,
    host: &mut [u8],
    before: &[u8],
    title: &mut [u8],
    runs: &mut Vec<(usize, usize)>,
) -> (usize, bool) {
    if crate::host::read_into(address as *const u8, title) {
        let written = apply(address, host, before, title, runs);
        return (
            written,
            written == runs.iter().map(|&(from, to)| to - from).sum(),
        );
    }
    let mut written = 0;
    let mut at = 0;
    if address % OS_PAGE + host.len() <= OS_PAGE {
        return (0, false);
    }
    while at < host.len() {
        let n = (OS_PAGE - (address + at) % OS_PAGE).min(host.len() - at);
        let range = at..at + n;
        let host = &mut host[range.clone()];
        let before = &before[range.clone()];
        let title = &mut title[range];
        if host != before && crate::host::read_into((address + at) as *const u8, title) {
            written += apply(address + at, host, before, title, runs);
        }
        at += n;
    }
    (written, false)
}

fn apply(
    address: usize,
    host: &mut [u8],
    before: &[u8],
    title: &[u8],
    runs: &mut Vec<(usize, usize)>,
) -> usize {
    merge(host, before, title, runs);
    let (Some(&(first, _)), Some(&(_, last))) = (runs.first(), runs.last()) else {
        return 0;
    };
    // This is all within one guarded title page. Opening it once avoids
    // reading or modifying the guard journal for every fragmented run.
    crate::watch::writing(address + first, last - first);
    runs.iter()
        .map(|&(from, to)| {
            crate::host::write_mapped(
                (address + from) as *mut u8,
                host[from..to].as_ptr(),
                to - from,
            )
        })
        .sum()
}

fn merge(host: &mut [u8], before: &[u8], title: &[u8], runs: &mut Vec<(usize, usize)>) {
    const WIDTH: usize = 16;
    runs.clear();
    let (host, host_tail) = host.as_chunks_mut::<WIDTH>();
    let (before, before_tail) = before.as_chunks::<WIDTH>();
    let (title, title_tail) = title.as_chunks::<WIDTH>();
    let tail_offset = host.len() * WIDTH;
    for (block, ((host, before), title)) in host.iter_mut().zip(before).zip(title).enumerate() {
        let mut mask = merge_block(host, before, title) as u32;
        while mask != 0 {
            let first = mask.trailing_zeros();
            let len = (mask >> first).trailing_ones();
            let from = block * WIDTH + first as usize;
            let to = from + len as usize;
            append(runs, from, to);
            mask &= !(((1 << len) - 1) << first);
        }
    }
    for (i, ((host, &before), &title)) in host_tail.iter_mut().zip(before_tail).zip(title_tail).enumerate() {
        if *host == before {
            *host = title;
        } else if *host != title {
            append(runs, tail_offset + i, tail_offset + i + 1);
        }
    }
}

fn append(runs: &mut Vec<(usize, usize)>, from: usize, to: usize) {
    if let Some((_, end)) = runs.last_mut()
        && *end == from
    {
        *end = to;
    } else {
        runs.push((from, to));
    }
}

#[cfg(target_arch = "x86_64")]
fn merge_block(host: &mut [u8; 16], before: &[u8; 16], title: &[u8; 16]) -> u16 {
    use std::arch::x86_64::*;
    // SAFETY: SSE2 is baseline on x86-64. These unaligned loads and the
    // store are wholly inside caller-owned, fixed-size buffers.
    unsafe {
        let h = _mm_loadu_si128(host.as_ptr().cast());
        let b = _mm_loadu_si128(before.as_ptr().cast());
        let t = _mm_loadu_si128(title.as_ptr().cast());
        let unchanged = _mm_cmpeq_epi8(h, b);
        let already_written = _mm_cmpeq_epi8(h, t);
        let merged = _mm_or_si128(_mm_andnot_si128(unchanged, h), _mm_and_si128(unchanged, t));
        _mm_storeu_si128(host.as_mut_ptr().cast(), merged);
        !(_mm_movemask_epi8(_mm_or_si128(unchanged, already_written)) as u16)
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn merge_block(host: &mut [u8; 16], before: &[u8; 16], title: &[u8; 16]) -> u16 {
    let mut mask = 0;
    for i in 0..host.len() {
        if host[i] == before[i] {
            host[i] = title[i];
        } else if host[i] != title[i] {
            mask |= 1 << i;
        }
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::merge;

    #[test]
    fn changes_coalesce_across_vector_boundaries_and_preserve_title_bytes() {
        let mut host = [0; 35];
        let before = host;
        let mut title = before;
        host[14..18].fill(9);
        host[19] = 12;
        host[30..].fill(5);
        title[5] = 3;
        title[18] = 7;
        title[19] = 12;
        title[28] = 2;
        let mut runs = Vec::new();
        merge(&mut host, &before, &title, &mut runs);
        assert_eq!(runs, [(14, 18), (30, 35)]);
        assert_eq!(host[5], 3);
        assert_eq!(host[18], 7);
        assert_eq!(host[19], 12);
        assert_eq!(host[28], 2);
        assert_eq!(host[15..17], [9; 2]);
    }
}
