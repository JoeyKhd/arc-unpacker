//! End-to-end tests: build FreeArc archives byte-for-byte (same layout as the format docs in
//! src/format.rs), then run the real arc-unpack binary on them.

use arc_unpack::format::encode_varint as varint;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MTIME: u32 = 1_600_000_000;

struct F {
    path: &'static str,
    data: Vec<u8>,
    is_dir: bool,
    bad_crc: bool,
}

fn file(path: &'static str, data: Vec<u8>) -> F {
    F { path, data, is_dir: false, bad_crc: false }
}
fn dir(path: &'static str) -> F {
    F { path, data: Vec::new(), is_dir: true, bad_crc: false }
}

struct B {
    method: &'static str,
    files: Vec<F>,
    /// Turns the block's concatenated file data into the stored payload.
    encode: fn(Vec<u8>) -> Vec<u8>,
}

fn xor(mut d: Vec<u8>) -> Vec<u8> {
    d.iter_mut().for_each(|b| *b ^= 0x5A);
    d
}

fn lzma_raw(d: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    let opts = lzma_rs::compress::Options { unpacked_size: lzma_rs::compress::UnpackedSize::SkipWritingToHeader };
    lzma_rs::lzma_compress_with_options(&mut &d[..], &mut out, &opts).unwrap();
    out.split_off(5) // drop props + dict size: FreeArc stores the raw stream
}

fn pseudo_random(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2654435761).max(1);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

fn cstr(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

fn descriptor(out: &mut Vec<u8>, kind: u64, method: &str, orig: usize, comp: usize, crc: u32, hash: fn(&[u8]) -> u32) {
    let start = out.len();
    out.extend_from_slice(b"ArC\x01");
    out.extend(varint(kind));
    cstr(out, method);
    out.extend(varint(orig as u64));
    out.extend(varint(comp as u64));
    out.extend(crc.to_le_bytes());
    let dcrc = hash(&out[start..]);
    out.extend(dcrc.to_le_bytes());
}

fn split_path(p: &str) -> (&str, &str) {
    p.rsplit_once('/').unwrap_or(("", p))
}

/// Builds a complete archive. `std_crc = false` stores bogus block CRCs, like a modified FreeArc
/// whose checksum arc-unpack doesn't know.
fn build(blocks: Vec<B>, std_crc: bool) -> Vec<u8> {
    build_with(blocks, crc32fast::hash, !std_crc)
}

/// Builds a complete archive whose checksums (files, blocks, descriptors) are `hash`; with
/// `bogus`, the block CRCs are wrong.
fn build_with(blocks: Vec<B>, hash: fn(&[u8]) -> u32, bogus: bool) -> Vec<u8> {
    let bogus = |c: u32| if bogus { !c } else { c };
    let mut out = Vec::new();
    let hdr = b"ArC\x01\x00\x00\x06\x07".to_vec();
    out.extend(&hdr);
    descriptor(&mut out, 1, "storing", 8, 8, bogus(hash(&hdr)), hash);

    let mut positions = Vec::new();
    let mut sizes = Vec::new();
    for b in &blocks {
        let raw: Vec<u8> = b.files.iter().flat_map(|f| f.data.clone()).collect();
        let payload = (b.encode)(raw);
        positions.push(out.len() as u64);
        sizes.push(payload.len() as u64);
        out.extend(payload);
    }

    let dir_pos = out.len() as u64;
    let mut d = Vec::new();
    d.extend(varint(blocks.len() as u64));
    for b in &blocks {
        d.extend(varint(b.files.len() as u64));
    }
    for b in &blocks {
        cstr(&mut d, b.method);
    }
    for p in &positions {
        d.extend(varint(dir_pos - p));
    }
    for s in &sizes {
        d.extend(varint(*s));
    }
    let all: Vec<&F> = blocks.iter().flat_map(|b| b.files.iter()).collect();
    let mut dirs: Vec<&str> = vec![""];
    for f in &all {
        let (dname, _) = split_path(f.path);
        if !dirs.contains(&dname) {
            dirs.push(dname);
        }
    }
    d.extend(varint(dirs.len() as u64));
    for dn in &dirs {
        cstr(&mut d, dn);
    }
    for f in &all {
        cstr(&mut d, split_path(f.path).1);
    }
    for f in &all {
        let idx = dirs.iter().position(|x| *x == split_path(f.path).0).unwrap();
        d.extend(varint(idx as u64));
    }
    for f in &all {
        d.extend(varint(f.data.len() as u64));
    }
    for _ in &all {
        d.extend(MTIME.to_le_bytes());
    }
    for f in &all {
        d.push(f.is_dir as u8);
    }
    for f in &all {
        let c = if f.is_dir { 0 } else { hash(&f.data) };
        d.extend((if f.bad_crc { c ^ 1 } else { c }).to_le_bytes());
    }
    d.extend(varint(0));
    let dcomp = lzma_raw(d.clone());
    out.extend(&dcomp);
    descriptor(&mut out, 3, "lzma:mfbt4:d8m", d.len(), dcomp.len(), bogus(hash(&d)), hash);

    let footer_pos = out.len() as u64;
    let mut ft = Vec::new();
    ft.extend(varint(2));
    ft.extend(varint(1));
    cstr(&mut ft, "storing");
    ft.extend(varint(footer_pos));
    ft.extend(varint(8));
    ft.extend(varint(8));
    ft.extend(bogus(hash(&hdr)).to_le_bytes());
    ft.extend(varint(3));
    cstr(&mut ft, "lzma:mfbt4:d8m");
    ft.extend(varint(footer_pos - dir_pos));
    ft.extend(varint(d.len() as u64));
    ft.extend(varint(dcomp.len() as u64));
    ft.extend(bogus(hash(&d)).to_le_bytes());
    ft.push(0); // locked
    ft.extend(varint(0)); // old comment
    cstr(&mut ft, ""); // recovery settings
    ft.extend(varint(0)); // comment
    out.extend(&ft);
    descriptor(&mut out, 4, "storing", ft.len(), ft.len(), bogus(hash(&ft)), hash);
    out
}

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        let env = Env { dir: tempfile::tempdir().unwrap() };
        let filter = env!("CARGO_BIN_EXE_testfilter").replace('\\', "/");
        let cfg = format!(
            r#"
codec_dir = "codecs"

[methods.xorpipe]
cmd = '"{filter}"'

[methods.xorfile]
cmd = '"{filter}" {{input}} {{output}}'
header = false

[methods.xorfail]
cmd = '"{filter}" --fail'
header = false

[methods.failnow]
cmd = '"{filter}" --fail-now'
header = false

[methods.hang]
cmd = '"{filter}" --hang'
header = false

[methods.dialog]
cmd = '"{filter}" --dialog'
header = false

[methods.lockcwd]
cmd = '"{filter}" --lockcwd'
header = false
"#
        );
        std::fs::create_dir_all(env.p("codecs")).unwrap();
        std::fs::write(env.p("codecs.toml"), cfg).unwrap();
        env
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn write_archive(&self, name: &str, blocks: Vec<B>, std_crc: bool) -> PathBuf {
        let path = self.p(name);
        std::fs::write(&path, build(blocks, std_crc)).unwrap();
        path
    }

    /// An archive with FitGirl's CRC-32 (the checksum of her modified unarc.dll) throughout.
    fn write_fitgirl_archive(&self, name: &str, blocks: Vec<B>) -> PathBuf {
        let path = self.p(name);
        std::fs::write(&path, build_with(blocks, arc_unpack::fgcrc::hash, false)).unwrap();
        path
    }

    fn run(&self, args: &[&str]) -> Output {
        let out = Command::new(env!("CARGO_BIN_EXE_arc-unpack"))
            .current_dir(self.dir.path())
            .args(args)
            .output()
            .unwrap();
        eprintln!(
            "--- arc-unpack {args:?} -> {:?}\nstdout:\n{}\nstderr:\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

fn assert_file(root: &Path, rel: &str, want: &[u8]) {
    let p = root.join(rel);
    let got = std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    assert!(got == want, "{rel}: content differs ({} vs {} bytes)", got.len(), want.len());
    let mtime = std::fs::metadata(&p).unwrap().modified().unwrap();
    assert_eq!(mtime.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(), MTIME as u64, "{rel}: mtime");
}

fn main_blocks() -> Vec<B> {
    vec![
        B { method: "storing", files: vec![dir("emptydir"), file("plain.txt", b"stored as-is\n".to_vec())], encode: |d| d },
        B {
            method: "lzma:mfbt4:d8m",
            files: vec![
                file("sub/a.txt", b"hello from lzma\n".repeat(1000)),
                file("sub/b.bin", pseudo_random(3 << 20, 1)),
                file("zero.dat", Vec::new()),
            ],
            encode: lzma_raw,
        },
        // header=1 stage, flag byte 1: run the codec on the rest.
        B { method: "xorpipe", files: vec![file("x/one.bin", pseudo_random(1_500_000, 2))], encode: |d| [vec![1], xor(d)].concat() },
        // header=1 stage, flag byte 0: payload stored raw, codec not run.
        B { method: "xorpipe", files: vec![file("x/raw.bin", pseudo_random(70_000, 3))], encode: |d| [vec![0], d].concat() },
        // Two-stage chain: compressed by lzma, then by xorfile (temp-file codec).
        B { method: "lzma:d8m+xorfile", files: vec![file("y/c.bin", b"chain ".repeat(200_000))], encode: |d| xor(lzma_raw(d)) },
    ]
}

fn check_main_output(out: &Path) {
    assert!(out.join("emptydir").is_dir());
    assert_file(out, "plain.txt", b"stored as-is\n");
    assert_file(out, "sub/a.txt", &b"hello from lzma\n".repeat(1000));
    assert_file(out, "sub/b.bin", &pseudo_random(3 << 20, 1));
    assert_file(out, "zero.dat", b"");
    assert_file(out, "x/one.bin", &pseudo_random(1_500_000, 2));
    assert_file(out, "x/raw.bin", &pseudo_random(70_000, 3));
    assert_file(out, "y/c.bin", &b"chain ".repeat(200_000));
}

#[test]
fn list_parses_synthetic_archive() {
    let env = Env::new();
    env.write_archive("a.arc", main_blocks(), true);
    let o = env.run(&["list", "a.arc", "--files"]);
    assert!(o.status.success());
    let t = text(&o);
    assert!(t.contains("7 files + 1 dirs"), "{t}");
    assert!(t.contains("standard CRC-32"));
    assert!(t.contains("lzma:d8m+xorfile"));
}

#[test]
fn extract_all_codec_kinds() {
    let env = Env::new();
    env.write_archive("a.arc", main_blocks(), true);
    let o = env.run(&["extract", "a.arc", "-o", "out", "-c", "codecs.toml", "-j", "3", "--writers", "4"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("7 files CRC OK"), "{}", text(&o));
    check_main_output(&env.p("out"));

    // Existing files are refused without --overwrite, accepted with it.
    let o = env.run(&["extract", "a.arc", "-o", "out", "-c", "codecs.toml"]);
    assert_eq!(o.status.code(), Some(2));
    let o = env.run(&["extract", "a.arc", "-o", "out", "-c", "codecs.toml", "--overwrite"]);
    assert!(o.status.success());
    check_main_output(&env.p("out"));
}

#[test]
fn test_mode_writes_nothing() {
    let env = Env::new();
    env.write_archive("a.arc", main_blocks(), true);
    let o = env.run(&["test", "a.arc", "-c", "codecs.toml"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("7 files CRC OK"));
    assert!(!env.p("out").exists());
}

#[test]
fn crc_mismatch_is_reported() {
    let env = Env::new();
    let mut f = file("bad.bin", pseudo_random(10_000, 9));
    f.bad_crc = true;
    env.write_archive("bad.arc", vec![B { method: "lzma:d8m", files: vec![file("good.bin", vec![7; 5000]), f], encode: lzma_raw }], true);
    let o = env.run(&["extract", "bad.arc", "-o", "out"]);
    assert_eq!(o.status.code(), Some(2));
    let t = text(&o);
    assert!(t.contains("bad.bin") && t.contains("CRC mismatch"), "{t}");
    assert!(t.contains("1 files CRC OK"), "{t}");
}

#[test]
fn nonstandard_checksums_are_detected_not_misreported() {
    let env = Env::new();
    env.write_archive("mod.arc", vec![B { method: "storing", files: vec![file("f.bin", vec![1, 2, 3])], encode: |d| d }], false);
    let o = env.run(&["list", "mod.arc"]);
    assert!(text(&o).contains("unknown checksums"));
    let o = env.run(&["extract", "mod.arc", "-o", "out"]);
    assert!(o.status.success(), "{}", text(&o));
    let t = text(&o);
    assert!(t.contains("neither standard nor FitGirl CRC-32") && t.contains("1 complete but not CRC-checked"), "{t}");
    assert_file(&env.p("out"), "f.bin", &[1, 2, 3]);
}

/// FitGirl's archives use another CRC-32 throughout: files are verified with it, and a damaged
/// file is caught.
#[test]
fn fitgirl_checksums_are_verified() {
    let env = Env::new();
    let mut bad = file("bad.bin", pseudo_random(10_000, 4));
    bad.bad_crc = true;
    env.write_fitgirl_archive("fg.arc", vec![B { method: "lzma:d8m", files: vec![file("a.bin", vec![5; 3000]), file("b.bin", pseudo_random(20_000, 3))], encode: lzma_raw }]);
    env.write_fitgirl_archive("fgbad.arc", vec![B { method: "lzma:d8m", files: vec![file("good.bin", vec![7; 5000]), bad], encode: lzma_raw }]);
    let o = env.run(&["list", "fg.arc"]);
    assert!(text(&o).contains("FitGirl CRC-32"), "{}", text(&o));
    let o = env.run(&["extract", "fg.arc", "-o", "out"]);
    assert!(o.status.success(), "{}", text(&o));
    let t = text(&o);
    assert!(!t.contains("not CRC-checked") && !t.contains("cannot be verified"), "{t}");
    assert_file(&env.p("out"), "b.bin", &pseudo_random(20_000, 3));
    let o = env.run(&["extract", "fgbad.arc", "-o", "outbad"]);
    assert_eq!(o.status.code(), Some(2));
    let t = text(&o);
    assert!(t.contains("bad.bin") && t.contains("CRC mismatch"), "{t}");
}

#[test]
fn path_traversal_is_refused() {
    let env = Env::new();
    env.write_archive(
        "evil.arc",
        vec![B { method: "storing", files: vec![file("../evil.txt", b"x".to_vec()), file("ok.txt", b"y".to_vec())], encode: |d| d }],
        true,
    );
    let o = env.run(&["extract", "evil.arc", "-o", "out"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(text(&o).contains("refusing path with '..'"), "{}", text(&o));
    assert!(!env.p("evil.txt").exists());
}

/// Codec processes running at once each get their own working folder, as under FreeArc: the
/// Razor stdio build keeps a fixed-name temp file there and crashes when two share one.
#[test]
fn concurrent_codecs_get_separate_working_folders() {
    let env = Env::new();
    let blocks = (0..4u32)
        .map(|i| B { method: "lockcwd", files: vec![file(["l/0.bin", "l/1.bin", "l/2.bin", "l/3.bin"][i as usize], pseudo_random(400_000, 10 + i))], encode: xor })
        .collect();
    env.write_archive("lock.arc", blocks, true);
    let o = env.run(&["extract", "lock.arc", "-o", "out", "-c", "codecs.toml", "-j", "4"]);
    assert!(o.status.success(), "{}", text(&o));
    for i in 0..4u32 {
        assert_file(&env.p("out"), &format!("l/{i}.bin"), &pseudo_random(400_000, 10 + i));
    }
    assert!(!env.p("codecs/testfilter.lock").exists());
}

#[test]
fn failing_codec_is_reported() {
    let env = Env::new();
    env.write_archive("fail.arc", vec![B { method: "xorfail", files: vec![file("f.bin", vec![0; 100_000])], encode: xor }], true);
    let o = env.run(&["extract", "fail.arc", "-o", "out", "-c", "codecs.toml"]);
    assert_eq!(o.status.code(), Some(2));
    let t = text(&o);
    assert!(t.contains("simulated codec failure") || t.contains("exit code: 3"), "{t}");
}

#[test]
fn missing_decoder_fails_fast_and_skip_missing_works() {
    let env = Env::new();
    env.write_archive(
        "m.arc",
        vec![
            B { method: "storing", files: vec![file("have.txt", b"ok".to_vec())], encode: |d| d },
            B { method: "magic9:x", files: vec![file("nope.bin", vec![1; 10])], encode: |d| d },
        ],
        true,
    );
    let o = env.run(&["extract", "m.arc", "-o", "out"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o).contains("magic9:x: no decoder"), "{}", text(&o));
    assert!(!env.p("out/have.txt").exists());

    let o = env.run(&["extract", "m.arc", "-o", "out", "--skip-missing"]);
    assert!(o.status.success(), "{}", text(&o));
    assert_file(&env.p("out"), "have.txt", b"ok");

    let o = env.run(&["check", "m.arc"]);
    assert_eq!(o.status.code(), Some(1));
}

/// Builds testdata/cls-xortest for `target` and returns the DLL path, or None if it can't.
fn build_cls_dll(target: &str) -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/cls-xortest");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let ok = Command::new(cargo)
        .args(["build", "--release", "--target", target, "--manifest-path"])
        .arg(root.join("Cargo.toml"))
        .status()
        .ok()?
        .success();
    let dll = root.join("target").join(target).join("release/cls_xortest.dll");
    (ok && dll.is_file()).then_some(dll)
}

fn cls_roundtrip(target: &str, host: &Path, host_name: &str) {
    let Some(dll) = build_cls_dll(target) else {
        eprintln!("SKIP: couldn't build the test CLS dll for {target}");
        return;
    };
    let env = Env::new();
    std::fs::copy(&dll, env.p("codecs/cls-xortest.dll")).unwrap();
    let hosts = env.p("hosts");
    std::fs::create_dir_all(&hosts).unwrap();
    std::fs::copy(host, hosts.join(host_name)).unwrap();

    let data = pseudo_random(2_000_000, 5);
    env.write_archive(
        "c.arc",
        vec![
            // "xortest:k51" -> the DLL reads "k51" through CLS_GET_PARAMSTR.
            B { method: "xortest:k51", files: vec![file("cls/a.bin", data.clone()), file("cls/b.txt", b"tail".to_vec())], encode: |d| {
                d.into_iter().map(|b| b ^ 0x33).collect()
            } },
            B { method: "xortest:fail", files: vec![file("cls/never.bin", vec![1; 10])], encode: |d| d },
        ],
        true,
    );
    let out = Command::new(env!("CARGO_BIN_EXE_arc-unpack"))
        .current_dir(env.dir.path())
        .env("ARC_UNPACK_HOST_DIR", &hosts)
        .args(["extract", "c.arc", "-o", "out", "-c", "codecs.toml"])
        .output()
        .unwrap();
    let t = text(&out);
    eprintln!("{t}");
    assert_eq!(out.status.code(), Some(2), "the 'fail' block must be reported: {t}");
    assert!(t.contains("ClsMain returned -7 (bad compressed data)"), "{t}");
    assert!(t.contains("2 files CRC OK"), "{t}");
    assert_file(&env.p("out"), "cls/a.bin", &data);
    assert_file(&env.p("out"), "cls/b.txt", b"tail");
}

#[test]
fn cls_dll_x64() {
    cls_roundtrip("x86_64-pc-windows-msvc", Path::new(env!("CARGO_BIN_EXE_cls-host")), "cls-host-x64.exe");
}

#[test]
fn cls_dll_x86() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i686-pc-windows-msvc");
    let Some(host) = ["release", "debug"].iter().map(|p| root.join(p).join("cls-host.exe")).find(|p| p.is_file()) else {
        eprintln!("SKIP: no 32-bit cls-host built (run build.ps1 first)");
        return;
    };
    cls_roundtrip("i686-pc-windows-msvc", &host, "cls-host-x86.exe");
}

/// A stage that dies without reading while the stage before it is still producing must fail
/// the block promptly (it used to leave the upstream codec blocked on a full pipe forever).
#[test]
fn dead_downstream_stage_does_not_hang() {
    let env = Env::new();
    // Decode order: xorpipe (big output) first, then failnow (exits without reading).
    env.write_archive(
        "dd.arc",
        vec![B { method: "failnow+xorpipe", files: vec![file("big.bin", pseudo_random(20 << 20, 7))], encode: |d| [vec![1], xor(d)].concat() }],
        true,
    );
    let t = std::time::Instant::now();
    let o = env.run(&["test", "dd.arc", "-c", "codecs.toml"]);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    assert!(t.elapsed().as_secs() < 60, "took {:?}", t.elapsed());
    assert!(text(&o).contains("failing before reading"), "{}", text(&o));
}

#[test]
fn hung_codec_is_stopped_by_watchdog() {
    let env = Env::new();
    env.write_archive("h.arc", vec![B { method: "hang", files: vec![file("f.bin", vec![1; 1000])], encode: |d| d }], true);
    let t = std::time::Instant::now();
    let o = env.run(&["test", "h.arc", "-c", "codecs.toml", "--stall-timeout", "3"]);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    assert!(t.elapsed().as_secs() < 30, "took {:?}", t.elapsed());
    assert!(text(&o).contains("no progress for 3 s"), "{}", text(&o));
}

/// Codecs that report errors with a MessageBox must not wait for a human to click OK.
#[cfg(windows)]
#[test]
fn codec_error_dialog_is_dismissed_and_reported() {
    let env = Env::new();
    env.write_archive("dlg.arc", vec![B { method: "dialog", files: vec![file("f.bin", vec![1; 1000])], encode: |d| d }], true);
    let t = std::time::Instant::now();
    let o = env.run(&["test", "dlg.arc", "-c", "codecs.toml", "--stall-timeout", "60"]);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    assert!(t.elapsed().as_secs() < 20, "dialog was not dismissed: took {:?}", t.elapsed());
    assert!(text(&o).contains("codec error dialog") && text(&o).contains("Can't create read mapping!"), "{}", text(&o));
}

/// The GUI's Cancel button: setting the flag must stop a running block promptly (here, a codec
/// that would otherwise hang forever) and report the run as cancelled.
#[test]
fn cancel_stops_a_running_block() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let env = Env::new();
    let path = env.write_archive("c.arc", vec![B { method: "hang", files: vec![file("f.bin", vec![1; 1000])], encode: |d| d }], true);
    let cfg = arc_unpack::config::Config::load(&env.p("codecs.toml")).unwrap();
    let archive = arc_unpack::format::Archive::open(&path).unwrap();
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let opts = arc_unpack::extract::Options {
        out_dir: None,
        jobs: 1,
        writers: 1,
        overwrite: false,
        skip_missing: false,
        tune: false,
        temp_dir: std::env::temp_dir(),
        host_dir: Path::new(env!("CARGO_BIN_EXE_arc-unpack")).parent().unwrap().to_path_buf(),
        stall_timeout: 0,
        cancel: cancel.clone(),
        heavy_jobs: 1,
        done_dir: None,
        md5: None,
        md5_prefix: String::new(),
        first_job_started: None,
    };
    let c2 = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(1));
        c2.store(true, Ordering::Relaxed);
    });
    let t = std::time::Instant::now();
    let s = arc_unpack::extract::run(vec![archive], &cfg, &opts, &arc_unpack::events::null_sink()).unwrap();
    assert!(s.cancelled);
    assert!(t.elapsed().as_secs() < 10, "took {:?}", t.elapsed());
    assert!(s.problems.iter().any(|p| p.contains("cancelled")), "{:?}", s.problems);
}

/// Resume: with a done_dir, finished jobs leave markers and a second run skips them.
#[test]
fn resume_skips_finished_jobs() {
    let env = Env::new();
    let path = env.write_archive("r.arc", main_blocks(), true);
    let cfg = arc_unpack::config::Config::load(&env.p("codecs.toml")).unwrap();
    let opts = |overwrite| arc_unpack::extract::Options {
        out_dir: Some(env.p("out")),
        jobs: 2,
        writers: 2,
        overwrite,
        skip_missing: false,
        tune: false,
        temp_dir: std::env::temp_dir(),
        host_dir: Path::new(env!("CARGO_BIN_EXE_arc-unpack")).parent().unwrap().to_path_buf(),
        stall_timeout: 0,
        cancel: Default::default(),
        heavy_jobs: 1,
        done_dir: Some(env.p("progress")),
        md5: None,
        md5_prefix: String::new(),
        first_job_started: None,
    };
    let sink = arc_unpack::events::null_sink();
    let s1 = arc_unpack::extract::run(vec![arc_unpack::format::Archive::open(&path).unwrap()], &cfg, &opts(false), &sink).unwrap();
    assert!(s1.problems.is_empty(), "{:?}", s1.problems);
    assert_eq!(s1.skipped, 0);
    let markers = std::fs::read_dir(env.p("progress")).unwrap().count();
    assert_eq!(markers, 5, "one marker per data block");
    check_main_output(&env.p("out"));
    // Second run: everything is skipped (no "file exists" errors even without overwrite).
    let s2 = arc_unpack::extract::run(vec![arc_unpack::format::Archive::open(&path).unwrap()], &cfg, &opts(false), &sink).unwrap();
    assert!(s2.problems.is_empty(), "{:?}", s2.problems);
    assert_eq!(s2.skipped, 5);
}
