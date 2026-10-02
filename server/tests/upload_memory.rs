//! Processing an uploaded image takes a bounded amount of memory, however
//! the image is made. Small files can describe big images: an animated GIF
//! of a few kilobytes can have hundreds of frames on a large canvas, each
//! of which decodes to the whole canvas.
//!
//! This is its own test binary because it counts every allocation the
//! process makes.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use parlour::uploads::{self, Purpose};

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(now, Ordering::SeqCst);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::SeqCst);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            if new_size > layout.size() {
                let grown = new_size - layout.size();
                let now = CURRENT.fetch_add(grown, Ordering::SeqCst) + grown;
                PEAK.fetch_max(now, Ordering::SeqCst);
            } else {
                CURRENT.fetch_sub(layout.size() - new_size, Ordering::SeqCst);
            }
        }
        new
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The most memory, in bytes, `f` had allocated at once beyond what was
/// allocated already.
fn peak_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let before = CURRENT.load(Ordering::SeqCst);
    PEAK.store(before, Ordering::SeqCst);
    let result = f();
    (result, PEAK.load(Ordering::SeqCst) - before)
}

/// A GIF with a `width`×`height` canvas and `frames` frames, each a single
/// pixel. It is tiny, but every frame decodes to the whole canvas.
fn pixel_frames_gif(width: u16, height: u16, frames: usize) -> Vec<u8> {
    let mut gif = b"GIF89a".to_vec();
    gif.extend_from_slice(&width.to_le_bytes());
    gif.extend_from_slice(&height.to_le_bytes());
    // A global colour table of two colours: black and white.
    gif.extend_from_slice(&[0x80, 0, 0, 0, 0, 0, 0xFF, 0xFF, 0xFF]);
    // Loop for ever.
    gif.extend_from_slice(&[
        0x21, 0xFF, 0x0B, b'N', b'E', b'T', b'S', b'C', b'A', b'P', b'E', b'2', b'.', b'0', 3, 1,
        0, 0, 0,
    ]);
    for n in 0..frames {
        // A tenth of a second each.
        gif.extend_from_slice(&[0x21, 0xF9, 4, 0, 10, 0, 0, 0]);
        let x = (n % usize::from(width)) as u16;
        let y = (n / usize::from(width) % usize::from(height)) as u16;
        gif.push(0x2C);
        gif.extend_from_slice(&x.to_le_bytes());
        gif.extend_from_slice(&y.to_le_bytes());
        gif.extend_from_slice(&[1, 0, 1, 0, 0]);
        // LZW, minimum code size 2: clear, colour 1, end.
        gif.extend_from_slice(&[2, 2, 0x4C, 0x01, 0]);
    }
    gif.push(0x3B);
    gif
}

#[test]
fn small_gifs_with_many_big_frames_take_little_memory() {
    const LIMIT: usize = 16 * 1024 * 1024;
    // The reviewer's file: 300 frames on a 577×577 canvas, in about 6 KB.
    let gif = pixel_frames_gif(577, 577, 300);
    assert!(gif.len() < 10_000, "{}", gif.len());
    let (_, peak) = peak_during(|| uploads::process(&gif, Purpose::Post));
    assert!(peak < LIMIT, "peak {peak} bytes");
    // A short animation on a big canvas is fine, and still small to
    // process.
    let gif = pixel_frames_gif(1000, 1000, 3);
    let (processed, peak) = peak_during(|| uploads::process(&gif, Purpose::Post));
    let processed = processed.expect("a short animation is accepted");
    assert_eq!((processed.width, processed.height), (1000, 1000));
    assert!(peak < LIMIT, "peak {peak} bytes");
    // Many frames on the biggest canvas allowed take about one decoded
    // frame's worth (four bytes a pixel), not one for every frame.
    let gif = pixel_frames_gif(6000, 6000, 300);
    let (processed, peak) = peak_during(|| uploads::process(&gif, Purpose::Post));
    assert!(processed.is_err());
    assert!(peak < 5 * 6000 * 6000, "peak {peak} bytes");
}
