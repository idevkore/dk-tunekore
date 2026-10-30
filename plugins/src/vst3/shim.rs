//! Calls into plugin code, guarded against C++ exceptions.
//!
//! Rust cannot catch a C++ exception: one that reaches a Rust frame aborts the process. Every
//! call into a plugin therefore goes through the C++ shim (`shim/tk_shim.cpp`), which calls the
//! plugin's function pointer inside `try`/`catch` and returns the exception as a status. The
//! wrappers here take the function pointer straight from the plugin object's vtable, with its
//! exact type, so a signature mismatch is a compile error.
//!
//! The wrappers never allocate or lock when the call succeeds; they are used on the audio thread.

use std::ffi::{c_char, c_void, CStr};

/// A C++ exception a plugin threw, as "type: message".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("plugin threw a C++ exception ({0})")]
pub struct Exception(pub String);

pub type Guarded<T> = Result<T, Exception>;

unsafe extern "C" {
    fn tk_last_exception() -> *const c_char;

    fn tk_i_p(f: *const c_void, out: *mut i32, a1: *mut c_void) -> i32;
    fn tk_u_p(f: *const c_void, out: *mut u32, a1: *mut c_void) -> i32;
    fn tk_i_pp(f: *const c_void, out: *mut i32, a1: *mut c_void, a2: *mut c_void) -> i32;
    fn tk_i_pi(f: *const c_void, out: *mut i32, a1: *mut c_void, a2: i32) -> i32;
    fn tk_i_pb(f: *const c_void, out: *mut i32, a1: *mut c_void, a2: u8) -> i32;
    fn tk_d_pu(f: *const c_void, out: *mut f64, a1: *mut c_void, a2: u32) -> i32;
    fn tk_p_pp(f: *const c_void, out: *mut *mut c_void, a1: *mut c_void, a2: *mut c_void) -> i32;
    fn tk_i_ppp(f: *const c_void, out: *mut i32, a1: *mut c_void, a2: *mut c_void, a3: *mut c_void) -> i32;
    fn tk_i_pip(f: *const c_void, out: *mut i32, a1: *mut c_void, a2: i32, a3: *mut c_void) -> i32;
    fn tk_i_pii(f: *const c_void, out: *mut i32, a1: *mut c_void, a2: i32, a3: i32) -> i32;
    fn tk_i_pud(f: *const c_void, out: *mut i32, a1: *mut c_void, a2: u32, a3: f64) -> i32;
    fn tk_d_pud(f: *const c_void, out: *mut f64, a1: *mut c_void, a2: u32, a3: f64) -> i32;
    fn tk_i_pppp(f: *const c_void, out: *mut i32, a1: *mut c_void, a2: *mut c_void, a3: *mut c_void, a4: *mut c_void) -> i32;
    fn tk_i_piip(f: *const c_void, out: *mut i32, a1: *mut c_void, a2: i32, a3: i32, a4: *mut c_void) -> i32;
    fn tk_i_pudp(f: *const c_void, out: *mut i32, a1: *mut c_void, a2: u32, a3: f64, a4: *mut c_void) -> i32;
    fn tk_i_pupp(f: *const c_void, out: *mut i32, a1: *mut c_void, a2: u32, a3: *mut c_void, a4: *mut c_void) -> i32;
    fn tk_i_piiip(f: *const c_void, out: *mut i32, a1: *mut c_void, a2: i32, a3: i32, a4: i32, a5: *mut c_void) -> i32;
    fn tk_i_piiib(f: *const c_void, out: *mut i32, a1: *mut c_void, a2: i32, a3: i32, a4: i32, a5: u8) -> i32;
    fn tk_i_ppipi(f: *const c_void, out: *mut i32, a1: *mut c_void, a2: *mut c_void, a3: i32, a4: *mut c_void, a5: i32) -> i32;
    fn tk_i_pihhp(f: *const c_void, out: *mut i32, a1: *mut c_void, a2: i32, a3: i16, a4: i16, a5: *mut c_void) -> i32;
    fn tk_b_p(f: *const c_void, out: *mut bool, a1: *mut c_void) -> i32;
    fn tk_b_(f: *const c_void, out: *mut bool) -> i32;
    fn tk_p_(f: *const c_void, out: *mut *mut c_void) -> i32;

    // Test doubles with a plugin method's shape: fn(object, arg) -> tresult.
    fn tk_selftest_ok(a: *mut c_void, b: *mut c_void) -> i32;
    fn tk_selftest_throws_std(a: *mut c_void, b: *mut c_void) -> i32;
    fn tk_selftest_throws_int(a: *mut c_void, b: *mut c_void) -> i32;
    fn tk_selftest_throws_other(a: *mut c_void, b: *mut c_void) -> i32;
}

/// A pointer-like argument: `*mut T` or `*const T`, passed to the plugin as it came.
pub trait RawPtr: Copy {
    fn raw(self) -> *mut c_void;
}
impl<T> RawPtr for *mut T {
    #[inline]
    fn raw(self) -> *mut c_void {
        self.cast()
    }
}
impl<T> RawPtr for *const T {
    #[inline]
    fn raw(self) -> *mut c_void {
        self.cast_mut().cast()
    }
}

#[cold]
fn thrown() -> Exception {
    // SAFETY: returns a NUL-terminated thread-local buffer that stays valid until this thread's
    // next exception; it is copied out immediately.
    let s = unsafe { CStr::from_ptr(tk_last_exception()) };
    Exception(s.to_string_lossy().into_owned())
}

#[inline]
fn finish<T>(status: i32, out: T) -> Guarded<T> {
    if status == 0 { Ok(out) } else { Err(thrown()) }
}

// The wrappers. `T` is the interface struct (`IComponent`, ...). Callers must pass an object the
// function pointer belongs to and arguments the plugin method accepts, as for any COM call.

#[inline]
pub unsafe fn i_p<T>(f: unsafe extern "system" fn(*mut T) -> i32, this: *mut T) -> Guarded<i32> {
    let mut out = 0;
    finish(unsafe { tk_i_p(f as *const c_void, &mut out, this.cast()) }, out)
}
#[inline]
pub unsafe fn u_p<T>(f: unsafe extern "system" fn(*mut T) -> u32, this: *mut T) -> Guarded<u32> {
    let mut out = 0;
    finish(unsafe { tk_u_p(f as *const c_void, &mut out, this.cast()) }, out)
}
#[inline]
pub unsafe fn i_pp<T, A: RawPtr>(f: unsafe extern "system" fn(*mut T, A) -> i32, this: *mut T, a: A) -> Guarded<i32> {
    let mut out = 0;
    finish(unsafe { tk_i_pp(f as *const c_void, &mut out, this.cast(), a.raw()) }, out)
}
#[inline]
pub unsafe fn i_pi<T>(f: unsafe extern "system" fn(*mut T, i32) -> i32, this: *mut T, a: i32) -> Guarded<i32> {
    let mut out = 0;
    finish(unsafe { tk_i_pi(f as *const c_void, &mut out, this.cast(), a) }, out)
}
#[inline]
pub unsafe fn i_pb<T>(f: unsafe extern "system" fn(*mut T, u8) -> i32, this: *mut T, a: u8) -> Guarded<i32> {
    let mut out = 0;
    finish(unsafe { tk_i_pb(f as *const c_void, &mut out, this.cast(), a) }, out)
}
#[inline]
pub unsafe fn d_pu<T>(f: unsafe extern "system" fn(*mut T, u32) -> f64, this: *mut T, a: u32) -> Guarded<f64> {
    let mut out = 0.0;
    finish(unsafe { tk_d_pu(f as *const c_void, &mut out, this.cast(), a) }, out)
}
#[inline]
pub unsafe fn p_pp<T, R, A: RawPtr>(f: unsafe extern "system" fn(*mut T, A) -> *mut R, this: *mut T, a: A) -> Guarded<*mut R> {
    let mut out: *mut c_void = std::ptr::null_mut();
    finish(unsafe { tk_p_pp(f as *const c_void, &mut out, this.cast(), a.raw()) }, out.cast())
}
#[inline]
pub unsafe fn i_ppp<T, A: RawPtr, B: RawPtr>(
    f: unsafe extern "system" fn(*mut T, A, B) -> i32,
    this: *mut T,
    a: A,
    b: B,
) -> Guarded<i32> {
    let mut out = 0;
    finish(unsafe { tk_i_ppp(f as *const c_void, &mut out, this.cast(), a.raw(), b.raw()) }, out)
}
#[inline]
pub unsafe fn i_pip<T, A: RawPtr>(f: unsafe extern "system" fn(*mut T, i32, A) -> i32, this: *mut T, i: i32, a: A) -> Guarded<i32> {
    let mut out = 0;
    finish(unsafe { tk_i_pip(f as *const c_void, &mut out, this.cast(), i, a.raw()) }, out)
}
#[inline]
pub unsafe fn i_pii<T>(f: unsafe extern "system" fn(*mut T, i32, i32) -> i32, this: *mut T, a: i32, b: i32) -> Guarded<i32> {
    let mut out = 0;
    finish(unsafe { tk_i_pii(f as *const c_void, &mut out, this.cast(), a, b) }, out)
}
#[inline]
pub unsafe fn i_pud<T>(f: unsafe extern "system" fn(*mut T, u32, f64) -> i32, this: *mut T, id: u32, v: f64) -> Guarded<i32> {
    let mut out = 0;
    finish(unsafe { tk_i_pud(f as *const c_void, &mut out, this.cast(), id, v) }, out)
}
#[inline]
pub unsafe fn d_pud<T>(f: unsafe extern "system" fn(*mut T, u32, f64) -> f64, this: *mut T, id: u32, v: f64) -> Guarded<f64> {
    let mut out = 0.0;
    finish(unsafe { tk_d_pud(f as *const c_void, &mut out, this.cast(), id, v) }, out)
}
#[inline]
pub unsafe fn i_pppp<T, A: RawPtr, B: RawPtr, C: RawPtr>(
    f: unsafe extern "system" fn(*mut T, A, B, C) -> i32,
    this: *mut T,
    a: A,
    b: B,
    c: C,
) -> Guarded<i32> {
    let mut out = 0;
    finish(unsafe { tk_i_pppp(f as *const c_void, &mut out, this.cast(), a.raw(), b.raw(), c.raw()) }, out)
}
#[inline]
pub unsafe fn i_piip<T, A: RawPtr>(
    f: unsafe extern "system" fn(*mut T, i32, i32, A) -> i32,
    this: *mut T,
    a: i32,
    b: i32,
    c: A,
) -> Guarded<i32> {
    let mut out = 0;
    finish(unsafe { tk_i_piip(f as *const c_void, &mut out, this.cast(), a, b, c.raw()) }, out)
}
#[inline]
pub unsafe fn i_pudp<T, A: RawPtr>(
    f: unsafe extern "system" fn(*mut T, u32, f64, A) -> i32,
    this: *mut T,
    id: u32,
    v: f64,
    p: A,
) -> Guarded<i32> {
    let mut out = 0;
    finish(unsafe { tk_i_pudp(f as *const c_void, &mut out, this.cast(), id, v, p.raw()) }, out)
}
#[inline]
pub unsafe fn i_pupp<T, A: RawPtr, B: RawPtr>(
    f: unsafe extern "system" fn(*mut T, u32, A, B) -> i32,
    this: *mut T,
    id: u32,
    a: A,
    b: B,
) -> Guarded<i32> {
    let mut out = 0;
    finish(unsafe { tk_i_pupp(f as *const c_void, &mut out, this.cast(), id, a.raw(), b.raw()) }, out)
}
#[inline]
pub unsafe fn i_piiip<T, A: RawPtr>(
    f: unsafe extern "system" fn(*mut T, i32, i32, i32, A) -> i32,
    this: *mut T,
    a: i32,
    b: i32,
    c: i32,
    d: A,
) -> Guarded<i32> {
    let mut out = 0;
    finish(unsafe { tk_i_piiip(f as *const c_void, &mut out, this.cast(), a, b, c, d.raw()) }, out)
}
#[inline]
pub unsafe fn i_piiib<T>(
    f: unsafe extern "system" fn(*mut T, i32, i32, i32, u8) -> i32,
    this: *mut T,
    a: i32,
    b: i32,
    c: i32,
    d: u8,
) -> Guarded<i32> {
    let mut out = 0;
    finish(unsafe { tk_i_piiib(f as *const c_void, &mut out, this.cast(), a, b, c, d) }, out)
}
#[inline]
pub unsafe fn i_ppipi<T, A: RawPtr, B: RawPtr>(
    f: unsafe extern "system" fn(*mut T, A, i32, B, i32) -> i32,
    this: *mut T,
    a: A,
    na: i32,
    b: B,
    nb: i32,
) -> Guarded<i32> {
    let mut out = 0;
    finish(unsafe { tk_i_ppipi(f as *const c_void, &mut out, this.cast(), a.raw(), na, b.raw(), nb) }, out)
}
#[inline]
pub unsafe fn i_pihhp<T, A: RawPtr>(
    f: unsafe extern "system" fn(*mut T, i32, i16, i16, A) -> i32,
    this: *mut T,
    a: i32,
    b: i16,
    c: i16,
    d: A,
) -> Guarded<i32> {
    let mut out = 0;
    finish(unsafe { tk_i_pihhp(f as *const c_void, &mut out, this.cast(), a, b, c, d.raw()) }, out)
}
/// A module-level function with one pointer argument and no object (`bundleEntry`).
#[inline]
pub unsafe fn b_p<A: RawPtr>(f: unsafe extern "C" fn(A) -> bool, a: A) -> Guarded<bool> {
    let mut out = false;
    finish(unsafe { tk_b_p(f as *const c_void, &mut out, a.raw()) }, out)
}
/// A module-level function with no arguments that returns bool (`bundleExit`, `ExitDll`).
#[inline]
pub unsafe fn b_(f: unsafe extern "system" fn() -> bool) -> Guarded<bool> {
    let mut out = false;
    finish(unsafe { tk_b_(f as *const c_void, &mut out) }, out)
}
/// `GetPluginFactory`.
#[inline]
pub unsafe fn p_<R>(f: unsafe extern "system" fn() -> *mut R) -> Guarded<*mut R> {
    let mut out: *mut c_void = std::ptr::null_mut();
    finish(unsafe { tk_p_(f as *const c_void, &mut out) }, out.cast())
}

#[cfg(test)]
mod tests {
    use super::*;

    type Method = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;

    /// Calls a test double through the shim, as a plugin method taking one pointer.
    fn call(f: Method) -> Guarded<i32> {
        let mut out = 0;
        // SAFETY: the test doubles ignore their arguments.
        finish(unsafe { tk_i_pp(f as *const c_void, &mut out, std::ptr::null_mut(), std::ptr::null_mut()) }, out)
    }

    #[test]
    fn a_normal_call_returns_its_result() {
        assert_eq!(call(tk_selftest_ok), Ok(42));
    }

    #[test]
    fn a_std_exception_comes_back_as_an_error_with_its_type_and_message() {
        let e = call(tk_selftest_throws_std).unwrap_err();
        assert!(e.0.contains("runtime_error"), "{e}");
        assert!(e.0.contains("boom from a plugin"), "{e}");
    }

    #[test]
    fn other_thrown_things_are_caught_too() {
        let e = call(tk_selftest_throws_int).unwrap_err();
        assert_eq!(e.0, "int: 7");
        let e = call(tk_selftest_throws_other).unwrap_err();
        assert!(e.0.contains("Odd"), "{e}");
    }

    #[test]
    fn the_message_belongs_to_the_thread_and_the_shim_is_usable_again() {
        let t = std::thread::spawn(|| call(tk_selftest_throws_std).unwrap_err().0);
        assert!(t.join().unwrap().contains("boom"));
        assert_eq!(call(tk_selftest_ok), Ok(42));
    }

    #[test]
    fn the_typed_wrappers_reject_nothing_at_run_time_but_check_signatures_at_compile_time() {
        // `i_pp` takes the exact vtable function type; this compiles only because it matches.
        unsafe extern "system" fn f(_this: *mut c_void, arg: *mut c_void) -> i32 {
            if arg.is_null() { 1 } else { 2 }
        }
        // SAFETY: `f` ignores `this` and only inspects `arg`.
        let r = unsafe { i_pp(f, std::ptr::null_mut(), std::ptr::null_mut::<c_void>()) };
        assert_eq!(r, Ok(1));
    }
}
