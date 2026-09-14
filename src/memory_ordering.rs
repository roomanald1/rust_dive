use loom::sync::Arc;
use loom::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicUsize, Ordering};
use loom::thread;
use std::marker::PhantomData;

//Relaxed would break here!!!
#[test]
fn relaxed_flag_data_can_break() {
    loom::model(|| {
        let data = Arc::new(AtomicI32::new(0));
        let flag = Arc::new(AtomicBool::new(false));

        let data_writer = data.clone();
        let flag_writer = flag.clone();

        let writer = thread::spawn(move || {
            data_writer.store(42, Ordering::Relaxed);
            flag_writer.store(true, Ordering::Release);
        });

        let data_reader = data.clone();
        let flag_reader = flag.clone();

        let reader = thread::spawn(move || {
            // Limit the spin loop so loom doesn't explode
            for _ in 0..3 {
                if flag_reader.load(Ordering::Acquire) {
                    break;
                }
                thread::yield_now();
            }

            let v = data_reader.load(Ordering::Relaxed);
            if flag_reader.load(Ordering::Relaxed) && v != 42 {
                panic!("BROKEN: {v}");
            }
        });

        writer.join().unwrap();
        reader.join().unwrap();
    });
}

//Relaxed does not break here
#[test]
fn cas_increment() {
    loom::model(|| {
        let counter = Arc::new(AtomicUsize::new(0));

        let mut threads = vec![];

        for _ in 0..2 {
            let c = counter.clone();
            threads.push(thread::spawn(move || {
                for _ in 0..2 {
                    loop {
                        let old = c.load(Ordering::Acquire);
                        let new = old + 1;

                        if c.compare_exchange(old, new, Ordering::Relaxed, Ordering::Relaxed)
                            .is_ok()
                        {
                            break;
                        }

                        thread::yield_now();
                    }
                }
            }));
        }

        for t in threads {
            t.join().unwrap();
        }

        let final_ = counter.load(Ordering::Acquire);
        assert_eq!(final_, 4);
    });
}

#[derive(Clone, Copy)]
struct Node<T> {
    value: T,
    next: *mut Node<T>,
}
#[derive(Clone, Copy)]
struct TaggedPtr<T> {
    ptr: *mut Node<T>,
    tag: usize,
}

impl<T> TaggedPtr<T> {
    fn pack(&self) -> usize {
        let ptr_bits = self.ptr as usize;
        (self.tag << 3) | ptr_bits
    }
    fn unpack(bits: usize) -> TaggedPtr<T> {
        let ptr = (bits & !0b111) as *mut Node<T>;
        let tag = bits >> 3;
        TaggedPtr { ptr, tag }
    }
}

struct Stack<T> {
    head: AtomicUsize, //(ptr, tag) packed into usize
    phantom_data: PhantomData<T>,
}

impl<T> Stack<T>
where
    T: Clone,
{
    fn push(&self, value: T) {
        let node = Box::new(Node {
            value,
            next: std::ptr::null_mut(),
        });
        let node_ptr = Box::into_raw(node) as *mut Node<T>;

        loop {
            let bits = self.head.load(Ordering::Acquire);
            let tagged = TaggedPtr::<T>::unpack(bits);
            unsafe {
                (*node_ptr).next = tagged.ptr;
            }

            let new_tagged = TaggedPtr {
                ptr: node_ptr,
                tag: tagged.tag.wrapping_add(1),
            };

            if self
                .head
                .compare_exchange(bits, new_tagged.pack(), Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
    }

    fn pop(&self) -> Option<T> {
        loop {
            let bits = self.head.load(Ordering::Acquire);
            let tagged = TaggedPtr::<T>::unpack(bits);

            if tagged.ptr.is_null() {
                return None;
            }

            let next = unsafe { (*tagged.ptr).next };

            let new_tagged = TaggedPtr {
                ptr: next,
                tag: tagged.tag.wrapping_add(1),
            };
            if self
                .head
                .compare_exchange(bits, new_tagged.pack(), Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                 let value = unsafe { (*tagged.ptr).value.clone() };
                return Some(value);
            }
        }
    }
    pub fn new() -> Self {
        Self {
            head: AtomicUsize::new(0),
            phantom_data: PhantomData,
        }
    }
}
#[test]
fn treiber_stack_basic() {
    loom::model(|| {
        let stack = Arc::new(Stack::<usize>::new());

        // Pushers
        let s1 = stack.clone();
        let t1 = thread::spawn(move || {
            s1.push(1);
            s1.push(2);
        });

        let s2 = stack.clone();
        let t2 = thread::spawn(move || {
            s2.push(3);
            s2.push(4);
        });

        // Poppers
        let s3 = stack.clone();
        let t3 = thread::spawn(move || {
            let _ = s3.pop();
            let _ = s3.pop();
        });

        let s4 = stack.clone();
        let t4 = thread::spawn(move || {
            let _ = s4.pop();
            let _ = s4.pop();
        });

        t1.join().unwrap();
        t2.join().unwrap();
        t3.join().unwrap();
        t4.join().unwrap();

        // Explicitly drain whatever is left
        while stack.pop().is_some() {}

        assert!(stack.pop().is_none(), "stack not empty");
    });
}
