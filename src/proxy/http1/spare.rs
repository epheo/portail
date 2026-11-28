//! A connection's buffers while it waits for its next request: given to
//! its worker thread, which hands them to whichever of its connections
//! next has bytes to read. An idle keep-alive connection then holds its
//! task and little else, and a busy one takes and gives back its buffers
//! at each request from the thread, not the allocator.

use std::cell::RefCell;

/// The buffers a waiting connection does not need.
pub(super) struct Idle<'a> {
    pub(super) read: &'a mut Vec<u8>,
    pub(super) head: &'a mut Vec<u8>,
    pub(super) scratch: &'a mut Vec<u8>,
}

/// Each kind's size to start with, and the most a spare may hold: one that
/// grew past it is freed, the rare head that needed it paying again.
const SIZES: [(usize, usize); 3] = [
    (super::CLIENT_BUF_INITIAL, super::CLIENT_BUF),
    (
        crate::proxy::upstream::response::RESPONSE_HEAD_INITIAL,
        crate::proxy::upstream::response::RESPONSE_HEAD_INITIAL,
    ),
    // The outgoing request head: a browser's cookies take it past its
    // first size on every request, so it is kept grown.
    (1024, 16 * 1024),
];

/// Sets of spares a thread keeps: the swing in how many of its
/// connections are between bytes at once. Past it a set is freed, and a
/// connection finding none allocates.
const KEPT: usize = 16;

/// A thread's spare sets: `sets[..held]` hold buffers, the rest are empty.
/// Buffers are swapped in and out where they lie: moving whole sets cost
/// a park and an unpark 190 instructions, most of a request's share.
struct Spares {
    held: usize,
    sets: [[Vec<u8>; 3]; KEPT],
}

thread_local! {
    static SPARE: RefCell<Spares> = const {
        RefCell::new(Spares {
            held: 0,
            sets: [const { [const { Vec::new() }; 3] }; KEPT],
        })
    };
}

impl Idle<'_> {
    fn each(&mut self) -> [&mut Vec<u8>; 3] {
        [&mut *self.read, &mut *self.head, &mut *self.scratch]
    }

    /// Give the buffers to the thread, their bytes dropped. They are
    /// empty after it, with no room: [`Idle::unpark`] knows them so.
    pub(super) fn park(&mut self) {
        let parked = SPARE.try_with(|spare| {
            let spare = &mut *spare.borrow_mut();
            let Some(set) = spare.sets.get_mut(spare.held) else {
                return false;
            };
            let mut kept = false;
            for ((buf, slot), (_, most)) in self.each().into_iter().zip(set).zip(SIZES) {
                if (1..=most).contains(&buf.capacity()) {
                    buf.clear();
                    std::mem::swap(buf, slot);
                    kept = true;
                }
            }
            spare.held += usize::from(kept);
            true
        });
        // A full thread, or one exiting: the buffers are freed.
        if parked != Ok(true) {
            for buf in self.each() {
                *buf = Vec::new();
            }
        }
        // A buffer past its kind's most was left out of the set.
        for buf in self.each() {
            if buf.capacity() > 0 {
                *buf = Vec::new();
            }
        }
    }

    /// Take buffers back where they were given, from the thread's spares
    /// or new at their first sizes. They must have none: parked, or new.
    pub(super) fn unpark(&mut self) {
        let _ = SPARE.try_with(|spare| {
            let spare = &mut *spare.borrow_mut();
            if let Some(held) = spare.held.checked_sub(1) {
                spare.held = held;
                for (buf, slot) in self.each().into_iter().zip(&mut spare.sets[held]) {
                    debug_assert_eq!(buf.capacity(), 0);
                    std::mem::swap(buf, slot);
                }
            }
        });
        for (buf, (first, _)) in self.each().into_iter().zip(SIZES) {
            if buf.capacity() == 0 {
                *buf = Vec::with_capacity(first);
            }
        }
    }
}

#[cfg(test)]
pub(super) fn spares() -> [usize; 3] {
    SPARE.with_borrow(|spare| {
        let sets = &spare.sets[..spare.held];
        let held = |kind: usize| sets.iter().filter(|set| set[kind].capacity() > 0).count();
        [held(0), held(1), held(2)]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_come_back_to_whoever_needs_them_next() {
        let (mut read, mut head, mut scratch) = (Vec::new(), Vec::new(), Vec::new());
        let mut idle = Idle {
            read: &mut read,
            head: &mut head,
            scratch: &mut scratch,
        };
        idle.unpark();
        idle.read.extend_from_slice(b"GET / HTTP/1.1\r\n\r\n");
        idle.scratch.resize(3000, b'c');
        let given = [
            idle.read.as_ptr(),
            idle.head.as_ptr(),
            idle.scratch.as_ptr(),
        ];
        let before = spares();
        idle.park();
        assert_eq!(spares(), [before[0] + 1, before[1] + 1, before[2] + 1]);
        assert_eq!(
            (read.capacity(), head.capacity(), scratch.capacity()),
            (0, 0, 0)
        );

        let (mut read2, mut head2, mut scratch2) = (Vec::new(), Vec::new(), Vec::new());
        let mut idle = Idle {
            read: &mut read2,
            head: &mut head2,
            scratch: &mut scratch2,
        };
        idle.unpark();
        let taken = [
            idle.read.as_ptr(),
            idle.head.as_ptr(),
            idle.scratch.as_ptr(),
        ];
        assert_eq!(taken, given, "the same buffers, not new ones");
        assert!(idle.read.is_empty() && idle.scratch.is_empty());
        assert!(idle.scratch.capacity() >= 3000, "kept grown");
    }

    /// A thread keeps no more than `KEPT` of a kind, and none past its
    /// kind's most.
    #[test]
    fn a_thread_keeps_few_spares_and_none_too_big() {
        std::thread::spawn(|| {
            let park = |read: usize, head: usize, scratch: usize| {
                Idle {
                    read: &mut Vec::with_capacity(read),
                    head: &mut Vec::with_capacity(head),
                    scratch: &mut Vec::with_capacity(scratch),
                }
                .park()
            };
            park(super::super::CLIENT_BUF + 1, 2 * 16384, 32 * 1024);
            assert_eq!(spares(), [0; 3], "each past its kind's most");
            for _ in 0..KEPT + 4 {
                park(4096, 16384, 1024);
            }
            assert_eq!(spares(), [KEPT; 3]);
        })
        .join()
        .unwrap();
    }
}
