//! Dynamic access to the private Spotlight exclusion calls in
//! `Metadata.framework`.
//!
//! The implementation mirrors the API usage found in `rohiitgit/nohog`
//! (`Sources/NohogCore/Reconcile.swift`, commit `e31bd29`):
//!
//! ```text
//! dlopen("/System/Library/Frameworks/CoreServices.framework/Frameworks/Metadata.framework/Metadata", RTLD_NOW)
//! _MDCopyExclusionList() -> CFArrayRef
//! _MDSetExclusion(CFStringRef path, uint32_t excluded) -> int32_t
//! ```
//!
//! The return value of `_MDSetExclusion` is not a success flag.  On macOS
//! 27.2.1 it is `0` on success and negative/small positive error codes
//! otherwise, but callers must re-read `_MDCopyExclusionList` before claiming
//! that an exclusion exists.  This module therefore exposes both operations
//! and leaves verification to the caller.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;

use thiserror::Error;

type CFTypeRef = *const c_void;
type CFArrayRef = *const c_void;
type CFStringRef = *const c_void;
type CFIndex = isize;
type CFStringEncoding = u32;
type Boolean = u8;

/// `kCFStringEncodingUTF8`.
const CF_STRING_ENCODING_UTF8: CFStringEncoding = 0x0800_0100;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithBytes(
        alloc: *const c_void,
        bytes: *const u8,
        num_bytes: CFIndex,
        encoding: CFStringEncoding,
        is_external_representation: Boolean,
    ) -> CFStringRef;
    fn CFStringGetLength(string: CFStringRef) -> CFIndex;
    fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: CFStringEncoding) -> CFIndex;
    fn CFStringGetCString(
        string: CFStringRef,
        buffer: *mut c_char,
        buffer_size: CFIndex,
        encoding: CFStringEncoding,
    ) -> Boolean;
    fn CFArrayGetCount(array: CFArrayRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(array: CFArrayRef, index: CFIndex) -> *const c_void;
    fn CFGetTypeID(value: CFTypeRef) -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFRelease(value: CFTypeRef);
}

type CopyExclusionListFn = unsafe extern "C" fn() -> CFArrayRef;
type SetExclusionFn = unsafe extern "C" fn(CFStringRef, u32) -> c_int;

/// The nested framework path used by nohog.  On current macOS the executable
/// is supplied by the dyld shared cache even when no loose file is visible.
pub const METADATA_FRAMEWORK: &str =
    "/System/Library/Frameworks/CoreServices.framework/Frameworks/Metadata.framework/Metadata";

#[derive(Debug, Error)]
pub enum MetadataError {
    #[error("Spotlight exclusion API unavailable: {0}")]
    Unavailable(String),
    #[error("Spotlight exclusion API call failed: {0}")]
    Call(String),
    #[error("Spotlight returned an unexpected exclusion list: {0}")]
    InvalidList(String),
}

/// A dynamically resolved handle to the private Metadata.framework calls.
pub struct MetadataBackend {
    copy_exclusion_list: CopyExclusionListFn,
    set_exclusion: SetExclusionFn,
    /// Kept open for the lifetime of the process; `dlclose` is deliberately
    /// never called because Metadata.framework is shared by other frameworks.
    _handle: usize,
}

// Function pointers are immutable and the handle is only used as an opaque
// token.  The backend performs no mutation of its own state, so it can be
// shared by the daemon's scan threads.
unsafe impl Send for MetadataBackend {}
unsafe impl Sync for MetadataBackend {}

impl MetadataBackend {
    /// Resolve the private symbols.  This never calls them, so it is safe to use
    /// as an availability probe.
    pub fn open() -> Result<Self, MetadataError> {
        let framework = CString::new(METADATA_FRAMEWORK)
            .map_err(|_| MetadataError::Unavailable("framework path contains NUL".into()))?;
        // SAFETY: `framework` is a valid NUL-terminated path.  RTLD_NOW resolves
        // the framework immediately; the handle is intentionally leaked.
        let handle = unsafe { dlopen(framework.as_ptr(), RTLD_NOW) };
        if handle.is_null() {
            return Err(MetadataError::Unavailable(format!(
                "cannot load Metadata.framework: {}",
                last_dl_error()
            )));
        }

        let copy = resolve(handle, "_MDCopyExclusionList")?;
        let set = resolve(handle, "_MDSetExclusion")?;

        // SAFETY: the symbols have the signatures documented above.  This is
        // the same cast nohog performs in Swift (`unsafeBitCast`).
        let copy_exclusion_list: CopyExclusionListFn = unsafe { std::mem::transmute(copy) };
        let set_exclusion: SetExclusionFn = unsafe { std::mem::transmute(set) };

        Ok(Self {
            copy_exclusion_list,
            set_exclusion,
            _handle: handle as usize,
        })
    }

    pub fn name(&self) -> &'static str {
        "Metadata.framework (_MDCopyExclusionList/_MDSetExclusion)"
    }

    /// Read Spotlight's Search Privacy list.  The returned strings are exactly
    /// what Spotlight reports; callers should canonicalise them before
    /// comparing filesystem paths.
    pub fn list(&self) -> Result<Vec<String>, MetadataError> {
        // SAFETY: `_MDCopyExclusionList` takes no arguments.  The "Copy" naming
        // convention means the returned CFArray is owned by us.
        let array = unsafe { (self.copy_exclusion_list)() };
        if array.is_null() {
            return Ok(Vec::new());
        }

        // SAFETY: CFTypeID checks are safe for any non-null CF object.  If the
        // private function ever changes shape, fail closed instead of
        // dereferencing an unrelated object.
        let array_type = unsafe { CFArrayGetTypeID() };
        let actual_type = unsafe { CFGetTypeID(array) };
        if actual_type != array_type {
            unsafe { CFRelease(array) };
            return Err(MetadataError::InvalidList(format!(
                "expected CFArray (type {array_type}), got CF type {actual_type}"
            )));
        }

        let count = unsafe { CFArrayGetCount(array) };
        if count < 0 {
            unsafe { CFRelease(array) };
            return Err(MetadataError::InvalidList("negative array count".into()));
        }

        let string_type = unsafe { CFStringGetTypeID() };
        let mut paths = Vec::with_capacity(count as usize);
        for index in 0..count {
            // SAFETY: index is within the array's count.
            let value = unsafe { CFArrayGetValueAtIndex(array, index) };
            if value.is_null() {
                continue;
            }
            let value_type = unsafe { CFGetTypeID(value) };
            if value_type != string_type {
                unsafe { CFRelease(array) };
                return Err(MetadataError::InvalidList(format!(
                    "entry {index} is CF type {value_type}, not CFString"
                )));
            }
            match unsafe { cf_string_to_string(value as CFStringRef) } {
                Ok(path) => paths.push(path),
                Err(error) => {
                    unsafe { CFRelease(array) };
                    return Err(error);
                }
            }
        }

        unsafe { CFRelease(array) };
        Ok(paths)
    }

    /// Ask Spotlight to add or remove a path.  This only issues the request; it
    /// does not prove that Spotlight applied it.
    pub fn set_excluded(&self, path: &str, excluded: bool) -> Result<i32, MetadataError> {
        if path.as_bytes().contains(&0) {
            return Err(MetadataError::Call("path contains NUL".into()));
        }
        let path_bytes = path.as_bytes();
        // SAFETY: null allocator means kCFAllocatorDefault; the byte pointer and
        // length describe a live Rust slice, and UTF-8 is a valid encoding.
        let string = unsafe {
            CFStringCreateWithBytes(
                ptr::null(),
                path_bytes.as_ptr(),
                path_bytes.len() as CFIndex,
                CF_STRING_ENCODING_UTF8,
                0,
            )
        };
        if string.is_null() {
            return Err(MetadataError::Call(
                "CFStringCreateWithBytes returned NULL".into(),
            ));
        }
        // SAFETY: the string is a valid CFString.  The second argument is a
        // 32-bit value (verified from the disassembly: the implementation
        // stores only w-register data into its mds request).
        let result = unsafe { (self.set_exclusion)(string, u32::from(excluded)) };
        unsafe { CFRelease(string) };
        Ok(result)
    }
}

fn resolve(handle: *mut c_void, symbol: &str) -> Result<*mut c_void, MetadataError> {
    let name = CString::new(symbol)
        .map_err(|_| MetadataError::Unavailable(format!("symbol {symbol} contains NUL")))?;
    // SAFETY: handle came from dlopen and name is NUL-terminated.
    let pointer = unsafe { dlsym(handle, name.as_ptr()) };
    if pointer.is_null() {
        return Err(MetadataError::Unavailable(format!(
            "symbol {symbol} missing from Metadata.framework: {}",
            last_dl_error()
        )));
    }
    Ok(pointer)
}

fn last_dl_error() -> String {
    // SAFETY: dlerror returns either NULL or a NUL-terminated string owned by
    // the dynamic loader.  The string is copied immediately.
    unsafe {
        let error = dlerror();
        if error.is_null() {
            "unknown dynamic loader error".into()
        } else {
            CStr::from_ptr(error).to_string_lossy().into_owned()
        }
    }
}

/// Convert a CFString to a Rust String with an explicit UTF-8 rounding trip.
unsafe fn cf_string_to_string(string: CFStringRef) -> Result<String, MetadataError> {
    let length = unsafe { CFStringGetLength(string) };
    let maximum = unsafe { CFStringGetMaximumSizeForEncoding(length, CF_STRING_ENCODING_UTF8) };
    if maximum <= 0 {
        return Err(MetadataError::InvalidList(
            "CFStringGetMaximumSizeForEncoding returned no space".into(),
        ));
    }
    let mut buffer = vec![0u8; maximum as usize + 1];
    let ok = unsafe {
        CFStringGetCString(
            string,
            buffer.as_mut_ptr() as *mut c_char,
            buffer.len() as CFIndex,
            CF_STRING_ENCODING_UTF8,
        )
    };
    if ok == 0 {
        return Err(MetadataError::InvalidList(
            "CFString could not be converted to UTF-8".into(),
        ));
    }
    // SAFETY: CFStringGetCString wrote a NUL-terminated UTF-8 string.
    let c_string = unsafe { CStr::from_ptr(buffer.as_ptr() as *const c_char) };
    Ok(c_string.to_string_lossy().into_owned())
}

const RTLD_NOW: c_int = 0x2;

#[link(name = "dl")]
unsafe extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *mut c_char;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_reports_availability_without_mutating_spotlight() {
        // This test only resolves symbols; it never calls them.
        match MetadataBackend::open() {
            Ok(backend) => assert!(backend.name().contains("Metadata.framework")),
            Err(MetadataError::Unavailable(reason)) => {
                // Non-macOS or a future macOS may legitimately lack the API.
                assert!(!reason.is_empty());
            }
            Err(error) => panic!("unexpected error: {error}"),
        }
    }
}
