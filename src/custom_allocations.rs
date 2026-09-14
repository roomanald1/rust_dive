use std::alloc::Layout;

struct Bump {
    buf: *mut u8,
    size: usize,
    offset: usize,
    _backing: Vec<u8>,
}

impl Bump {
    fn new(size: usize) -> Self {
        let mut backing = vec![0u8; size];
        let buf = backing.as_mut_ptr();
        Self {
            buf,
            size,
            offset: 0,
            _backing: backing,
        }
    }

    fn alloc_raw(&mut self, layout: Layout) -> *mut u8 {
        let align = layout.align();
        let alloc_size = layout.size();

        // Align the offset
        let aligned = (self.offset + align - 1) & !(align - 1);

        if aligned + alloc_size > self.size {
            return std::ptr::null_mut(); // out of memory
        }

        let ptr = unsafe { self.buf.add(aligned) };
        self.offset = aligned + alloc_size;
        ptr
    }

    pub fn alloc<T>(&mut self, value: T) -> &mut T {
        let x = self.alloc_raw(Layout::new::<T>()) as *mut T;
        unsafe {
            x.write(value);
            &mut *x
        }
    }

    pub fn reset(&mut self) {
        self.offset = 0;
    }
}

#[test]
fn test() {
    let mut bump = Bump::new(1024);
    let x = bump.alloc(12.5);
    println!("{}", x)
}
