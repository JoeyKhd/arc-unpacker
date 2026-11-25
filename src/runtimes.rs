//! The runtimes a repack's game needs (DirectX, Visual C++, .NET, PhysX), which of them this PC
//! is missing, and installing those.
//!
//! FitGirl's installers don't include most of them: for each runtime component the setup wizard
//! downloads the official installer (Inno Download Plugin, `IdpAddFileComp(url, file,
//! component)`), and the Finish page offers to run it; DirectX's web installer is bundled. Here
//! the same files are used: the bundled ones from setup.exe, the others downloaded from the
//! links the installer registers. Each is only run if it carries a valid signature from
//! Microsoft (or NVIDIA, for PhysX), silently.

use crate::events::{self, Event, Sink};
use crate::inno::Setup;
use crate::pascal::{Proc, Script};
use anyhow::{Context, Result, bail, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Visual C++ redistributable of a year, 64-bit or 32-bit.
    Vc { year: u16, x64: bool },
    /// The DirectX June 2010 legacy components (d3dx9, xinput1_3, ...).
    DirectX,
    DotNet35,
    /// .NET Framework 4.x, at least this "Release" value (0 = any 4.x).
    DotNet4 { release: u32 },
    /// PhysX legacy system software.
    PhysX,
}

fn kind_of(component: &str) -> Option<Kind> {
    let c = component.to_ascii_lowercase();
    Some(match c.as_str() {
        "directx" => Kind::DirectX,
        "dotnet35" | "dotnet35cp" => Kind::DotNet35,
        "dotnet4full" => Kind::DotNet4 { release: 0 },
        "dotnet451" => Kind::DotNet4 { release: 378_675 },
        "dotnet471" => Kind::DotNet4 { release: 461_308 },
        "dotnet48" => Kind::DotNet4 { release: 528_040 },
        "physx" => Kind::PhysX,
        _ => {
            let year = c.strip_prefix("vc_")?.get(..4)?.parse::<u16>().ok()?;
            let x64 = c.ends_with("x64");
            ensure_some(x64 || c.ends_with("x86"))?;
            Kind::Vc { year, x64 }
        }
    })
}

fn ensure_some(ok: bool) -> Option<()> {
    ok.then_some(())
}

fn label(kind: Kind) -> String {
    match kind {
        Kind::Vc { year, x64 } => {
            let years = if year >= 2015 { "2015-2022".to_string() } else { year.to_string() };
            format!("Visual C++ {years} {}", if x64 { "x64" } else { "x86" })
        }
        Kind::DirectX => "DirectX".into(),
        Kind::DotNet35 => ".NET Framework 3.5".into(),
        Kind::DotNet4 { release } => match release {
            0 => ".NET Framework 4".into(),
            r if r >= 528_040 => ".NET Framework 4.8".into(),
            r if r >= 461_308 => ".NET Framework 4.7.1".into(),
            _ => ".NET Framework 4.5.1".into(),
        },
        Kind::PhysX => "PhysX".into(),
    }
}

/// Command-line switches for an unattended install of this kind of installer.
fn silent_args(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::Vc { year, .. } if year >= 2012 => &["/install", "/quiet", "/norestart"],
        Kind::Vc { year: 2010, .. } => &["/q", "/norestart"],
        Kind::Vc { .. } => &["/q"],
        Kind::DirectX => &["/Q"],
        Kind::DotNet35 | Kind::DotNet4 { .. } => &["/q", "/norestart"],
        Kind::PhysX => &["/qn", "/norestart"],
    }
}

/// Where the installer for a runtime comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    /// Inside setup.exe (index into `Setup::files`).
    Bundled(usize),
    /// Downloaded from the link the installer's script registers.
    Download { url: String, file: String },
    /// Neither (the installer names the runtime but has no way to get it).
    None,
}

/// A runtime the repack's game needs.
#[derive(Debug, Clone)]
pub struct Runtime {
    /// The installer's component (e.g. `vc_2022_x64`).
    pub component: String,
    /// In plain words ("Visual C++ 2015-2022 x64").
    pub label: String,
    /// Whether this PC already has it.
    pub installed: bool,
    /// Whether arc-unpack can get its installer (bundled, or a download link).
    pub available: bool,
    kind: Kind,
    source: Source,
}

/// The runtimes a default install of this repack includes, and whether this PC has them.
pub fn of_repack(setup: &Setup, script: Option<&Script>, repack_files: &[String]) -> Vec<Runtime> {
    let downloads = script.map(registered_downloads).unwrap_or_default();
    let wanted = crate::setup_script::default_components(setup, repack_files);
    let mut out = Vec::new();
    for c in setup.components.iter().filter(|c| crate::install::english_or_neutral(&c.languages)) {
        let name = c.name.to_ascii_lowercase();
        let Some(kind) = kind_of(&name) else { continue };
        if !wanted.contains(&name) || out.iter().any(|r: &Runtime| r.component == name) {
            continue;
        }
        let file_name = |f: &crate::inno::SetupFile| f.dest.rsplit(['\\', '/']).next().unwrap_or("").to_ascii_lowercase();
        let bundled = setup.files.iter().position(|f| {
            let n = file_name(f);
            let for_this = f.components.split(|ch: char| !ch.is_alphanumeric() && ch != '_').any(|w| w.eq_ignore_ascii_case(&name))
                // DirectX's web installer usually comes with every install, without a component.
                || (kind == Kind::DirectX && n == "dxwebsetup.exe");
            f.has_data() && crate::install::english_or_neutral(&f.languages) && for_this && (n.ends_with(".exe") || n.ends_with(".msi"))
        });
        let source = match (bundled, downloads.get(&name)) {
            (Some(i), _) => Source::Bundled(i),
            (None, Some((url, file))) => Source::Download { url: url.clone(), file: file.clone() },
            (None, None) => Source::None,
        };
        out.push(Runtime { component: name, label: label(kind), installed: installed(kind), available: source != Source::None, kind, source });
    }
    out
}

/// The runtimes of a repack this PC is missing and arc-unpack can install.
pub fn missing(list: &[Runtime]) -> Vec<&Runtime> {
    list.iter().filter(|r| (!r.installed || forced(&r.component)) && r.available).collect()
}

/// Testing: `ARC_UNPACK_RUNTIMES_FORCE=vc_2022_x64,...` treats these components as missing.
fn forced(component: &str) -> bool {
    std::env::var("ARC_UNPACK_RUNTIMES_FORCE").is_ok_and(|v| v.split(',').any(|c| c.trim().eq_ignore_ascii_case(component)))
}

/// `IdpAddFileComp(url, file, component)` calls in the script: component -> (url, file name).
fn registered_downloads(script: &Script) -> BTreeMap<String, (String, String)> {
    let mut out = BTreeMap::new();
    for (i, p) in script.procs.iter().enumerate() {
        if !matches!(p, Proc::Internal { .. }) {
            continue;
        }
        let Ok(lines) = crate::pascal::disassemble(script, i as u32) else { continue };
        let mut recent: Vec<String> = Vec::new();
        for line in lines {
            if let Some(s) = string_constant(&line) {
                recent.push(s);
                if recent.len() > 6 {
                    recent.remove(0);
                }
            }
            if line.contains("IDPADDFILECOMP") && recent.len() >= 3 {
                let n = recent.len();
                let (component, dest, url) = (&recent[n - 3], &recent[n - 2], &recent[n - 1]);
                let lower = url.to_ascii_lowercase();
                if lower.starts_with("https://") || lower.starts_with("http://") {
                    let file = dest.rsplit(['\\', '/']).next().unwrap_or(dest).to_string();
                    out.entry(component.to_ascii_lowercase()).or_insert((url.clone(), file));
                }
                recent.clear();
            }
        }
    }
    out
}

/// The string in a disassembled `X := Str("...")` (Rust debug escaping).
fn string_constant(line: &str) -> Option<String> {
    let start = line.find(":= Str(\"")? + 8;
    let body = line.get(start..)?.strip_suffix("\")")?;
    let mut out = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            '0' => out.push('\0'),
            'u' => {
                let hex: String = chars.by_ref().skip(1).take_while(|&c| c != '}').collect();
                out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
            }
            other => out.push(other),
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------------- detection

fn installed(kind: Kind) -> bool {
    match kind {
        Kind::Vc { year, x64 } => {
            let arch = if x64 { "x64" } else { "x86" };
            match year {
                2015..=2022 => {
                    let key = format!(r"SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\{arch}");
                    let min_minor = match year {
                        2015 | 2016 => 0,
                        2017 | 2018 => 10,
                        2019..=2021 => 20,
                        _ => 30,
                    };
                    reg_dword(&key, "Installed", !x64) == Some(1) && reg_dword(&key, "Minor", !x64).is_some_and(|m| m >= min_minor)
                }
                2013 => reg_dword(&format!(r"SOFTWARE\Microsoft\VisualStudio\12.0\VC\Runtimes\{arch}"), "Installed", !x64) == Some(1),
                2012 => reg_dword(&format!(r"SOFTWARE\Microsoft\VisualStudio\11.0\VC\Runtimes\{arch}"), "Installed", !x64) == Some(1),
                2010 => reg_dword(&format!(r"SOFTWARE\Microsoft\VisualStudio\10.0\VC\VCRedist\{arch}"), "Installed", !x64) == Some(1),
                2008 => in_winsxs(&format!("{}_microsoft.vc90.crt_", if x64 { "amd64" } else { "x86" })),
                2005 => in_winsxs(&format!("{}_microsoft.vc80.crt_", if x64 { "amd64" } else { "x86" })),
                _ => false,
            }
        }
        Kind::DirectX => {
            const DLLS: [&str; 5] = ["d3dx9_43.dll", "d3dx11_43.dll", "d3dcompiler_43.dll", "xinput1_3.dll", "xaudio2_7.dll"];
            ["System32", "SysWOW64"].iter().all(|d| DLLS.iter().all(|f| system_root().join(d).join(f).is_file()))
        }
        Kind::DotNet35 => reg_dword(r"SOFTWARE\Microsoft\NET Framework Setup\NDP\v3.5", "Install", false) == Some(1),
        Kind::DotNet4 { release } => {
            let key = r"SOFTWARE\Microsoft\NET Framework Setup\NDP\v4\Full";
            reg_dword(key, "Install", false) == Some(1) && (release == 0 || reg_dword(key, "Release", false).is_some_and(|r| r >= release))
        }
        Kind::PhysX => ["System32", "SysWOW64"].iter().any(|d| system_root().join(d).join("PhysXLoader.dll").is_file()),
    }
}

fn system_root() -> PathBuf {
    std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
}

fn in_winsxs(prefix: &str) -> bool {
    std::fs::read_dir(system_root().join("WinSxS"))
        .map(|d| d.flatten().any(|e| e.file_name().to_string_lossy().to_ascii_lowercase().starts_with(prefix)))
        .unwrap_or(false)
}

/// A DWORD under HKEY_LOCAL_MACHINE, from the 64-bit registry view or the 32-bit one.
#[cfg(windows)]
fn reg_dword(subkey: &str, value: &str, view32: bool) -> Option<u32> {
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_SUBKEY_WOW6432KEY, RRF_SUBKEY_WOW6464KEY, RegGetValueW};
    let w = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (k, v) = (w(subkey), w(value));
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let view = if view32 { RRF_SUBKEY_WOW6432KEY } else { RRF_SUBKEY_WOW6464KEY };
    let rc = unsafe {
        RegGetValueW(HKEY_LOCAL_MACHINE, k.as_ptr(), v.as_ptr(), RRF_RT_REG_DWORD | view, std::ptr::null_mut(), (&mut data as *mut u32).cast(), &mut size)
    };
    (rc == 0).then_some(data)
}

#[cfg(not(windows))]
fn reg_dword(_: &str, _: &str, _: bool) -> Option<u32> {
    None
}

// ---------------------------------------------------------------------------------- installing

/// Gets and runs the installers of these runtimes (each only if this PC still lacks it), one
/// after another. Returns what happened, one line per runtime, and the problems.
pub fn install(setup: &Setup, runtimes: &[&Runtime], work: &Path, sink: &Sink) -> (Vec<String>, Vec<String>) {
    let (mut done, mut problems) = (Vec::new(), Vec::new());
    let total = runtimes.len();
    for (i, r) in runtimes.iter().enumerate() {
        sink(Event::Step { index: i + 1, total, label: format!("Installing {}", r.label) });
        if installed(r.kind) && !forced(&r.component) {
            done.push(format!("{}: already installed", r.label));
            continue;
        }
        match install_one(setup, r, work, sink) {
            Ok(note) => done.push(format!("{}: {note}", r.label)),
            Err(e) => problems.push(format!("{}: {e:#}", r.label)),
        }
    }
    (done, problems)
}

fn install_one(setup: &Setup, r: &Runtime, work: &Path, sink: &Sink) -> Result<String> {
    std::fs::create_dir_all(work)?;
    let file = match &r.source {
        Source::Bundled(i) => {
            let f = &setup.files[*i];
            let name = f.dest.rsplit(['\\', '/']).next().unwrap_or("setup.exe");
            let path = work.join(format!("{}-{name}", r.component));
            std::fs::write(&path, setup.read(f)?).with_context(|| format!("write {}", path.display()))?;
            path
        }
        Source::Download { url, file } => {
            let path = work.join(format!("{}-{file}", r.component));
            events::info(sink, format!("downloading {} from {url}", r.label));
            download(url, &path)?;
            path
        }
        Source::None => bail!("the installer has no download link for it"),
    };
    let signer = signer(&file)?;
    let wanted = if r.kind == Kind::PhysX { "NVIDIA Corporation" } else { "Microsoft Corporation" };
    ensure!(signer.contains(&format!("O={wanted}")), "not installed: {} isn't signed by {wanted} (signer: {signer})", file.display());
    let mut cmd = if r.kind == Kind::PhysX {
        let mut c = std::process::Command::new(system_root().join("System32").join("msiexec.exe"));
        c.arg("/i").arg(&file);
        c
    } else {
        std::process::Command::new(&file)
    };
    cmd.args(silent_args(r.kind));
    events::info(sink, format!("installing {} ({})", r.label, file.display()));
    let status = cmd.status().with_context(|| format!("start {}", file.display()))?;
    let _ = std::fs::remove_file(&file);
    match status.code() {
        Some(0) => Ok("installed".into()),
        Some(3010) | Some(1641) => Ok("installed (Windows needs a restart to finish)".into()),
        Some(1638) => Ok("a newer version is already installed".into()),
        Some(c) => bail!("its installer failed (exit code {c})"),
        None => bail!("its installer was stopped"),
    }
}

/// Downloads with Windows' own curl.exe (Windows 10 1803 and later).
fn download(url: &str, to: &Path) -> Result<()> {
    let curl = system_root().join("System32").join("curl.exe");
    ensure!(curl.is_file(), "Windows' curl.exe isn't available, so it can't be downloaded");
    let out = std::process::Command::new(&curl)
        .args(["--location", "--fail", "--silent", "--show-error", "--retry", "2", "--connect-timeout", "30", "--output"])
        .arg(to)
        .arg(url)
        .output()
        .context("start curl.exe")?;
    ensure!(out.status.success(), "download failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    ensure!(std::fs::metadata(to).map(|m| m.len()).unwrap_or(0) > 0, "download failed: empty file");
    Ok(())
}

/// The subject of a file's Authenticode signer certificate, if the signature is valid.
fn signer(path: &Path) -> Result<String> {
    let script = "$s = Get-AuthenticodeSignature -LiteralPath $env:ARC_UNPACK_SIGNED_FILE; \
                  if ($s.Status -eq 'Valid') { $s.SignerCertificate.Subject } else { 'INVALID ' + $s.Status }";
    let out = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("ARC_UNPACK_SIGNED_FILE", path)
        .output()
        .context("start powershell.exe to check the signature")?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    ensure!(!text.is_empty() && !text.starts_with("INVALID"), "not installed: {} has no valid signature ({text})", path.display());
    Ok(text)
}

/// The runtime components a list names (for messages).
pub fn labels(list: &[&Runtime]) -> String {
    let set: BTreeSet<&str> = list.iter().map(|r| r.label.as_str()).collect();
    set.into_iter().collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_components() {
        assert_eq!(kind_of("vc_2022_x64"), Some(Kind::Vc { year: 2022, x64: true }));
        assert_eq!(kind_of("VC_2010_SP1_X86"), Some(Kind::Vc { year: 2010, x64: false }));
        assert_eq!(kind_of("directx"), Some(Kind::DirectX));
        assert_eq!(kind_of("dotnet48"), Some(Kind::DotNet4 { release: 528_040 }));
        assert_eq!(kind_of("text"), None);
        assert_eq!(kind_of("vc_2022"), None, "no architecture");
        assert_eq!(label(Kind::Vc { year: 2019, x64: true }), "Visual C++ 2015-2022 x64");
        assert_eq!(label(Kind::Vc { year: 2010, x64: false }), "Visual C++ 2010 x86");
        assert_eq!(silent_args(Kind::Vc { year: 2022, x64: true }), &["/install", "/quiet", "/norestart"]);
    }

    #[test]
    fn disassembled_strings() {
        assert_eq!(string_constant(r#"  3705  S4 := Str("http://x/y.msi")"#).as_deref(), Some("http://x/y.msi"));
        assert_eq!(string_constant(r#"  3632  S4 := Str("{tmp}\\PhysX.msi")"#).as_deref(), Some(r"{tmp}\PhysX.msi"));
        assert_eq!(string_constant(r#"  12  S1 := Str("a\"b\u{e9}")"#).as_deref(), Some("a\"b\u{e9}"));
        assert_eq!(string_constant("  12  S1 := Int(3)"), None);
    }
}
