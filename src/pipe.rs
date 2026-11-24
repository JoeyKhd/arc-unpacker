//! In-process pipe made of large chunks over a bounded channel. Used between built-in stages,
//! so data moves in megabyte-sized pieces instead of tiny OS pipe writes.
//!
//! The writer must call `finish()` for a clean end of stream. If it is dropped without that
//! (its stage failed or panicked), the reader gets an error instead of a silent short stream.

use crossbeam_channel::{Receiver, Sender, bounded};
use std::io::{self, Read, Write};

pub const CHUNK: usize = 1 << 20;

enum Msg {
    Data(Vec<u8>),
    End,
}

pub struct PipeWriter {
    tx: Sender<Msg>,
    buf: Vec<u8>,
    finished: bool,
}

pub struct PipeReader {
    rx: Receiver<Msg>,
    cur: Vec<u8>,
    off: usize,
    done: bool,
}

pub fn pipe(depth: usize) -> (PipeWriter, PipeReader) {
    let (tx, rx) = bounded(depth.max(1));
    (
        PipeWriter { tx, buf: Vec::with_capacity(CHUNK), finished: false },
        PipeReader { rx, cur: Vec::new(), off: 0, done: false },
    )
}

fn broken() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "downstream stage stopped reading")
}

impl PipeWriter {
    fn send_buf(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            let full = std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK));
            self.tx.send(Msg::Data(full)).map_err(|_| broken())?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<()> {
        self.send_buf()?;
        self.finished = true;
        self.tx.send(Msg::End).map_err(|_| broken())
    }
}

impl Write for PipeWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let n = data.len().min(CHUNK - self.buf.len());
        self.buf.extend_from_slice(&data[..n]);
        if self.buf.len() == CHUNK {
            self.send_buf()?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send_buf()
    }
}

impl Read for PipeReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.off == self.cur.len() {
            if self.done {
                return Ok(0);
            }
            match self.rx.recv() {
                Ok(Msg::Data(d)) => {
                    self.cur = d;
                    self.off = 0;
                }
                Ok(Msg::End) => self.done = true,
                Err(_) => {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "upstream stage ended without finishing"));
                }
            }
        }
        let n = out.len().min(self.cur.len() - self.off);
        out[..n].copy_from_slice(&self.cur[self.off..self.off + n]);
        self.off += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_failure() {
        let (mut w, mut r) = pipe(2);
        let t = std::thread::spawn(move || {
            let data: Vec<u8> = (0..3 * CHUNK + 17).map(|i| i as u8).collect();
            w.write_all(&data).unwrap();
            w.finish().unwrap();
        });
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        t.join().unwrap();
        assert_eq!(got.len(), 3 * CHUNK + 17);
        assert!(got.iter().enumerate().all(|(i, &b)| b == i as u8));

        let (mut w, mut r) = pipe(2);
        w.write_all(b"partial").unwrap();
        w.flush().unwrap();
        drop(w); // no finish(): simulated stage failure
        let mut got = Vec::new();
        assert!(r.read_to_end(&mut got).is_err());
    }
}
