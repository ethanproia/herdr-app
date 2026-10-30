//! Raw IOSurface, CoreFoundation and bootstrap FFI (macOS only).
#![allow(non_upper_case_globals, non_snake_case)]

use std::ffi::{CString, c_char, c_void};

pub type IOSurfaceRef = *mut c_void;
type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFAllocatorRef = *const c_void;
type MachPort = u32;
type KernReturn = i32;

pub const K_IOSURFACE_LOCK_READ_ONLY: u32 = 1;
const kCFNumberSInt32Type: i32 = 3;
const BGRA: i32 = i32::from_be_bytes(*b"BGRA");

#[repr(C)]
struct CFDictionaryCallBacks([usize; 6]);

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: CFDictionaryCallBacks;
    static kCFTypeDictionaryValueCallBacks: CFDictionaryCallBacks;
    static kCFBooleanTrue: CFTypeRef;
    fn CFDictionaryCreate(a: CFAllocatorRef, k: *const CFTypeRef, v: *const CFTypeRef, n: isize, kc: *const CFDictionaryCallBacks, vc: *const CFDictionaryCallBacks) -> CFDictionaryRef;
    fn CFNumberCreate(a: CFAllocatorRef, t: i32, v: *const c_void) -> CFTypeRef;
    pub fn CFRetain(c: CFTypeRef) -> CFTypeRef;
    pub fn CFRelease(c: CFTypeRef);
}

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    static kIOSurfaceWidth: CFStringRef;
    static kIOSurfaceHeight: CFStringRef;
    static kIOSurfaceBytesPerElement: CFStringRef;
    static kIOSurfacePixelFormat: CFStringRef;
    static kIOSurfaceIsGlobal: CFStringRef;
    fn IOSurfaceCreate(props: CFDictionaryRef) -> IOSurfaceRef;
    pub fn IOSurfaceLookup(id: u32) -> IOSurfaceRef;
    pub fn IOSurfaceGetID(s: IOSurfaceRef) -> u32;
    pub fn IOSurfaceGetWidth(s: IOSurfaceRef) -> usize;
    pub fn IOSurfaceGetHeight(s: IOSurfaceRef) -> usize;
    pub fn IOSurfaceGetBytesPerRow(s: IOSurfaceRef) -> usize;
    pub fn IOSurfaceGetBaseAddress(s: IOSurfaceRef) -> *mut c_void;
    pub fn IOSurfaceLock(s: IOSurfaceRef, opts: u32, seed: *mut u32) -> KernReturn;
    pub fn IOSurfaceUnlock(s: IOSurfaceRef, opts: u32, seed: *mut u32) -> KernReturn;
    pub fn IOSurfaceIsInUse(s: IOSurfaceRef) -> u8;
    pub fn IOSurfaceIncrementUseCount(s: IOSurfaceRef);
    pub fn IOSurfaceDecrementUseCount(s: IOSurfaceRef);
    fn IOSurfaceCreateMachPort(s: IOSurfaceRef) -> MachPort;
    fn IOSurfaceLookupFromMachPort(p: MachPort) -> IOSurfaceRef;
}

unsafe extern "C" {
    static bootstrap_port: MachPort;
    static mach_task_self_: MachPort;
    fn bootstrap_register2(bp: MachPort, name: *const c_char, sp: MachPort, flags: u64) -> KernReturn;
    fn bootstrap_look_up(bp: MachPort, name: *const c_char, sp: *mut MachPort) -> KernReturn;
    fn mach_port_deallocate(task: MachPort, name: MachPort) -> KernReturn;
}

/// A BGRA, 4-bytes-per-pixel surface flagged global so `IOSurfaceLookup` works across processes.
pub unsafe fn create_bgra(w: u32, h: u32) -> IOSurfaceRef {
    unsafe {
        let num = |v: i32| CFNumberCreate(std::ptr::null(), kCFNumberSInt32Type, &v as *const i32 as *const c_void);
        let keys = [kIOSurfaceWidth, kIOSurfaceHeight, kIOSurfaceBytesPerElement, kIOSurfacePixelFormat, kIOSurfaceIsGlobal];
        let vals = [num(w as i32), num(h as i32), num(4), num(BGRA), CFRetain(kCFBooleanTrue)];
        let dict = CFDictionaryCreate(std::ptr::null(), keys.as_ptr(), vals.as_ptr(), keys.len() as isize, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
        for v in vals {
            CFRelease(v);
        }
        let s = IOSurfaceCreate(dict);
        CFRelease(dict);
        s
    }
}

/// Register a send right for `s` with the bootstrap server under `name`; returns the port to free later.
pub unsafe fn register_bootstrap(s: IOSurfaceRef, name: &str) -> Result<MachPort, String> {
    unsafe {
        let port = IOSurfaceCreateMachPort(s);
        if port == 0 {
            return Err("IOSurfaceCreateMachPort returned MACH_PORT_NULL".into());
        }
        let c = CString::new(name).map_err(|e| e.to_string())?;
        let k = bootstrap_register2(bootstrap_port, c.as_ptr(), port, 0);
        if k != 0 {
            mach_port_deallocate(mach_task_self_, port);
            return Err(format!("bootstrap_register2({name}) = {k}"));
        }
        Ok(port)
    }
}

pub unsafe fn lookup_bootstrap(name: &str) -> Result<IOSurfaceRef, String> {
    unsafe {
        let c = CString::new(name).map_err(|e| e.to_string())?;
        let mut port: MachPort = 0;
        let k = bootstrap_look_up(bootstrap_port, c.as_ptr(), &mut port);
        if k != 0 {
            return Err(format!("bootstrap_look_up({name}) = {k}"));
        }
        let s = IOSurfaceLookupFromMachPort(port);
        mach_port_deallocate(mach_task_self_, port);
        Ok(s)
    }
}

pub unsafe fn release_port(port: MachPort) {
    unsafe {
        mach_port_deallocate(mach_task_self_, port);
    }
}
