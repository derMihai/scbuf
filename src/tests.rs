use std::{
    num::Wrapping,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
};

use crate::{new_scbuf, ConsEnd, ProdEnd};

struct ReleaseEvent {
    flag: &'static AtomicBool,
}

impl Drop for ReleaseEvent {
    fn drop(&mut self) {
        self.flag.store(true, Ordering::Relaxed);
    }
}

struct AllocTracked {
    seq: Wrapping<usize>,
    _counter: Arc<ReleaseEvent>,
}

impl AllocTracked {
    fn check_seq(&self, last: &Self) {
        if (self.seq - last.seq).0 != 1usize {
            panic!(
                "expected seq {}, got {}",
                (last.seq + Wrapping(1)).0,
                self.seq.0
            );
        }
    }
}

fn pop_reader(mut cbuf_reader: ConsEnd<AllocTracked>, data: &mut Vec<AllocTracked>) {
    'outer: while data.capacity() != data.len() {
        for item in &mut cbuf_reader {
            data.push(item);

            if data.capacity() == data.len() {
                break 'outer;
            }
        }
    }
}

fn push_writer(mut cbuf_writer: ProdEnd<AllocTracked>, data: Vec<AllocTracked>) {
    for mut item in data.into_iter() {
        while let Some(skipped) = cbuf_writer.push(item) {
            item = skipped;
        }
    }
}

fn batch_reader(mut cbuf_reader: ConsEnd<AllocTracked>, data: &mut Vec<AllocTracked>) {
    while data.capacity() != data.len() {
        cbuf_reader.read(data);
    }
}

fn batch_writer(mut cbuf_writer: ProdEnd<AllocTracked>, mut data: Vec<AllocTracked>) {
    let mut data = data.drain(..);
    while data.len() > 0 {
        cbuf_writer.write(&mut data);
    }
}

fn run_test<R, W>(items_w: usize, items_r: usize, cap: usize, readf: R, writef: W)
where
    R: Fn(ConsEnd<AllocTracked>, &mut Vec<AllocTracked>) + Send + 'static,
    W: Fn(ProdEnd<AllocTracked>, Vec<AllocTracked>) + Send + 'static,
{
    let (writer, reader) = new_scbuf::<AllocTracked>(cap);
    let rel_event = Box::leak(Box::<AtomicBool>::new(AtomicBool::new(false)));

    let reader_thread = thread::spawn(move || {
        let mut data: Vec<AllocTracked> = Vec::with_capacity(items_r);
        readf(reader, &mut data);
        // Dropping items might use atomics, so we do it at the end to limit 'beneficial'
        // side-effects that might prevent reordering issues in the buffer implementation.
        let mut data = data.into_iter();
        let mut last = match data.next() {
            Some(item) => item,
            None => return,
        };

        for item in data {
            item.check_seq(&last);
            last = item;
        }
    });

    let counter = Arc::<ReleaseEvent>::new(ReleaseEvent { flag: rel_event });
    // Allocating AllocTracked issues atomic calls, so we do it beforehand to limit
    // 'beneficial' side-effects that might prevent reordering issues in the buffer
    // implementation.
    let mut data: Vec<AllocTracked> = Vec::with_capacity(items_w);
    for i in 0..items_w {
        data.push(AllocTracked {
            seq: Wrapping(i),
            _counter: Arc::clone(&counter),
        });
    }

    writef(writer, data);

    reader_thread.join().unwrap();
    drop(counter);

    if !rel_event.load(Ordering::Relaxed) {
        panic!("Not all items released!");
    }

    println!(
        "Passed test: written={:>10}, read={:>10}, buffer capacity={:>10}",
        items_w, items_r, cap
    );
}

const TEST_SET: [(usize, usize, usize); 8] = [
    (32, 0, 32),
    (1, 1, 1),
    (0, 0, 32),
    (1, 0, 1),
    (10_000_000, 10_000_000, 1),
    (10_000_000, 10_000_000, 4096),
    (10_004_000, 10_000_000, 4096),
    (4_001, 4_000, 4096),
];
#[test]
fn push_pop() {
    for (w, r, c) in TEST_SET {
        run_test(w, r, c, pop_reader, push_writer);
    }
}

#[test]
fn read_write() {
    for (w, r, c) in TEST_SET {
        run_test(w, r, c, batch_reader, batch_writer);
    }
}

#[test]
#[should_panic]
fn cap_not_pow2() {
    let (_, _) = new_scbuf::<char>(3);
}

#[test]
#[should_panic]
fn item_zero_sized() {
    let (_, _) = new_scbuf::<()>(2);
}
