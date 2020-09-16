use std::mem::{self, ManuallyDrop};
use std::sync::Arc;
use std::task::{RawWaker, RawWakerVTable, Waker};

pub fn waker_fn<F: Fn() + Send + Sync + 'static>(f: F) -> Waker {
    let raw = Arc::into_raw(Arc::new(f)) as *const ();
    let vtable = &Helper::<F>::VTABLE;
    unsafe { Waker::from_raw(RawWaker::new(raw, vtable)) }
}

struct Helper<F>(F);

impl<F: Fn() + Send + Sync + 'static> Helper<F> {
    const VTABLE: RawWakerVTable =
        RawWakerVTable::new(Self::clone, Self::wake, Self::wake_by_ref, Self::drop);

    unsafe fn clone(f: *const ()) -> RawWaker {
        let arc = ManuallyDrop::new(Arc::from_raw(f as *const F));
        #[allow(clippy::redundant_clone)]
        mem::forget(arc.clone());
        RawWaker::new(f, &Self::VTABLE)
    }

    unsafe fn wake(f: *const ()) {
        let arc = Arc::from_raw(f as *const F);
        arc();
    }

    unsafe fn wake_by_ref(f: *const ()) {
        let arc = ManuallyDrop::new(Arc::from_raw(f as *const F));
        arc();
    }

    unsafe fn drop(f: *const ()) {
        drop(Arc::from_raw(f as *const F));
    }
}
