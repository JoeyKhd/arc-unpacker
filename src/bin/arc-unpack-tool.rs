//! Stand-in for a repack tool arc-unpack knows exactly (see `arc_unpack::tools`). It takes the
//! tool's place in arc-unpack's temporary folder; the install script's commands run it with the
//! tool's command line. It does the work itself for the command lines it understands and runs
//! the original tool (moved aside) for anything else, or when anything goes wrong.
//!
//! ARC_UNPACK_TOOL_VERIFY=1: also runs the original, into a temporary name, and compares; the
//! original's output is kept, and the result goes to `<list>.verify.log`.

use arc_unpack::tools::{self, Entry, Role};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let me = std::env::current_exe().unwrap_or_default();
    let Some(map) = find_map(&me) else {
        eprintln!("arc-unpack-tool: no list of stand-ins ({} not set)", tools::MAP_ENV);
        return 1;
    };
    let Some(entry) = tools::lookup(&map, &me) else {
        eprintln!("arc-unpack-tool: {} is not in {}", me.display(), map.display());
        return 1;
    };
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let Some(mut job) = understood(entry.role, &args, &map) else {
        return run_original(&entry, None);
    };
    job.original = entry.original.clone();
    // (A whole folder can't be redirected for a side-by-side check: the install's own file check
    // covers FreeArc extractions.)
    let verify = std::env::var_os(tools::VERIFY_ENV).is_some_and(|v| v == "1") && job.output_arg != usize::MAX;
    if verify {
        return run_verified(&entry, &map, &job);
    }
    let started = Instant::now();
    if job.role == Role::X5nDirPatch {
        // The original does the patch, without re-reading unchanged files; its exit code is ours.
        return match std::process::Command::new(&job.original).args(&job.args).status() {
            Ok(st) => {
                let note = format!("{} native {:.1} s {} (with -C-new: unchanged files not re-read)", entry.role.name(), started.elapsed().as_secs_f64(), job.output.display());
                log(&map, "log", &note);
                st.code().unwrap_or(1)
            }
            Err(e) => {
                log(&map, "log", &format!("{} failed to start ({e}), running the original: {}", entry.role.name(), job.output.display()));
                run_original(&entry, None)
            }
        };
    }
    // An x2n map that doesn't check out stays the original's, before anything is touched.
    let plan = match job.role {
        Role::X2n => match tools::x2n_plan(&job.input, job.extra.as_deref().unwrap_or(Path::new(""))) {
            Ok(p) => Some(p),
            Err(e) => {
                log(&map, "log", &format!("{} left to the original ({e:#}): {}", entry.role.name(), job.input.display()));
                return run_original(&entry, None);
            }
        },
        _ => None,
    };
    match job.run(plan) {
        Ok(summary) => {
            let summary = if summary.is_empty() { summary } else { format!(" ({summary})") };
            log(&map, "log", &format!("{} native {:.1} s {}{summary}", entry.role.name(), started.elapsed().as_secs_f64(), job.input.display()));
            0
        }
        Err(e) if job.in_place => {
            // The target may be half patched: running the original on it now would be wrong.
            log(&map, "log", &format!("{} FAILED ({e:#}): {}", entry.role.name(), job.output.display()));
            eprintln!("arc-unpack-tool: {}: {e:#}", entry.role.name());
            1
        }
        Err(e) => {
            if job.output.is_file() {
                let _ = std::fs::remove_file(&job.output);
            }
            log(&map, "log", &format!("{} failed ({e:#}), running the original: {}", entry.role.name(), job.input.display()));
            run_original(&entry, None)
        }
    }
}

/// A command line we can carry out ourselves.
struct Job {
    role: Role,
    input: PathBuf,
    output: PathBuf,
    key: Vec<u8>,
    /// The argument that names the output (to redirect the original's output when verifying).
    output_arg: usize,
    /// The output is changed in place (x2n's target), not created.
    in_place: bool,
    /// x2n: the map.
    extra: Option<PathBuf>,
    /// The original tool (Cricket's fgpack runs it on pieces of the input).
    original: PathBuf,
    /// run.exe: the original's command line, with its worker count changed.
    args: Vec<OsString>,
}

impl Job {
    /// Does the job; returns a note for the log (or nothing).
    fn run(&self, plan: Option<tools::X2nPlan>) -> anyhow::Result<String> {
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        match self.role {
            Role::Xor => tools::xor_file(&self.input, &self.output, &self.key).map(|_| String::new()),
            Role::Lz4Cli194 => tools::lz4_compress_file(&self.input, &self.output, 12, threads).map(|_| String::new()),
            Role::X2n => {
                let map = self.extra.as_deref().unwrap_or(Path::new(""));
                match plan {
                    Some(p) => tools::x2n_apply_plan(&self.output, &self.input, p),
                    None => tools::x2n_apply(&self.output, &self.input, map),
                }
                .map(|_| String::new())
            }
            Role::FgpackSegLzma => tools::fgpack_parallel(&self.input, &self.output, &self.original, threads),
            Role::FreeArcUnarc => tools::freearc_extract(&self.input, &self.output, &self.original),
            Role::X5nDirPatch => anyhow::bail!("run by run() itself"),
            Role::RunExe => {
                // (Its exit code is always 0; once it has started, its lines have run: no fallback.)
                std::process::Command::new(&self.original).args(&self.args).status()?;
                Ok(format!("its lines one per CPU instead of {}", self.key.len()))
            }
        }
    }
}

/// The command lines we do ourselves: exactly the forms the repacks use, with an input file
/// that exists and an output that doesn't (the tools' own handling of other cases is theirs).
fn understood(role: Role, args: &[OsString], map: &Path) -> Option<Job> {
    let s: Vec<&str> = args.iter().map(|a| a.to_str()).collect::<Option<_>>()?;
    if role == Role::X5nDirPatch {
        // hpatchz [-s-<size> | -m] [-f] [-n-<n>] <old> <diff> <new>, patching a folder in place
        // (old and new the same folder) from a directory diff, without a -C option of its own.
        let mut paths = Vec::new();
        for a in &s {
            if a.starts_with('-') {
                let known = a.starts_with("-s-") || *a == "-s" || *a == "-m" || *a == "-f" || a.starts_with("-n-");
                if !known {
                    return None;
                }
            } else {
                paths.push(*a);
            }
        }
        let [old, diff, new] = paths[..] else { return None };
        let abs = |p: &str| std::path::absolute(p).ok();
        if abs(old).is_none() || abs(old) != abs(new) || !Path::new(old).is_dir() || !tools::is_dir_diff(Path::new(diff)) {
            return None;
        }
        let mut with = vec![OsString::from("-C-new")];
        with.extend(args.iter().cloned());
        return Some(Job {
            role,
            input: PathBuf::from(diff),
            output: PathBuf::from(new),
            key: Vec::new(),
            output_arg: usize::MAX,
            in_place: true,
            extra: None,
            original: PathBuf::new(),
            args: with,
        });
    }
    if role == Role::RunExe {
        // run " " <url> <list> <N> [NAME=value ...]: only N below the CPU count, for a list whose
        // lines touch disjoint files, changes (to 0 = one per CPU).
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(64);
        let n: i64 = s.get(3)?.trim().parse().ok()?;
        // (N=1 may mean the lines must run one after the other.)
        if !s[0].starts_with(' ') || n <= 1 || n as usize >= cpus {
            return None;
        }
        let text: String = std::fs::read(s[2]).ok()?.iter().map(|&b| b as char).collect();
        if !tools::run_list_lines_independent(&text) {
            return None;
        }
        // Every program the lines run must touch only the files on its line: a tool with a
        // fixed-name temp file (xtool's <exe>-vm.tmp) would collide with its other copies.
        // (run.exe runs its lines in its own folder, which is ours.)
        let here = std::env::current_exe().ok()?.parent()?.to_path_buf();
        let programs = tools::run_list_programs(&text);
        if programs.is_empty() || !programs.iter().all(|p| {
            let p = Path::new(p);
            let p = if p.is_absolute() { p.to_path_buf() } else { here.join(p) };
            tools::named_files_tool(&p, Some(map)).is_some()
        }) {
            return None;
        }
        let mut wider = args.to_vec();
        wider[3] = OsString::from("0");
        // (`key` keeps the old count, for the log.)
        return Some(Job {
            role,
            input: PathBuf::from(s[2]),
            output: PathBuf::new(),
            key: vec![0; n as usize],
            output_arg: usize::MAX,
            in_place: false,
            extra: None,
            original: PathBuf::new(),
            args: wider,
        });
    }
    if role == Role::FreeArcUnarc {
        // FreeArc 0.67 unarc: `x -o+ [-dp<dir>] [--] <archive>`, nothing else (no file lists, no
        // other switches): extract everything into the current folder (or -dp), overwriting.
        let (cmd, rest) = s.split_first()?;
        if *cmd != "x" {
            return None;
        }
        let (mut overwrite, mut base, mut files, mut options_done) = (false, None, Vec::new(), false);
        for a in rest {
            match *a {
                "--" if !options_done => options_done = true,
                "-o+" if !options_done => overwrite = true,
                o if !options_done && o.starts_with("-dp") && o.len() > 3 => base = Some(PathBuf::from(&o[3..])),
                o if !options_done && o.starts_with('-') => return None,
                f => files.push(PathBuf::from(f)),
            }
        }
        let [archive] = <[PathBuf; 1]>::try_from(files).ok()?;
        let out = base.or_else(|| std::env::current_dir().ok())?;
        if !overwrite || !archive.is_file() || !out.is_dir() {
            return None;
        }
        // (Not in place: when ours fails, the original's -o+ rewrites every file.)
        return Some(Job { role, input: archive, output: out, key: Vec::new(), output_arg: usize::MAX, in_place: false, extra: None, original: PathBuf::new(), args: Vec::new() });
    }
    if let (Role::X2n, [t, d, m]) = (role, s.as_slice()) {
        // x2n <target> <data> <map>: all three exist, three different files.
        let paths: Vec<PathBuf> = [t, d, m].iter().map(PathBuf::from).collect();
        let abs: Vec<PathBuf> = paths.iter().filter_map(|p| std::path::absolute(p).ok()).collect();
        let distinct = abs.len() == 3 && abs[0] != abs[1] && abs[1] != abs[2] && abs[0] != abs[2];
        if [t, d, m].iter().any(|a| a.starts_with('-')) || !paths.iter().all(|p| p.is_file()) || !distinct {
            return None;
        }
        let [target, data, map] = <[PathBuf; 3]>::try_from(paths).ok()?;
        return Some(Job { role, input: data, output: target, key: Vec::new(), output_arg: 0, in_place: true, extra: Some(map), original: PathBuf::new(), args: Vec::new() });
    }
    let (input, output, key, output_arg) = match (role, s.as_slice()) {
        (Role::Xor, [i, o, k]) => (*i, *o, tools::xor_key(k)?, 1),
        (Role::Lz4Cli194, ["-12", i, o]) => (*i, *o, Vec::new(), 2),
        (Role::FgpackSegLzma, ["e", i, o]) => (*i, *o, Vec::new(), 2),
        _ => return None,
    };
    if input.starts_with('-') || output.starts_with('-') {
        return None;
    }
    let (input, output) = (PathBuf::from(input), PathBuf::from(output));
    let same = |a: &Path, b: &Path| std::path::absolute(a).ok().zip(std::path::absolute(b).ok()).is_some_and(|(a, b)| a == b);
    if !input.is_file() || output.exists() || same(&input, &output) || output.parent().is_some_and(|p| !p.as_os_str().is_empty() && !p.is_dir()) {
        return None;
    }
    Some(Job { role, input, output, key, output_arg, in_place: false, extra: None, original: PathBuf::new(), args: Vec::new() })
}

/// Runs the original tool with this command line (or with argument `replace.0` replaced).
fn run_original(entry: &Entry, replace: Option<(usize, &Path)>) -> i32 {
    let mut cmd = std::process::Command::new(&entry.original);
    match replace {
        None => {
            raw_tail(&mut cmd);
        }
        Some((i, path)) => {
            let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
            args[i] = path.as_os_str().to_owned();
            cmd.args(args);
        }
    }
    match cmd.status() {
        Ok(st) => st.code().unwrap_or(1),
        Err(e) => {
            eprintln!("arc-unpack-tool: cannot start {}: {e}", entry.original.display());
            1
        }
    }
}

/// Both, compared: the original's output is the one kept.
fn run_verified(entry: &Entry, map: &Path, job: &Job) -> i32 {
    let check = job.output.with_file_name(format!("{}.arc-unpack-check", job.output.file_name().unwrap_or_default().to_string_lossy()));
    // A tool that changes its target in place gets a copy of it to work on.
    if job.in_place && std::fs::copy(&job.output, &check).is_err() {
        return run_original(entry, None);
    }
    let t0 = Instant::now();
    let code = run_original(entry, Some((job.output_arg, &check)));
    let t_orig = t0.elapsed().as_secs_f64();
    let t1 = Instant::now();
    let ours = job.run(None);
    let t_ours = t1.elapsed().as_secs_f64();
    let verdict = match (&ours, std::fs::read(&check), std::fs::read(&job.output)) {
        (Ok(_), Ok(a), Ok(b)) if a == b => format!("same ({} bytes)", a.len()),
        (Ok(_), Ok(a), Ok(b)) => {
            let at = a.iter().zip(&b).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
            format!("DIFFERENT (original {} bytes, ours {} bytes, first difference at {at})", a.len(), b.len())
        }
        (Err(e), _, _) => format!("ours FAILED: {e:#}"),
        (_, Err(e), _) => format!("original wrote nothing ({e}), exit {code}"),
        (_, _, Err(e)) => format!("ours wrote nothing ({e})"),
    };
    log(map, "verify.log", &format!("{} {}: {verdict}; original {t_orig:.1} s, ours {t_ours:.1} s", entry.role.name(), job.input.display()));
    // Keep the original's output under the real name.
    let _ = std::fs::remove_file(&job.output);
    if check.exists() && std::fs::rename(&check, &job.output).is_err() {
        let _ = std::fs::copy(&check, &job.output);
        let _ = std::fs::remove_file(&check);
    }
    code
}

/// The command line after our own program name, untouched, for the original.
fn raw_tail(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let line = unsafe {
            let p = windows_sys::Win32::System::Environment::GetCommandLineW();
            let mut n = 0;
            while *p.add(n) != 0 {
                n += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
        };
        cmd.raw_arg(after_program(&line));
    }
    #[cfg(not(windows))]
    cmd.args(std::env::args_os().skip(1));
}

/// What follows the program name in a Windows command line (CommandLineToArgvW's rule for
/// argument 0: up to the closing quote, or to the first space or tab).
fn after_program(line: &str) -> &str {
    let rest = if let Some(r) = line.strip_prefix('"') {
        r.find('"').map(|i| &r[i + 1..]).unwrap_or("")
    } else {
        line.find([' ', '\t']).map(|i| &line[i..]).unwrap_or("")
    };
    rest.trim_start_matches([' ', '\t'])
}

/// The list of stand-ins: from the environment, else next to an ancestor of our own folder.
fn find_map(me: &Path) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(tools::MAP_ENV).map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(p);
    }
    me.ancestors().skip(1).map(|d| d.join(tools::MAP_FILE)).find(|p| p.is_file())
}

/// Appends one line. In one write: the repack's runner starts dozens of stand-ins at once, and
/// a line written in pieces (as writeln! may) gets mixed with another process's.
fn log(map: &Path, suffix: &str, line: &str) {
    use std::io::Write;
    let path = map.with_extension(suffix);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(format!("{line}\n").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn program_name_is_stripped() {
        use super::after_program;
        assert_eq!(after_program(r#""C:\a b\xx.exe" in out 0x01"#), "in out 0x01");
        assert_eq!(after_program(r"C:\t\xx.exe  -12 a b"), "-12 a b");
        assert_eq!(after_program(r"xx.exe"), "");
    }
}
