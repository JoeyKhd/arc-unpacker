//! Test helper used as an "external codec" by the integration tests (not needed at runtime).
//!   testfilter                 XOR 0x5A, stdin -> stdout
//!   testfilter <in> <out>      XOR 0x5A, file -> file
//!   testfilter --fail          consume stdin, then exit with code 3
//!   testfilter --fail-now      exit with code 3 immediately, without reading anything
//!   testfilter --hang          consume stdin, then hang forever
//!   testfilter --dialog        show a modal error MessageBox (like some real codecs), then exit 4
//!   testfilter --lockcwd       XOR stdin -> stdout while holding a fixed-name file in the working
//!                              folder; exit 5 if it's already there (like the Razor stdio build)
use std::io::{Read, Write};

fn xor(buf: &mut [u8]) {
    for b in buf {
        *b ^= 0x5A;
    }
}

fn read_all() -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = std::io::stdin().lock().read_to_end(&mut buf);
    buf
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => {
            let mut buf = read_all();
            xor(&mut buf);
            std::io::stdout().lock().write_all(&buf).unwrap();
        }
        ["--fail"] => {
            read_all();
            eprintln!("testfilter: simulated codec failure");
            std::process::exit(3);
        }
        ["--fail-now"] => {
            eprintln!("testfilter: failing before reading");
            std::process::exit(3);
        }
        ["--lockcwd"] => {
            let lock = std::path::Path::new("testfilter.lock");
            if std::fs::OpenOptions::new().write(true).create_new(true).open(lock).is_err() {
                eprintln!("testfilter: another process is using this working folder");
                std::process::exit(5);
            }
            let mut buf = read_all();
            std::thread::sleep(std::time::Duration::from_millis(300));
            xor(&mut buf);
            std::io::stdout().lock().write_all(&buf).unwrap();
            let _ = std::fs::remove_file(lock);
        }
        ["--hang"] => {
            read_all();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
        ["--dialog"] => {
            #[cfg(windows)]
            {
                use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MessageBoxW};
                let w = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
                let (text, title) = (w("Can't create read mapping!"), w("testfilter"));
                unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_ICONERROR) };
            }
            std::process::exit(4);
        }
        [input, output] => {
            let mut buf = std::fs::read(input).unwrap();
            xor(&mut buf);
            std::fs::write(output, buf).unwrap();
        }
        _ => std::process::exit(64),
    }
}
