use std::{
    alloc,
    cmp::min,
    mem,
    num::Wrapping,
    ptr::NonNull,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    vec::Drain,
};

struct Cbuf<T> {
    buf: NonNull<T>,
    cap: usize,
    msk: usize,
    w_idx: AtomicUsize,
    r_idx: AtomicUsize,
}

impl<T> Cbuf<T> {
    fn new(capacity: usize) -> Cbuf<T> {
        if mem::size_of::<T>() == 0 {
            panic!("zero-sized types unsupported");
        }
        if !capacity.is_power_of_two() {
            panic!("capacity not a power of two")
        }

        let layout = alloc::Layout::array::<T>(capacity).unwrap();
        Cbuf {
            buf: unsafe { NonNull::new(alloc::alloc(layout) as *mut T).unwrap() },
            cap: capacity,
            msk: capacity - 1,
            w_idx: AtomicUsize::new(0),
            r_idx: AtomicUsize::new(0),
        }
    }
}

impl<T> Drop for Cbuf<T> {
    fn drop(&mut self) {
        // Fine to use relaxed, since from now on there are no endpoints holding
        // on this buffer.
        let mut r_idx = Wrapping(self.r_idx.load(Ordering::Relaxed));
        let w_idx = Wrapping(self.w_idx.load(Ordering::Relaxed));

        while r_idx != w_idx {
            unsafe {
                // If ri != wi, there were more items moved in than moved out. Since
                // from now on no endpoints hold any reference to this buffer, we
                // may safely drop any remaining items.
                self.buf.as_ptr().add(r_idx.0 & self.msk).drop_in_place();
            }
            r_idx += 1;
        }

        unsafe {
            let layout = alloc::Layout::array::<T>(self.cap).unwrap();
            alloc::dealloc(self.buf.as_ptr() as *mut u8, layout)
        }
    }
}

/// Producer endpoint for inserting data into the circular buffer.
/// See [`ProdEnd::push()`] and [`ProdEnd::write()`].
pub struct ProdEnd<T> {
    cbuf: Arc<Cbuf<T>>,
}
/// Consumer endpoint for retrieving data from the circular buffer.
/// See [`ConsEnd::pop()`] and [`ConsEnd::read()`].
pub struct ConsEnd<T> {
    cbuf: Arc<Cbuf<T>>,
}

/// Creates a new circular buffer with the given `capacity` and returns one consumer
/// and one producer endpoint.  
///
/// The endpoints are [`Send`] and [`Sync`], so it is safe to use them in a multi-threaded
/// scenario. They rely solely on atomic operations for synchronization (lock-free).
/// An endpoint can be further shared across multiple threads (multiple producers/consumers)
/// by wrapping it into an [`Arc<Mutex<>>`]. This way, consumers will block other
/// consumers (and producers other producers), but producers are still lock-free
/// w.r. to consumers.
///
/// The `capacity` must be a power of two and leq [`isize::MAX`]` + 1`.
///
/// Dropping both endpoints will drop the underlying buffer storage and any items
/// left inside.
///
///
/// # Panics
/// Will panic if `capacity` is not a power of two (zero included), or greater
/// than [`isize::MAX`]` + 1`.
///
/// # Examples
/// ```
/// use scbuf;
/// use std::{sync::{Arc, Mutex}, thread};
///
/// let (prod_ep, mut cons_ep) = scbuf::new_scbuf(2);
///
/// let prod_ep1 = Arc::new(Mutex::new(prod_ep));
/// let prod_ep2 = Arc::clone(&prod_ep1);
/// let producer_1 = thread::spawn(move || prod_ep1.lock().unwrap().push('a'));
/// let producer_2 = thread::spawn(move || prod_ep2.lock().unwrap().push('b'));
///
/// producer_1.join().unwrap();
/// producer_2.join().unwrap();
///
/// let mut data = Vec::with_capacity(2);
/// cons_ep.read(&mut data);
///
/// assert!(data == ['a', 'b'] || data == ['b', 'a']);
/// ```
///
pub fn new_scbuf<T>(capacity: usize) -> (ProdEnd<T>, ConsEnd<T>) {
    let cbuf = Arc::new(Cbuf::<T>::new(capacity));

    (
        ProdEnd {
            cbuf: Arc::clone(&cbuf),
        },
        ConsEnd {
            cbuf: Arc::clone(&cbuf),
        },
    )
}

impl<T> ProdEnd<T> {
    fn space(&self) -> (usize, Wrapping<usize>) {
        let cbuf = self.cbuf.as_ref();
        // OK to use relaxed for w_idx, since there either is exactly one thread
        // writing, or other sync mechanism will enforce the ordering.
        let w_idx = Wrapping(cbuf.w_idx.load(Ordering::Relaxed));
        // OK to use relaxed for r_idx, since writes are not speculated -> no item
        // will be committed to the buffer before r_idx will have been evaluated.
        let r_idx = Wrapping(cbuf.r_idx.load(Ordering::Relaxed));

        (cbuf.cap - (w_idx - r_idx).0, w_idx)
    }

    /// Moves a single item of into the circular buffer. Returns [`None`] on success,
    /// [`Some<T>`] containing the item if the buffer was full.
    ///
    /// # Examples
    ///
    /// ```
    /// use scbuf;
    ///
    /// let (mut prod_ep, _) = scbuf::new_scbuf(1);
    /// assert_eq!(prod_ep.push('a'), None);
    /// assert_eq!(prod_ep.push('b'), Some('b'));
    ///
    /// ```
    // Must be mutable, otherwise:
    // - one could send multiple references to different threads -> racy.
    // - would be covariant: we could push &'a T in a ProdEnd<&'static T>.
    pub fn push(&mut self, item: T) -> Option<T> {
        let cbuf = self.cbuf.as_ref();

        let (space, w_idx) = self.space();
        if space == 0 {
            return Some(item);
        }

        unsafe {
            // Safe to write: if space > 0, then some data items were moved out
            // of the buffer. This is ensured by the control dependency on 'space',
            // paired with the release semantics of ConsEnd::pop().
            cbuf.buf.as_ptr().add(w_idx.0 & cbuf.msk).write(item);
        }

        cbuf.w_idx.store((w_idx + Wrapping(1)).0, Ordering::Release);

        None
    }
    /// Moves n elements from `data` into the buffer, where n is the minimum of
    /// `data.len()` and the space available in the buffer. [`Drain`] is used instead
    /// of [`Vec`] to avoid possibly unnecessary shift of items in the original
    /// vector, should it not be drained completely.
    ///
    /// # Examples
    ///
    /// ```
    /// use scbuf;
    ///
    /// let (mut prod_ep, _) = scbuf::new_scbuf(2);
    /// let mut data = vec!['a', 'b', 'c'];
    /// let mut drain = data.drain(..);
    ///
    /// prod_ep.write(&mut drain);
    /// // we can pop the leftover directly from `drain` without having it shifted
    /// // into the first position of `data`
    /// assert_eq!(drain.next(), Some('c'));
    /// ```
    pub fn write<'a>(&mut self, data: &mut Drain<'a, T>) {
        let cbuf = self.cbuf.as_ref();
        let (space, mut w_idx) = self.space();

        for item in data.take(min(space, data.len())) {
            unsafe {
                // see ProdEnd<T>::push()
                cbuf.buf.as_ptr().add(w_idx.0 & cbuf.msk).write(item);
            }
            w_idx += 1;
        }

        cbuf.w_idx.store(w_idx.0, Ordering::Release);
    }
}

impl<T> ConsEnd<T> {
    fn fill(&self) -> (usize, Wrapping<usize>) {
        let cbuf = self.cbuf.as_ref();
        // OK to use relaxed for r_idx, since there either is exactly one thread
        // reading, or other sync mechanism will enforce the ordering.
        let r_idx = Wrapping(cbuf.r_idx.load(Ordering::Relaxed));
        let w_idx = Wrapping(cbuf.w_idx.load(Ordering::Acquire));

        ((w_idx - r_idx).0, r_idx)
    }

    /// Pops one item from the circular buffer. Returns [`None`] if the buffer was
    /// empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use scbuf;
    /// use std::thread;
    ///
    /// let (mut prod_ep, mut cons_ep) = scbuf::new_scbuf(2);
    ///
    /// prod_ep.push("data");
    ///
    /// thread::spawn(move || {
    ///     assert_eq!(cons_ep.pop(), Some("data"));
    ///     assert_eq!(cons_ep.pop(), None);
    /// });
    ///
    /// ```
    // Must be mutable, otherwise one could send multiple references to different
    // threads -> racy.
    pub fn pop(&mut self) -> Option<T> {
        let cbuf = self.cbuf.as_ref();

        let (fill, r_idx) = self.fill();
        if fill == 0 {
            return None;
        }

        let item = unsafe {
            // Safe to read: if fill > 0, then some data items were moved to the
            // buffer. This is ensured by the acquire semantics in Self::fill()
            // paired with the release semantics of ProdEnd::push().
            cbuf.buf.as_ptr().add(r_idx.0 & cbuf.msk).read()
        };

        cbuf.r_idx.store((r_idx + Wrapping(1)).0, Ordering::Release);

        Some(item)
    }

    /// Reads n items from the buffer into `data`, where n is the minimum of the
    /// available space in `data` (`data.capacity() - data.len()`) and the number
    /// of items available in the buffer.
    ///
    /// # Examples
    ///
    /// ```
    /// use scbuf;
    /// use std::thread;
    ///
    /// let (mut prod_ep, mut cons_ep) = scbuf::new_scbuf(4);
    ///
    /// prod_ep.write(&mut vec![101, 102, 103].drain(..));
    ///
    /// let mut cons_buf = Vec::with_capacity(1);
    ///
    /// cons_ep.read(&mut cons_buf);
    /// assert_eq!(cons_buf, [101]);
    /// cons_ep.read(&mut cons_buf);
    /// assert_eq!(cons_buf, [101]);
    ///
    /// let mut cons_buf = Vec::with_capacity(2);
    ///
    /// cons_ep.read(&mut cons_buf);
    /// assert_eq!(cons_buf, [102, 103]);
    /// ```
    pub fn read(&mut self, data: &mut Vec<T>) {
        let cbuf = self.cbuf.as_ref();
        let (fill, mut r_idx) = self.fill();

        for _ in 0..min(fill, data.capacity() - data.len()) {
            data.push(unsafe {
                // see ConsEnd<T>::pop()
                cbuf.buf.as_ptr().add(r_idx.0 & cbuf.msk).read()
            });
            r_idx += 1;
        }

        cbuf.r_idx.store(r_idx.0, Ordering::Release);
    }
}

impl<T> Iterator for ConsEnd<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.pop()
    }
}

// NonNull is not Send and Sync, but we use atomics for synchronization
unsafe impl<T> Send for Cbuf<T> {}
unsafe impl<T> Sync for Cbuf<T> {}
