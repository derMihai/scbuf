pub mod cbuf {
    use std::{
        alloc::{self, alloc_zeroed},
        array::IntoIter,
        marker::PhantomData,
        mem,
        num::Wrapping,
        path::Iter,
        ptr::{self, NonNull},
        rc::Rc,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    struct Cbuf<T> {
        buf: NonNull<T>,
        cap: usize,
        wi: AtomicUsize,
        ri: AtomicUsize,
        rc: AtomicUsize,
    }

    impl<T> Cbuf<T> {
        fn new(capacity: usize) -> Cbuf<T> {
            assert!(mem::size_of::<T>() > 0, "zero-sized types unsupported");
            assert!(capacity > 0, "zero capacity unsupported");
            assert!(capacity.is_power_of_two(), "capacity not a power of two");
            assert!(
                capacity <= isize::MAX.try_into().unwrap(),
                "capacity > isize::MAX"
            );

            let layout = alloc::Layout::array::<T>(capacity).unwrap();
            Cbuf {
                buf: unsafe { NonNull::new(alloc::alloc(layout) as *mut T).unwrap() },
                cap: capacity,
                wi: AtomicUsize::new(0),
                ri: AtomicUsize::new(0),
                rc: AtomicUsize::new(2),
            }
        }

        fn fill(&self) -> usize {
            let ri = Wrapping(self.ri.load(Ordering::Acquire));
            let wi = Wrapping(self.wi.load(Ordering::Acquire));

            (wi - ri).0
        }

        fn space(&self) -> usize {
            self.cap - self.fill()
        }
    }

    impl<T> Drop for Cbuf<T> {
        fn drop(&mut self) {
            let mut ri = Wrapping(self.ri.load(Ordering::Relaxed));
            let wi = Wrapping(self.wi.load(Ordering::Relaxed));
            let msk = self.cap - 1;

            while ri != wi {
                unsafe {
                    let _ = self.buf.as_ptr().add(ri.0 & msk).read();
                }
                ri += 1;
            }

            unsafe {
                alloc::dealloc(
                    self.buf.as_ptr() as *mut u8,
                    alloc::Layout::array::<T>(self.cap).unwrap(),
                )
            }
        }
    }

    pub struct CbufWriter<T> {
        cbuf: NonNull<Cbuf<T>>,
    }

    pub struct CbufReader<T> {
        cbuf: NonNull<Cbuf<T>>,
    }

    pub fn new_pair<T>(capacity: usize) -> (CbufWriter<T>, CbufReader<T>) {
        let cbuf = Cbuf::<T>::new(capacity);
        let layout = alloc::Layout::for_value(&cbuf);
        let ptr = unsafe {
            let ptr = NonNull::new(alloc::alloc(layout) as *mut Cbuf<T>).unwrap();
            ptr.as_ptr().write(cbuf);
            ptr
        };

        (CbufWriter { cbuf: ptr }, CbufReader { cbuf: ptr })
    }

    impl<T> CbufWriter<T> {
        pub fn write(&mut self, item: T) -> Option<T> {
            let cbuf: &Cbuf<T> = unsafe { self.cbuf.as_ref() };

            let space = cbuf.space();

            if space == 0 {
                return Some(item);
            }

            let wi = cbuf.wi.load(Ordering::Relaxed);
            let msk = cbuf.cap - 1;
            unsafe {
                cbuf.buf.as_ptr().add(wi & msk).write(item);
            }

            cbuf.wi.store(Wrapping(wi + 1).0, Ordering::Release);

            None
        }
    }

    impl<T> CbufReader<T> {
        pub fn read(&mut self) -> Option<T> {
            let cbuf: &Cbuf<T> = unsafe { self.cbuf.as_ref() };

            let fill = cbuf.fill();
            if fill == 0 {
                return None;
            }

            let ri = cbuf.ri.load(Ordering::Relaxed);
            let msk = cbuf.cap - 1;

            let item;
            unsafe {
                item = cbuf.buf.as_ptr().add(ri & msk).read();
            }

            cbuf.ri.store(Wrapping(ri + 1).0, Ordering::Release);

            Some(item)
        }
    }

    impl<T> Drop for CbufWriter<T> {
        fn drop(&mut self) {
            unsafe {
                let rc = (*self.cbuf.as_ptr()).rc.fetch_sub(1, Ordering::Acquire) - 1;
                if rc == 0 {
                    ptr::drop_in_place(self.cbuf.as_ptr());
                    let layout = alloc::Layout::for_value(self.cbuf.as_ref());
                    alloc::dealloc(self.cbuf.as_ptr() as *mut u8, layout);
                }
            }
        }
    }

    impl<T> Drop for CbufReader<T> {
        fn drop(&mut self) {
            unsafe {
                let rc = (*self.cbuf.as_ptr()).rc.fetch_sub(1, Ordering::Acquire) - 1;
                if rc == 0 {
                    ptr::drop_in_place(self.cbuf.as_ptr());
                    let layout = alloc::Layout::for_value(self.cbuf.as_ref());
                    alloc::dealloc(self.cbuf.as_ptr() as *mut u8, layout);
                }
            }
        }
    }
}
