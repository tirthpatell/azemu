//! The decoder's memory for hostile input stays small. One test only: it
//! counts the allocations of the whole process, so no other test may run beside it in this binary.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use azemu_amqp::codec::decode;

struct Counting;
static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            let now = CURRENT.fetch_add(l.size(), Ordering::SeqCst) + l.size();
            PEAK.fetch_max(now, Ordering::SeqCst);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l);
        CURRENT.fetch_sub(l.size(), Ordering::SeqCst);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = System.realloc(p, l, new);
        if !q.is_null() {
            if new >= l.size() {
                let now = CURRENT.fetch_add(new - l.size(), Ordering::SeqCst) + (new - l.size());
                PEAK.fetch_max(now, Ordering::SeqCst);
            } else {
                CURRENT.fetch_sub(l.size() - new, Ordering::SeqCst);
            }
        }
        q
    }
}

#[global_allocator]
static A: Counting = Counting;

fn zero_width_array(count: u32) -> Vec<u8> {
    let mut v = vec![0xf0];
    v.extend_from_slice(&5u32.to_be_bytes());
    v.extend_from_slice(&count.to_be_bytes());
    v.push(0x40);
    v
}

fn list_of_arrays(n: usize, per: u32) -> Vec<u8> {
    let items: Vec<u8> = (0..n).flat_map(|_| zero_width_array(per)).collect();
    let mut v = vec![0xd0];
    v.extend_from_slice(&((items.len() + 4) as u32).to_be_bytes());
    v.extend_from_slice(&(n as u32).to_be_bytes());
    v.extend_from_slice(&items);
    v
}

fn peak_of<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let base = CURRENT.load(Ordering::SeqCst);
    PEAK.store(base, Ordering::SeqCst);
    let r = f();
    (r, PEAK.load(Ordering::SeqCst).saturating_sub(base))
}

#[test]
fn hostile_frames_cost_a_constant_times_their_size() {
    // 1.5 KB frame of the review (it cost 640 MB), and a 64 KiB one (27 GB). Refused, and the
    // peak stays below 64 values per input byte plus the allowance (a Value is 64 bytes).
    for (n, per) in [(150usize, 65_535u32), (7_000, 65_535)] {
        let frame = list_of_arrays(n, per);
        let (result, peak) = peak_of(|| decode(&frame));
        assert!(result.is_err());
        let limit = (frame.len() + 1024) * 64 * 2;
        assert!(peak <= limit, "frame of {} bytes: peak {peak} bytes, limit {limit}", frame.len());
        assert!(peak < 8 << 20);
    }
    // the largest legal frame (1 MiB) of one-byte values: at most 64 MiB, so stay well under for 64 KiB
    let mut list = vec![0xd0];
    let n = 60_000usize;
    list.extend_from_slice(&((4 + n) as u32).to_be_bytes());
    list.extend_from_slice(&(n as u32).to_be_bytes());
    list.extend(std::iter::repeat_n(0x40u8, n));
    let (result, peak) = peak_of(|| decode(&list).map(|(v, _)| v));
    assert!(result.is_ok());
    assert!(peak <= (list.len() + 1024) * 64 * 2, "peak {peak}");
}
