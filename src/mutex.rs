use std::ops::DerefMut;

pub trait MutexT {
    type T;
    type Guard<'a>: DerefMut<Target = Self::T>
    where
        Self: 'a;

    fn new(val: Self::T) -> Self;
    fn lock(&self) -> Self::Guard<'_>;
}

impl<T> MutexT for std::sync::Mutex<T> {
    type T = T;
    type Guard<'a>
        = std::sync::MutexGuard<'a, T>
    where
        Self: 'a;

    fn new(val: T) -> Self {
        std::sync::Mutex::new(val)
    }

    fn lock(&self) -> Self::Guard<'_> {
        self.lock().unwrap()
    }
}

#[cfg(feature = "parking_lot")]
impl<T> MutexT for parking_lot::Mutex<T> {
    type T = T;
    type Guard<'a>
        = parking_lot::MutexGuard<'a, T>
    where
        Self: 'a;

    fn new(val: T) -> Self {
        parking_lot::Mutex::new(val)
    }

    fn lock(&self) -> Self::Guard<'_> {
        self.lock()
    }
}
