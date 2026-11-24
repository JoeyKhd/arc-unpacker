//! Runs a repack installer's own install procedure (`CurStepChanged` in its compiled `[Code]`
//! section) with arc-unpack providing the Inno Setup and ISDone functions it calls. Archive
//! extraction goes through arc-unpack's engine; the repack's post-processing tools run exactly
//! as its script says, in the same order, so the result matches what the installer produces.
//!
//! Safety: the script can only write, delete or start programs inside the game folder and the
//! run's own temp folder (plus `cmd.exe`); the repack folder is read-only to it. UI calls are
//! accepted and ignored.

use crate::inno::Setup;
use crate::pascal::{Host, Script, Value, Vm};
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

/// One thing the script asks for that does real work.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Extract `archive` into `out`, with the FreeArc config `cfg` (the script's `arc.ini`).
    /// With `base`, only that folder inside the archive, its prefix stripped (FreeArc `-ap`).
    Extract { archive: PathBuf, out: PathBuf, cfg: PathBuf, base: Option<String> },
    /// Run `program` with the command-line tail `args` in `dir`.
    Exec { program: PathBuf, args: String, dir: PathBuf },
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Step::Extract { archive, out, base, .. } => {
                let part = base.as_ref().map(|b| format!(" [{b}]")).unwrap_or_default();
                write!(f, "extract {}{part} -> {}", archive.display(), out.display())
            }
            Step::Exec { program, args, dir } => {
                let name = program.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
                write!(f, "run {name}{args} (in {})", dir.display())
            }
        }
    }
}

/// Carries out steps. Returns whether a step succeeded; the script decides what to do then.
pub trait Runner {
    fn run(&mut self, step: &Step) -> Result<bool>;
    /// The script changed a file itself (e.g. an INI value in the codec config).
    fn file_written(&mut self, _path: &Path) {}
}

/// The folders behind Inno's constants.
#[derive(Debug, Clone)]
pub struct Folders {
    /// `{app}`: the game folder.
    pub app: PathBuf,
    /// `{tmp}`: this run's copy of the installer's temporary files.
    pub tmp: PathBuf,
    /// `{src}`: the repack folder.
    pub src: PathBuf,
    /// `{srcexe}`: setup.exe.
    pub setup_exe: PathBuf,
}

/// Inno's TSetupStep values.
pub const SS_INSTALL: i64 = 1;
pub const SS_POST_INSTALL: i64 = 2;
pub const SS_DONE: i64 = 3;

/// Components a default silent install selects (the first setup type's, plus fixed ones), plus
/// optional components whose `*optional*` archive was downloaded (FitGirl names them
/// `fg-optional-<component>...bin`; having downloaded one means wanting it).
pub fn default_components(setup: &Setup, repack_files: &[String]) -> BTreeSet<String> {
    let default_type = setup.types.first().map(|t| t.name.to_ascii_lowercase());
    let mut out = BTreeSet::new();
    for c in &setup.components {
        let name = c.name.to_ascii_lowercase();
        let in_type = default_type
            .as_ref()
            .is_some_and(|d| c.types.split(',').any(|t| t.trim().eq_ignore_ascii_case(d)));
        let downloaded = name.len() >= 3
            && repack_files.iter().any(|f| {
                let f = f.to_ascii_lowercase();
                f.contains("optional") && f.contains(name.rsplit('\\').next().unwrap_or(&name))
            });
        if c.fixed || in_type || downloaded {
            out.insert(name);
        }
    }
    out
}

/// Evaluates an Inno component expression ("a", "a\b", "a or not (b and c)").
fn eval_components(expr: &str, selected: &BTreeSet<String>) -> Result<bool> {
    let spaced = expr.replace('(', " ( ").replace(')', " ) ");
    let tokens: Vec<String> = spaced.split_whitespace().map(|t| t.to_ascii_lowercase()).collect();
    fn or(t: &[String], i: &mut usize, s: &BTreeSet<String>) -> Result<bool> {
        let mut v = and(t, i, s)?;
        while t.get(*i).is_some_and(|x| x == "or") {
            *i += 1;
            v |= and(t, i, s)?;
        }
        Ok(v)
    }
    fn and(t: &[String], i: &mut usize, s: &BTreeSet<String>) -> Result<bool> {
        let mut v = not(t, i, s)?;
        while t.get(*i).is_some_and(|x| x == "and") {
            *i += 1;
            v &= not(t, i, s)?;
        }
        Ok(v)
    }
    fn not(t: &[String], i: &mut usize, s: &BTreeSet<String>) -> Result<bool> {
        let tok = t.get(*i).context("incomplete component expression")?.clone();
        *i += 1;
        Ok(match tok.as_str() {
            "not" => !not(t, i, s)?,
            "(" => {
                let v = or(t, i, s)?;
                ensure!(t.get(*i).is_some_and(|x| x == ")"), "unbalanced parentheses");
                *i += 1;
                v
            }
            name => s.contains(name),
        })
    }
    let mut i = 0;
    let v = or(&tokens, &mut i, selected)?;
    ensure!(i == tokens.len(), "unexpected '{}' in component expression", tokens[i]);
    Ok(v)
}

pub struct InnoHost<'a> {
    pub folders: Folders,
    setup: &'a Setup,
    selected: BTreeSet<String>,
    runner: &'a mut dyn Runner,
    /// Messages the script showed (MsgBox) and refusals; shown to the user afterwards.
    pub messages: Vec<String>,
    /// Every step taken, in order.
    pub steps: Vec<Step>,
    /// Plan only: file changes outside the temp folder are reported as done but not made.
    pub dry: bool,
    /// Files the script changed itself (INI writes), with the number of steps taken before.
    pub writes: Vec<(usize, PathBuf)>,
}

impl<'a> InnoHost<'a> {
    pub fn new(folders: Folders, setup: &'a Setup, selected: BTreeSet<String>, runner: &'a mut dyn Runner) -> InnoHost<'a> {
        InnoHost { folders, setup, selected, runner, messages: Vec::new(), steps: Vec::new(), dry: false, writes: Vec::new() }
    }

    fn expand(&self, s: &str) -> Result<String> {
        let mut out = String::new();
        let mut rest = s;
        while let Some(i) = rest.find('{') {
            out.push_str(&rest[..i]);
            rest = &rest[i..];
            if let Some(r) = rest.strip_prefix("{{") {
                out.push('{');
                rest = r;
                continue;
            }
            let end = rest.find('}').with_context(|| format!("unterminated constant in '{s}'"))?;
            let name = &rest[1..end];
            out.push_str(&self.constant(name)?);
            rest = &rest[end + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }

    fn constant(&self, name: &str) -> Result<String> {
        let p = |p: &Path| p.to_string_lossy().into_owned();
        let env = |k: &str| std::env::var(k).unwrap_or_default();
        let lower = name.to_ascii_lowercase();
        if lower.starts_with("cm:") {
            return Ok(String::new()); // custom messages: only used for labels
        }
        if let Some(var) = name.strip_prefix('%') {
            let (k, default) = var.split_once('|').unwrap_or((var, ""));
            return Ok(std::env::var(k).unwrap_or_else(|_| default.to_string()));
        }
        let win = env("SystemRoot");
        Ok(match lower.as_str() {
            "app" => p(&self.folders.app),
            "tmp" => p(&self.folders.tmp),
            "src" => p(&self.folders.src),
            "srcexe" => p(&self.folders.setup_exe),
            "uninstallexe" => p(&self.folders.app.join("unins000.exe")),
            "cmd" => format!("{win}\\System32\\cmd.exe"),
            "win" => win,
            "sys" | "sysnative" => format!("{win}\\System32"),
            "syswow64" => format!("{win}\\SysWOW64"),
            "sd" => env("SystemDrive"),
            "pf" | "pf64" | "commonpf" | "commonpf64" => env("ProgramFiles"),
            "pf32" | "commonpf32" => env("ProgramFiles(x86)"),
            "cf" | "cf64" | "commoncf" => env("CommonProgramFiles"),
            "cf32" => env("CommonProgramFiles(x86)"),
            "localappdata" => env("LOCALAPPDATA"),
            "userappdata" => env("APPDATA"),
            "commonappdata" => env("ProgramData"),
            "userdocs" => format!("{}\\Documents", env("USERPROFILE")),
            "commondocs" => format!("{}\\Documents", env("PUBLIC")),
            "language" => "en".into(),
            "computername" => env("COMPUTERNAME"),
            "username" => env("USERNAME"),
            "\\" => "\\".into(),
            _ => bail!("the script uses the constant {{{name}}}, which arc-unpack does not provide"),
        })
    }

    /// Normalized absolute path, if it is inside the game folder or the temp folder.
    fn writable(&self, path: &str) -> Option<PathBuf> {
        let p = normalize(Path::new(path))?;
        let inside = |root: &Path| normalize(root).is_some_and(|r| starts_with_ci(&p, &r));
        (inside(&self.folders.app) || inside(&self.folders.tmp)).then_some(p)
    }

    fn runnable(&self, program: &str) -> Option<PathBuf> {
        let p = normalize(Path::new(program))?;
        let cmd = normalize(Path::new(&self.constant("cmd").ok()?))?;
        let inside = |root: &Path| normalize(root).is_some_and(|r| starts_with_ci(&p, &r));
        (p == cmd || inside(&self.folders.app) || inside(&self.folders.tmp) || inside(&self.folders.src)).then_some(p)
    }

    fn refuse(&mut self, what: String) {
        self.messages.push(format!("refused: {what}"));
    }

    fn step(&mut self, step: Step) -> Result<bool> {
        let ok = self.runner.run(&step)?;
        self.steps.push(step);
        Ok(ok)
    }

    fn exec(&mut self, program: &str, args: &str, dir: &str) -> Result<bool> {
        // FitGirl's installers flush the system file cache to free RAM first: it only costs time.
        if Path::new(program).file_name().is_some_and(|n| n.eq_ignore_ascii_case("FlushFileCache.exe")) {
            return Ok(true);
        }
        let Some(program) = self.runnable(program) else {
            self.refuse(format!("running {program} (outside the game, temp and repack folders)"));
            return Ok(false);
        };
        let Some(dir) = self.writable(dir).or_else(|| normalize(Path::new(dir)).filter(|d| d.is_dir())) else {
            bail!("bad working folder '{dir}'");
        };
        let args = if args.is_empty() || args.starts_with(' ') { args.to_string() } else { format!(" {args}") };
        self.step(Step::Exec { program, args, dir })
    }

    fn ini(&mut self, write: bool, section: &str, key: &str, value: &str, file: &str) -> Result<Value> {
        if write {
            let Some(path) = self.writable(file) else {
                self.refuse(format!("writing {file}"));
                return Ok(Value::Int(0));
            };
            let ok = ini_write(&path, section, key, value);
            if ok {
                self.runner.file_written(&path);
                self.writes.push((self.steps.len(), path));
            }
            return Ok(Value::Int(ok as i64));
        }
        Ok(Value::Str(ini_read(Path::new(file), section, key, value)))
    }
}

impl Host for InnoHost<'_> {
    fn signature(&self, key: &str) -> Option<(usize, bool)> {
        if let Some(member) = key.strip_prefix("CLASS:") {
            // Window/control members: accepted and ignored. Setters (`@`) and a few methods
            // return nothing; everything else is a getter/function returning a default.
            let m = member.split('|').nth(1).unwrap_or("");
            const PROCEDURES: &[&str] =
                &["HIDE", "SHOW", "BRINGTOFRONT", "SENDTOBACK", "FREE", "SETFOCUS", "REFRESH", "REPAINT", "UPDATE", "INVALIDATE", "CLEAR", "CLOSE", "SETBOUNDS"];
            return Some((0, !(m.ends_with('@') || PROCEDURES.contains(&m))));
        }
        Some(match key {
            "EXPANDCONSTANT" | "ISCOMPONENTSELECTED" | "ISTASKSELECTED" | "FILEEXISTS" | "DIREXISTS" | "DELETEFILE"
            | "FORCEDIRECTORIES" | "CREATEDIR" | "REMOVEDIR" | "ADDQUOTES" | "REMOVEQUOTES" | "EXTRACTFILEPATH"
            | "EXTRACTFILEDIR" | "EXTRACTFILENAME" | "EXTRACTFILEEXT" | "ADDBACKSLASH" | "REMOVEBACKSLASH"
            | "REMOVEBACKSLASHUNLESSROOT" | "INTTOSTR" | "STRTOINT" | "UPPERCASE" | "LOWERCASE" | "TRIM" | "LENGTH"
            | "SETUPMESSAGE" | "SCALEX" | "SCALEY" | "WIZARDSILENT" => (1, true),
            "STRTOINTDEF" | "RENAMEFILE" | "POS" => (2, true),
            "COPY" | "FILECOPY" | "MINIMIZEPATHNAME" | "MSGBOX" => (3, true),
            "SUPPRESSIBLEMSGBOX" | "SETINISTRING" | "GETINISTRING" | "DELTREE" => (4, true),
            "EXEC" => (6, true),
            "WIZARDFORM" | "MAINFORM" | "GETCMDTAIL" => (0, true),
            "EXTRACTTEMPORARYFILE" => (1, false),
            "dll:ISARCEXTRACT" => (10, true),
            "dll:ISEXEC" => (8, true),
            "dll:ISDONEINIT" => (8, true),
            "dll:ISDONESTOP" => (0, true),
            "dll:CHANGELANGUAGE" | "dll:FILESEARCHINIT" => (1, true),
            "dll:SREPINIT" | "dll:PRECOMPINIT" | "dll:EXEC2" => (3, true),
            "dll:WRAPCALLBACK" => (2, true),
            "dll:SUSPENDPROC" | "dll:RESUMEPROC" => (0, false),
            "dll:SETTASKBARPROGRESSVALUE" | "dll:SETTASKBARPROGRESSSTATE" => (1, false),
            _ => return None,
        })
    }

    fn call(&mut self, key: &str, a: &[Value], out: &mut Vec<(usize, Value)>) -> Result<Option<Value>> {
        let s = |i: usize| a.get(i).map(Value::as_str).unwrap_or_default();
        let int = |i: usize| a.get(i).map(Value::as_int).unwrap_or(0);
        let yes = |b: bool| Some(Value::Int(b as i64));
        if key.starts_with("CLASS:") {
            return Ok(None);
        }
        // Dry run: pretend file changes in the game folder worked (the temp folder is real).
        const CHANGES: &[&str] = &["DELETEFILE", "DELTREE", "FORCEDIRECTORIES", "CREATEDIR", "REMOVEDIR", "RENAMEFILE", "FILECOPY"];
        if self.dry && CHANGES.contains(&key) {
            let tmp = normalize(&self.folders.tmp).unwrap_or_default();
            let target = if key == "FILECOPY" { s(1) } else { s(0) };
            if !normalize(Path::new(&target)).is_some_and(|p| starts_with_ci(&p, &tmp)) {
                return Ok(yes(true));
            }
        }
        Ok(match key {
            "EXPANDCONSTANT" => Some(Value::Str(self.expand(&s(0))?)),
            "ISCOMPONENTSELECTED" => yes(eval_components(&s(0), &self.selected)?),
            "ISTASKSELECTED" => yes(false),
            "WIZARDSILENT" => yes(true),
            "FILEEXISTS" => yes(Path::new(&s(0)).is_file()),
            "DIREXISTS" => yes(Path::new(&s(0)).is_dir()),
            "DELETEFILE" => match self.writable(&s(0)) {
                Some(p) => yes(std::fs::remove_file(p).is_ok()),
                None => {
                    self.refuse(format!("deleting {}", s(0)));
                    yes(false)
                }
            },
            "DELTREE" => {
                let (path, is_dir, files, subdirs) = (s(0), int(1) != 0, int(2) != 0, int(3) != 0);
                match self.writable(&path) {
                    Some(p) if p == normalize(&self.folders.app).unwrap_or_default() => {
                        self.refuse(format!("deleting the whole game folder {path}"));
                        yes(false)
                    }
                    Some(p) => yes(deltree(&p, is_dir, files, subdirs)),
                    None => {
                        self.refuse(format!("deleting {path}"));
                        yes(false)
                    }
                }
            }
            "FORCEDIRECTORIES" | "CREATEDIR" => match self.writable(&s(0)) {
                Some(p) => yes(std::fs::create_dir_all(p).is_ok()),
                None => yes(false),
            },
            "REMOVEDIR" => match self.writable(&s(0)) {
                Some(p) => yes(std::fs::remove_dir(p).is_ok()),
                None => yes(false),
            },
            "RENAMEFILE" => match (self.writable(&s(0)), self.writable(&s(1))) {
                (Some(x), Some(y)) => yes(std::fs::rename(x, y).is_ok()),
                _ => yes(false),
            },
            "FILECOPY" => match self.writable(&s(1)) {
                Some(dst) if int(2) == 0 || !dst.exists() => yes(std::fs::copy(s(0), dst).is_ok()),
                _ => yes(false),
            },
            "ADDQUOTES" => {
                let v = s(0);
                Some(Value::Str(if v.contains(' ') && !v.starts_with('"') { format!("\"{v}\"") } else { v }))
            }
            "REMOVEQUOTES" => Some(Value::Str(s(0).trim_matches('"').to_string())),
            "EXTRACTFILEPATH" => {
                let v = s(0);
                Some(Value::Str(v[..v.rfind(['\\', ':']).map(|i| i + 1).unwrap_or(0)].to_string()))
            }
            "EXTRACTFILEDIR" => {
                let v = s(0);
                let cut = v.rfind(['\\', ':']).unwrap_or(0);
                let keep = if cut > 0 && v.as_bytes()[cut] == b':' { cut + 1 } else { cut };
                Some(Value::Str(v[..keep].to_string()))
            }
            "EXTRACTFILENAME" => Some(Value::Str(s(0).rsplit(['\\', ':']).next().unwrap_or("").to_string())),
            "EXTRACTFILEEXT" => {
                let name = s(0).rsplit(['\\', ':']).next().unwrap_or("").to_string();
                Some(Value::Str(name.rfind('.').map(|i| name[i..].to_string()).unwrap_or_default()))
            }
            "ADDBACKSLASH" => {
                let v = s(0);
                Some(Value::Str(if v.is_empty() || v.ends_with('\\') { v } else { v + "\\" }))
            }
            "REMOVEBACKSLASH" | "REMOVEBACKSLASHUNLESSROOT" => {
                let v = s(0);
                let root = key == "REMOVEBACKSLASHUNLESSROOT" && v.len() == 3 && v.ends_with(":\\");
                Some(Value::Str(if root { v } else { v.trim_end_matches('\\').to_string() }))
            }
            "INTTOSTR" => Some(Value::Str(int(0).to_string())),
            "STRTOINT" => Some(Value::Int(s(0).trim().parse().context("StrToInt")?)),
            "STRTOINTDEF" => Some(Value::Int(s(0).trim().parse().unwrap_or(int(1)))),
            "UPPERCASE" => Some(Value::Str(s(0).to_uppercase())),
            "LOWERCASE" => Some(Value::Str(s(0).to_lowercase())),
            "TRIM" => Some(Value::Str(s(0).trim().to_string())),
            "LENGTH" => Some(Value::Int(s(0).chars().count() as i64)),
            "POS" => Some(Value::Int(s(1).find(&s(0)).map(|i| s(1)[..i].chars().count() as i64 + 1).unwrap_or(0))),
            "COPY" => {
                let v: Vec<char> = s(0).chars().collect();
                let start = (int(1).max(1) - 1) as usize;
                let n = int(2).max(0) as usize;
                Some(Value::Str(v.iter().skip(start).take(n).collect()))
            }
            "MINIMIZEPATHNAME" => Some(Value::Str(s(0))),
            "SETUPMESSAGE" | "GETCMDTAIL" => Some(Value::Str(String::new())),
            "SCALEX" | "SCALEY" => Some(Value::Int(int(0))),
            "WIZARDFORM" => Some(Value::Obj(1)),
            "MAINFORM" => Some(Value::Obj(2)),
            "MSGBOX" | "SUPPRESSIBLEMSGBOX" => {
                self.messages.push(format!("installer message: {}", s(0).trim()));
                // Silent installs answer with the default button: OK / Yes.
                let default = if key == "SUPPRESSIBLEMSGBOX" { int(3) } else { 0 };
                Some(Value::Int(if default != 0 {
                    default
                } else {
                    match int(2) & 0xF {
                        3 | 4 => 6, // IDYES
                        2 => 3,     // IDABORT
                        5 => 4,     // IDRETRY
                        _ => 1,     // IDOK
                    }
                }))
            }
            "SETINISTRING" => Some(self.ini(true, &s(0), &s(1), &s(2), &s(3))?),
            "GETINISTRING" => Some(self.ini(false, &s(0), &s(1), &s(2), &s(3))?),
            "EXTRACTTEMPORARYFILE" => {
                let name = s(0);
                let dest = self.folders.tmp.join(&name);
                if !dest.exists() {
                    let f = self
                        .setup
                        .files
                        .iter()
                        .find(|f| f.has_data() && f.dest.strip_prefix("{tmp}\\").is_some_and(|n| n.eq_ignore_ascii_case(&name)))
                        .with_context(|| format!("the installer has no temporary file {name}"))?;
                    std::fs::write(&dest, self.setup.read(f)?)?;
                }
                None
            }
            "EXEC" => {
                let ok = self.exec(&s(0), &s(1), &s(2))?;
                out.push((5, Value::Int(if ok { 0 } else { 1 })));
                yes(ok)
            }
            "dll:ISEXEC" => yes(self.exec(&s(3), &s(4), &s(5))?),
            "dll:EXEC2" => {
                let tmp = self.folders.tmp.to_string_lossy().into_owned();
                yes(self.exec(&s(0), &s(1), &tmp)?)
            }
            "dll:ISARCEXTRACT" => {
                let (archive, dest, inner, password, cfg) = (s(2), s(3), s(4), s(6), s(7));
                let base = inner.trim_matches(['\\', '/']).to_string();
                ensure!(!base.split(['\\', '/']).any(|p| p == ".."), "bad path inside archive '{inner}'");
                // FitGirl passes the unarc.dll path in the password slot; real passwords aren't supported.
                ensure!(password.is_empty() || password.to_ascii_lowercase().ends_with(".dll"), "password-protected archives are not supported");
                let Some(out_dir) = self.writable(&dest) else {
                    self.refuse(format!("extracting into {dest}"));
                    return Ok(yes(false));
                };
                let cfg = if cfg.is_empty() { self.folders.tmp.join("arc.ini") } else { PathBuf::from(cfg) };
                let base = (!base.is_empty()).then_some(base);
                let ok = self.step(Step::Extract { archive: PathBuf::from(&archive), out: out_dir, cfg, base })?;
                if ok && int(5) != 0 {
                    // DeleteInFile: only archives the install itself produced, never the downloads.
                    match self.writable(&archive) {
                        Some(p) => {
                            let _ = std::fs::remove_file(p);
                        }
                        None => self.messages.push(format!("kept {archive} (the script asked to delete it after extraction)")),
                    }
                }
                yes(ok)
            }
            "dll:ISDONEINIT" | "dll:ISDONESTOP" | "dll:CHANGELANGUAGE" | "dll:FILESEARCHINIT" | "dll:SREPINIT"
            | "dll:PRECOMPINIT" => yes(true),
            "dll:WRAPCALLBACK" => Some(Value::Int(0)),
            "dll:SUSPENDPROC" | "dll:RESUMEPROC" | "dll:SETTASKBARPROGRESSVALUE" | "dll:SETTASKBARPROGRESSSTATE" => None,
            _ => bail!("{key} is declared but not implemented"),
        })
    }
}

/// Runs the install procedure for one setup step. Steps without script code are no-ops.
pub fn run_step(script: &Script, vm: &mut Vm, host: &mut InnoHost, step: i64) -> Result<()> {
    if script.find("CURSTEPCHANGED").is_none() {
        return Ok(());
    }
    vm.call("CURSTEPCHANGED", &[Value::Int(step)], host)
}

/// Absolute path without `.`/`..` (purely lexical), or None for relative/odd paths.
fn normalize(p: &Path) -> Option<PathBuf> {
    if !p.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::Normal(n) => out.push(n),
        }
    }
    Some(out)
}

fn starts_with_ci(p: &Path, root: &Path) -> bool {
    let (p, r) = (p.to_string_lossy().to_lowercase(), root.to_string_lossy().to_lowercase());
    let r = r.trim_end_matches('\\');
    p == r || p.starts_with(&format!("{r}\\"))
}

fn deltree(p: &Path, is_dir: bool, files: bool, subdirs: bool) -> bool {
    if is_dir {
        if !p.is_dir() {
            return false;
        }
        if files && subdirs {
            return std::fs::remove_dir_all(p).is_ok();
        }
        if files {
            for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
                if e.path().is_file() {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        return std::fs::remove_dir(p).is_ok() || files;
    }
    // A file name or wildcard pattern: delete matching files in that folder.
    let (dir, pattern) = (p.parent().unwrap_or(Path::new(".")), p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    let Some(pattern) = pattern else { return false };
    let mut any = false;
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().to_lowercase();
        if wildcard(&pattern, &name) {
            let path = e.path();
            any |= if path.is_dir() { subdirs && std::fs::remove_dir_all(&path).is_ok() } else { files && std::fs::remove_file(&path).is_ok() };
        }
    }
    any
}

fn wildcard(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    fn go(p: &[char], n: &[char]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some('*'), _) => go(&p[1..], n) || (!n.is_empty() && go(p, &n[1..])),
            (Some('?'), Some(_)) => go(&p[1..], &n[1..]),
            (Some(a), Some(b)) => a == b && go(&p[1..], &n[1..]),
            _ => false,
        }
    }
    go(&p, &n)
}

#[cfg(windows)]
fn ini_write(path: &Path, section: &str, key: &str, value: &str) -> bool {
    use windows_sys::Win32::System::WindowsProgramming::WritePrivateProfileStringW;
    let w = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (sec, k, v, f) = (w(section), w(key), w(value), w(&path.to_string_lossy()));
    unsafe { WritePrivateProfileStringW(sec.as_ptr(), k.as_ptr(), v.as_ptr(), f.as_ptr()) != 0 }
}

#[cfg(windows)]
fn ini_read(path: &Path, section: &str, key: &str, default: &str) -> String {
    use windows_sys::Win32::System::WindowsProgramming::GetPrivateProfileStringW;
    let w = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (sec, k, d, f) = (w(section), w(key), w(default), w(&path.to_string_lossy()));
    let mut buf = vec![0u16; 4096];
    let n = unsafe { GetPrivateProfileStringW(sec.as_ptr(), k.as_ptr(), d.as_ptr(), buf.as_mut_ptr(), buf.len() as u32, f.as_ptr()) };
    String::from_utf16_lossy(&buf[..n as usize])
}

#[cfg(not(windows))]
fn ini_write(_: &Path, _: &str, _: &str, _: &str) -> bool {
    false
}

#[cfg(not(windows))]
fn ini_read(_: &Path, _: &str, _: &str, default: &str) -> String {
    default.to_string()
}

/// Records steps without doing them (for `--dry-run`): extraction "succeeds", programs "run".
pub struct PlanOnly;

impl Runner for PlanOnly {
    fn run(&mut self, _: &Step) -> Result<bool> {
        Ok(true)
    }
}

/// Writes the installer's `{tmp}` files into `dir` (Inno extracts these before the script runs).
pub fn prepare_tmp(setup: &Setup, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    setup.extract_with(|f| match f.dest.strip_prefix("{tmp}\\") {
        Some(rel) if !rel.is_empty() => Ok(Some(dir.join(crate::inno::safe_dest(rel)?))),
        _ => Ok(None),
    })?;
    Ok(())
}

/// Names of the files in the repack folder (for optional-component detection).
pub fn repack_files(repack: &Path) -> Vec<String> {
    std::fs::read_dir(repack)
        .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default()
}

/// What a repack's install script would do, without doing it.
pub struct Plan {
    /// Every component with whether a default install (plus downloaded optional parts) selects it.
    pub components: Vec<(String, String, bool)>,
    pub steps: Vec<Step>,
    /// Files the script changes itself, with the number of steps taken before (see InnoHost).
    pub writes: Vec<(usize, PathBuf)>,
    pub messages: Vec<String>,
    /// Set if the script stopped (e.g. it calls something arc-unpack doesn't provide).
    pub error: Option<String>,
}

/// Runs the install script against a scratch temp folder with nothing extracted or run.
/// Extraction "succeeds" without producing files, so steps that check for extracted files may
/// be missing from the listing.
pub fn plan(setup_exe: &Path, repack: &Path, target: &Path) -> Result<Plan> {
    let setup = Setup::open(setup_exe)?;
    ensure!(!setup.code.is_empty(), "this installer has no install script");
    let script = Script::load(&setup.code)?;
    let tmp = std::env::temp_dir().join(format!("arcunp-plan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    prepare_tmp(&setup, &tmp)?;
    let selected = default_components(&setup, &repack_files(repack));
    let components = setup
        .components
        .iter()
        .filter(|c| crate::install::english_or_neutral(&c.languages))
        .map(|c| (c.name.clone(), c.description.clone(), selected.contains(&c.name.to_ascii_lowercase())))
        .collect();
    let folders = Folders {
        app: std::path::absolute(target)?,
        tmp: tmp.clone(),
        src: std::path::absolute(repack)?,
        setup_exe: std::path::absolute(setup_exe)?,
    };
    let mut runner = PlanOnly;
    let mut host = InnoHost::new(folders, &setup, selected, &mut runner);
    host.dry = true;
    let mut vm = Vm::new(&script);
    let mut error = None;
    for step in [SS_INSTALL, SS_POST_INSTALL, SS_DONE] {
        if let Err(e) = run_step(&script, &mut vm, &mut host, step) {
            error = Some(format!("{e:#}"));
            break;
        }
    }
    let plan = Plan {
        components,
        steps: std::mem::take(&mut host.steps),
        writes: std::mem::take(&mut host.writes),
        messages: std::mem::take(&mut host.messages),
        error,
    };
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_expressions() {
        let sel: BTreeSet<String> = ["text", "bonus", "voice\\eng"].iter().map(|s| s.to_string()).collect();
        assert!(eval_components("bonus", &sel).unwrap());
        assert!(eval_components("Voice\\Eng", &sel).unwrap());
        assert!(!eval_components("voice\\rus", &sel).unwrap());
        assert!(eval_components("voice\\rus or (text and not directx)", &sel).unwrap());
        assert!(eval_components("(bonus)", &sel).is_ok());
        assert!(eval_components("bonus and", &sel).is_err());
    }

    #[test]
    fn path_rules() {
        assert!(starts_with_ci(Path::new(r"C:\Games\X\a.txt"), Path::new(r"c:\games\x")));
        assert!(!starts_with_ci(Path::new(r"C:\Games\Xy\a.txt"), Path::new(r"C:\Games\X")));
        assert_eq!(normalize(Path::new(r"C:\Games\X\..\Y")), Some(PathBuf::from(r"C:\Games\Y")));
        assert_eq!(normalize(Path::new(r"relative\x")), None);
        assert!(wildcard("*.addon", "bonus.addon"));
        assert!(!wildcard("*.addon", "bonus.addon2"));
        assert!(wildcard("0?.fgpack", "01.fgpack"));
    }
}
