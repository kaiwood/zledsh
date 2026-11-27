use std::ffi::{CStr, CString};

pub(crate) struct Locale(libc::locale_t);

impl Locale {
    pub(crate) fn new(ctype: &str, collate: &str) -> Option<Self> {
        let mut locale = Self(unsafe {
            libc::newlocale(libc::LC_ALL_MASK, c"C".as_ptr(), std::ptr::null_mut())
        });
        if locale.0.is_null() {
            return None;
        }
        for (mask, name) in [
            (libc::LC_CTYPE_MASK, ctype),
            (libc::LC_COLLATE_MASK, collate),
        ] {
            if name == "C" || name == "POSIX" {
                continue;
            }
            if let Ok(name) = CString::new(name) {
                // On success newlocale consumes the previous handle; on failure it does not.
                let next = unsafe { libc::newlocale(mask, name.as_ptr(), locale.0) };
                if !next.is_null() {
                    locale.0 = next;
                }
            }
        }
        Some(locale)
    }

    pub(crate) fn with<T>(&self, f: impl FnOnce() -> T) -> T {
        struct Restore(libc::locale_t);
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe {
                    libc::uselocale(self.0);
                }
            }
        }
        // The locale outlives the guard and only the current thread is affected.
        let previous = unsafe { libc::uselocale(self.0) };
        let _guard = (!previous.is_null()).then(|| Restore(previous));
        f()
    }

    pub(crate) fn collate(left: &CStr, right: &CStr) -> std::cmp::Ordering {
        unsafe { libc::strcoll(left.as_ptr(), right.as_ptr()) }.cmp(&0)
    }
}

impl Drop for Locale {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                libc::freelocale(self.0);
            }
        }
    }
}
