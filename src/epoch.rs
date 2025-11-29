#![allow(unused)]
use std::cell::{Cell, RefCell};
use std::mem;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

static EPOCH: Epoch = Epoch::new();

/// Every thread has got three lists. It starts pushing the things
/// into the recent list. One an operation it checks the global epoch
/// if it finds that it has advanced or if the thread itself advances
/// the global epoch, it will deallocate the memory pointed to by the
/// pointers in the LAST list, make PREVIOUS the last, RECENT the previous
/// and RECENT will be a List::new().
thread_local! {
    static RECENT: RefCell<List> = RefCell::new(List::new());
    static PREVIOUS: RefCell<List> = RefCell::new(List::new());
}

/// TODO: Add loom tests. Find a way to use the loom variant the thread local with
/// lazily initialized statics. The loom::thread_local macro does not match for a
/// macro call inside of it. If it were to be true we could have used lazy_static.

/// Holds the current state.
struct Epoch {
    counter: AtomicUsize,
    registrations: Registrations,
}

impl Epoch {
    const fn new() -> Self {
        Self {
            counter: AtomicUsize::new(0),
            registrations: Registrations::new(),
        }
    }
}

/// Holder of the retired things.
/// Has got three active instances at any point of time.
struct List {
    stamp: isize,
    elements: Vec<ListEntry>,
}

impl List {
    const fn new() -> Self {
        Self {
            stamp: -1,
            elements: Vec::new(),
        }
    }
}

struct ListEntry {
    value: NonNull<dyn Common>,
    deleter: &'static dyn Reclaim,
}

impl ListEntry {
    fn new(value: *mut dyn Common, deleter: &'static dyn Reclaim) -> Option<ListEntry> {
        if let Some(ptr) = NonNull::new(value) {
            let ret = ListEntry {
                value: ptr,
                deleter,
            };
            Some(ret)
        } else {
            None
        }
    }
}

/// This trait is necessary to create a common characteristic for every
/// type so that they can be used to cast from and back into a type.
/// This becomes important at the time of actually reclaiming the memory
/// and also for them to be stored inside the retired list.
pub trait Common {}

impl<T> Common for T {}

/// A trait to make sure that the pointers are dropped in accordance with
/// how they were constructed in the first place.
pub trait Reclaim {
    fn reclaim(&self, ptr: *mut dyn Common);
}

/// A type for reclaiming memory pointed to by raw pointers that
/// were originally constructed from Box.
pub struct DropBox;

impl DropBox {
    pub const fn new() -> Self {
        DropBox
    }
}

impl Reclaim for DropBox {
    fn reclaim(&self, ptr: *mut dyn Common) {
        /// SAFETY:
        ///     All the pointer safety requirements such as
        ///     proper alignment must be upheld. Further, DropBox
        ///     is meant to be used when the underlying raw pointer
        ///     was constructed using a Box. Not maintaining this
        ///     invariant will lead to a instant Undefined Behaviour.
        let owned = unsafe { Box::from_raw(ptr) };
        mem::drop(owned);
    }
}

/// A type for reclaiming memory pointed to by raw pointers that were
/// constructed directly.
pub struct DropPointer;

impl DropPointer {
    pub const fn new() -> Self {
        DropPointer
    }
}

impl Reclaim for DropPointer {
    fn reclaim(&self, ptr: *mut dyn Common) {
        /// SAFETY:
        ///    The safety requirements can be read from
        ///    std::ptr::drop_in_place() in the standard
        ///    library docs.
        ///    https://doc.rust-lang.org/std/ptr/fn.drop_in_place.html
        unsafe {
            ptr::drop_in_place(ptr);
        }
    }
}

/// List of all the registrations.
/// None of the registrations will be dropped until
/// the end of the program.
/// Therefore the ABA problem cannot arise.
struct Registrations {
    head: AtomicPtr<Registration>,
}

impl Registrations {
    const fn new() -> Self {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

/// Every thread registers itself before it does any operation.
pub struct Registration {
    counter: Cell<isize>,
    next: AtomicPtr<Registration>,
    active: AtomicBool,
}

impl Registration {
    pub fn find_register() -> Option<Worker> {
        let mut current = EPOCH.registrations.head.load(Ordering::Acquire);
        while !current.is_null() {
            /// SAFETY:
            ///    The raw pointer cannot be null as a registration is
            ///    not deallocated until the end of the program.
            ///    Therefore, the operation is safe.
            let deref = unsafe { &(*current) };
            if deref
                .active
                .compare_exchange(true, false, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                deref.counter.set(-1);
                let ret = Worker { reg: deref };
                return Some(ret);
            } else {
                current = deref.next.load(Ordering::Acquire);
            }
        }
        None
    }

    pub fn create_register() -> Worker {
        loop {
            let current = EPOCH.registrations.head.load(Ordering::Acquire);
            let new = Registration {
                counter: Cell::new(-1),
                next: AtomicPtr::new(current),
                active: AtomicBool::new(false),
            };
            let boxed = Box::into_raw(Box::new(new));
            if EPOCH
                .registrations
                .head
                .compare_exchange(current, boxed, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                /// SAFETY:
                ///    The pointer being dereferenced cannot be null
                ///    as a registration is never deallocated until the
                ///    end of the program. Therefore the operation is safe.
                let shared = unsafe { &(*boxed) };
                let ret = Worker { reg: shared };
                return ret;
            } else {
                /// SAFETY:
                ///    As the function makes it clear, the underlying
                ///    raw pointer can never be null and the function is
                ///    called only once on a pointer. Therefore,
                ///    the operation is safe.
                let _ = unsafe { Box::from_raw(boxed) };
            }
        }
    }
}

/// This is the type that is user uses to load and swap pointers
/// in the AtomicPtr. It uses the RAII pattern for setting the thread
/// to an inactive state in case of loads and the implementation of swap
/// does it in the method call itself.
pub struct Worker {
    reg: &'static Registration,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.reg.active.store(true, Ordering::Relaxed);
    }
}

/// A type which when dropped signals that the thread is no
/// longer in a critcal section.
pub struct Res<'a, T> {
    worker: &'a Worker,
    ptr: *mut T,
}

impl<T> Drop for Res<'_, T> {
    fn drop(&mut self) {
        self.worker.reg.counter.set(-1);
    }
}

impl Worker {
    pub fn load<'a, T>(&'a self, ptr: &AtomicPtr<T>) -> Res<'a, T> {
        let count = Self::try_advance();
        self.reg.counter.set(count as isize);
        let pointer = ptr.load(Ordering::Acquire);
        Res {
            worker: self,
            ptr: pointer,
        }
    }

    /// The deleter parameter signifies a way the pointer that is going to be dropped.
    /// Currently this will work as expected if the user is sure that the CAS will succeed
    /// in the first attempt. If not so, the user must ensure that all the pointers are
    /// constructed using a common method that is either a box or directly.
    pub fn swap<T>(&self, ptr: &AtomicPtr<T>, new: T, deleter: &'static dyn Reclaim) {
        let count = Self::try_advance();
        self.reg.counter.set(count as isize);
        let boxed = Box::into_raw(Box::new(new));
        let mut current = ptr.load(Ordering::Acquire);
        loop {
            if ptr
                .compare_exchange(current, boxed, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                let stamp = RECENT.with(|interior| interior.borrow().stamp);
                if stamp < count as isize {
                    Self::rearrange(current as *mut dyn Common, deleter);
                    self.reg.counter.set(-1);
                    return;
                } else {
                    let entry = ListEntry::new(current as *mut dyn Common, deleter);
                    if let Some(e) = entry {
                        RECENT.with(|interior| interior.borrow_mut().elements.push(e));
                    }
                    self.reg.counter.set(-1);
                    return;
                }
            } else {
                current = ptr.load(Ordering::Acquire);
            }
        }
        self.reg.counter.set(-1);
    }

    fn rearrange(ptr: *mut dyn Common, deleter: &'static dyn Reclaim) {
        let counter = EPOCH.counter.load(Ordering::Relaxed) as isize;
        let entry = ListEntry::new(ptr, deleter);
        let vec = if let Some(e) = entry {
            vec![e]
        } else {
            Vec::new()
        };
        let make_prev = RECENT.with(|interior| {
            let mut borrowed = interior.borrow_mut();
            borrowed.stamp = counter;
            mem::replace(&mut borrowed.elements, vec)
        });
        let rec = PREVIOUS.with(|interior| {
            let mut borrowed = interior.borrow_mut();
            borrowed.stamp = counter - 1;
            mem::replace(&mut borrowed.elements, make_prev)
        });
        for element in rec {
            element.deleter.reclaim(element.value.as_ptr());
        }
    }

    fn try_advance() -> usize {
        let count = EPOCH.counter.load(Ordering::Relaxed);
        let mut current = EPOCH.registrations.head.load(Ordering::Acquire);
        while !current.is_null() {
            /// SAFETY:
            ///    The operation is safe because we check the
            ///    nullability of current before dereferencing
            ///    and the the responsibility of giving a safe pointer
            ///    in this case does not rest on the user but is a part
            ///    of the implementation itself and I make sure that those
            ///    safety invariants are upheld.
            let reg = unsafe { &(*current) };
            let reg_counter = reg.counter.get();
            if reg_counter < 0 || reg_counter == count as isize {
                current = reg.next.load(Ordering::Acquire);
            } else {
                return count;
            }
        }
        let ret = count + 1;
        let _ = EPOCH
            .counter
            .compare_exchange(count, ret, Ordering::Relaxed, Ordering::Relaxed);
        return ret;
    }
}