use std::ffi::c_void;

type Callback = unsafe extern "C" fn(*mut c_void, i32, *mut c_void, i32) -> i32;

const CLS_INIT: i32 = 1;
const CLS_DONE: i32 = 2;
const CLS_DECOMPRESS: i32 = 4;
const CLS_MALLOC: i32 = 1;
const CLS_FREE: i32 = 2;
const CLS_GET_PARAMSTR: i32 = 3;
const CLS_PARTIAL_READ: i32 = 5120;
const CLS_FULL_WRITE: i32 = 6144;
const CLS_ERROR_NOT_IMPLEMENTED: i32 = -2;
const CLS_ERROR_GENERAL: i32 = -1;

/// Exercises the same callbacks real codecs use: params, malloc/free, partial read, full write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ClsMain(op: i32, cb: Option<Callback>, instance: *mut c_void) -> i32 {
    match op {
        CLS_INIT | CLS_DONE => 0,
        CLS_DECOMPRESS => unsafe {
            let Some(cb) = cb else { return CLS_ERROR_GENERAL };
            let mut params = [0u8; 256];
            if cb(instance, CLS_GET_PARAMSTR, params.as_mut_ptr().cast(), params.len() as i32) != 0 {
                return CLS_ERROR_GENERAL;
            }
            let len = params.iter().position(|&b| b == 0).unwrap_or(0);
            let p = std::str::from_utf8(&params[..len]).unwrap_or("");
            if p == "fail" {
                return -7; // bad compressed data
            }
            let key = p.strip_prefix('k').and_then(|k| k.parse::<u8>().ok()).unwrap_or(0x5A);

            const N: i32 = 65536;
            let mut buf: *mut u8 = std::ptr::null_mut();
            if cb(instance, CLS_MALLOC, (&mut buf as *mut *mut u8).cast(), N) != 0 || buf.is_null() {
                return -3;
            }
            let result = loop {
                let n = cb(instance, CLS_PARTIAL_READ, buf.cast(), N);
                if n < 0 {
                    break n;
                }
                if n == 0 {
                    break 0;
                }
                for b in std::slice::from_raw_parts_mut(buf, n as usize) {
                    *b ^= key;
                }
                let w = cb(instance, CLS_FULL_WRITE, buf.cast(), n);
                if w < 0 {
                    break w;
                }
            };
            cb(instance, CLS_FREE, buf.cast(), 0);
            result
        },
        _ => CLS_ERROR_NOT_IMPLEMENTED,
    }
}
