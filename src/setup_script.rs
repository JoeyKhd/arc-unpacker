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
#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// One thing the script asks for that does real work.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Extract `archive` into `out`, with the FreeArc config `cfg` (the script's `arc.ini`).
    /// With `base`, only that folder inside the archive, its prefix stripped (FreeArc `-ap`).
    Extract { archive: PathBuf, out: PathBuf, cfg: PathBuf, base: Option<String> },
    /// Run `program` with the command-line tail `args` in `dir`.
    Exec { program: PathBuf, args: String, dir: PathBuf },
    /// Extract the RAR archive `archive` into `out` with the installer's `unrar` (Unrar.dll), as
    /// ISDone's ISRarExtract does (older repacks rebuild a RAR file and unpack it).
    Rar { archive: PathBuf, out: PathBuf, unrar: PathBuf, password: String },
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
            Step::Rar { archive, out, .. } => write!(f, "unrar {} -> {}", archive.display(), out.display()),
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
/// optional components whose archive was downloaded (FitGirl names them
/// `fg-optional-<component>...bin`, and language packs `fg-selective-<language>.bin`; having
/// downloaded one means wanting it).
pub fn default_components(setup: &Setup, repack_files: &[String]) -> BTreeSet<String> {
    let default_type = setup.types.first().map(|t| t.name.to_ascii_lowercase());
    let mut out = BTreeSet::new();
    for c in &setup.components {
        let name = c.name.to_ascii_lowercase();
        let in_type = default_type
            .as_ref()
            .is_some_and(|d| c.types.split(',').any(|t| t.trim().eq_ignore_ascii_case(d)));
        // A downloaded optional archive ticks its part, but not in a "pick one" group: there the
        // installer's default stays (the first choice, see `with_exclusive_choices`). Left 4 Dead
        // 2 with several voice packs downloaded installs English by default, not the first pack.
        if c.fixed || in_type || (!c.exclusive && downloaded_part(c, repack_files)) {
            out.insert(name);
        }
    }
    with_exclusive_choices(setup, out)
}

/// Inno Setup keeps exactly one choice of a "pick one" group (components with the exclusive
/// flag, such as FitGirl's game language) ticked whenever the group's parent is ticked: the first
/// one, unless another is ticked already, and of several ticked ones the first. Both measured:
/// `/COMPONENTS=text,local` installs the first language (Ancient Wars: Sparta HD), and
/// `/COMPONENTS=text,local,local\eng,local\rus,...` installs only English (Call of Duty: Black Ops).
pub fn with_exclusive_choices(setup: &Setup, selected: BTreeSet<String>) -> BTreeSet<String> {
    exclusive_choices_of(&setup.components, selected)
}

fn exclusive_choices_of(components: &[crate::inno::Component], mut selected: BTreeSet<String>) -> BTreeSet<String> {
    let components: Vec<(String, bool)> = components
        .iter()
        .filter(|c| crate::install::english_or_neutral(&c.languages))
        .map(|c| (c.name.to_ascii_lowercase(), c.exclusive))
        .collect();
    // In definition order, so a choice ticked here can tick a group below it in turn.
    for (parent, _) in &components {
        if !selected.contains(parent) {
            continue;
        }
        let prefix = format!("{parent}\\");
        let group: Vec<&String> = components
            .iter()
            .filter(|(name, exclusive)| *exclusive && name.strip_prefix(&prefix).is_some_and(|rest| !rest.contains('\\')))
            .map(|(name, _)| name)
            .collect();
        let ticked: Vec<&String> = group.iter().copied().filter(|name| selected.contains(*name)).collect();
        match ticked.as_slice() {
            [] => {
                if let Some(first) = group.first() {
                    selected.insert((*first).clone());
                }
            }
            [_] => {}
            [_keep, rest @ ..] => {
                for name in rest {
                    selected.remove(*name);
                }
            }
        }
    }
    selected
}

/// Whether the repack folder has an optional/selective archive for this component, e.g.
/// `bonus` -> `fg-optional-bonus-content.bin`, `local\esp` ("Spanish") -> `fg-selective-spanish.bin`.
fn downloaded_part(component: &crate::inno::Component, repack_files: &[String]) -> bool {
    repack_files.iter().any(|f| part_file_matches(component, f))
}

/// Whether `file` is the optional/selective archive of `component`: by the last part of its
/// name (`voice\french` -> `...french...`), or by its description when that is one word, as
/// FitGirl's language names are (`local\esp` "Spanish" -> `...spanish...`), or by the words of
/// the archive's part name: their initials (`le` -> `fg-optional-limited-edition.bin`) or as a
/// phrase in the description ("Install Limited Edition as well").
fn part_file_matches(component: &crate::inno::Component, file: &str) -> bool {
    if !is_optional_archive(file) {
        return false;
    }
    let file = file.to_ascii_lowercase();
    let leaf = component.name.rsplit('\\').next().unwrap_or(&component.name).to_ascii_lowercase();
    let word: String = component.description.trim().to_ascii_lowercase();
    let one_word = !word.starts_with('{') && word.len() >= 4 && word.chars().all(|c| c.is_ascii_alphabetic());
    if (leaf.len() >= 3 && file.contains(&leaf)) || (one_word && file.contains(&word)) {
        return true;
    }
    // "fg-optional-limited-edition.bin" -> ["limited", "edition"].
    let stem = file.rsplit_once('.').map_or(file.as_str(), |(s, _)| s);
    let part = ["optional-", "selective-"].iter().find_map(|k| stem.split_once(k).map(|(_, rest)| rest)).unwrap_or("");
    let words: Vec<&str> = part.split(['-', '_', ' ']).filter(|w| !w.is_empty()).collect();
    if words.len() < 2 {
        return false;
    }
    let initials: String = words.iter().filter_map(|w| w.chars().next()).collect();
    let phrase = words.join(" ");
    (leaf.len() >= 2 && initials == leaf) || (phrase.len() >= 6 && word.contains(&phrase))
}

/// Inno Setup's uninstaller (`unins000.exe`, `unins001.exe`, ...).
fn is_uninstaller(program: &str) -> bool {
    let name = Path::new(program.trim_matches('"')).file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    name.strip_prefix("unins").and_then(|r| r.strip_suffix(".exe")).is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
}

/// Whether a repack file is an optional part (FitGirl: `fg-optional-*.bin`, language packs
/// `fg-selective-*.bin`) rather than part of the base game.
pub fn is_optional_archive(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("optional") || n.contains("selective")
}

/// Which of a repack's archives an install takes. FitGirl's scripts extract every archive that
/// is in the repack folder, so this is decided here: a normal install leaves out the optional
/// parts the user unticked; adding parts to an installed game takes only the ticked parts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parts {
    /// Optional-part archives (lower-case file names) to leave out.
    pub skip: BTreeSet<String>,
    /// Add parts to a game that is already installed: leave out every archive that isn't an
    /// optional part.
    pub only_optional: bool,
}

impl Parts {
    /// Whether the install leaves this repack archive out (with the steps that process it).
    pub fn skips(&self, archive_name: &str) -> bool {
        let n = archive_name.to_ascii_lowercase();
        if is_optional_archive(&n) { self.skip.contains(&n) } else { self.only_optional }
    }
}

/// The components that make up the base game: the installer's fixed ones (FitGirl's "Main game
/// files"). Installers without a fixed component have no base game to leave out.
pub fn base_components(setup: &Setup) -> BTreeSet<String> {
    base_of(&setup.components)
}

fn base_of(components: &[crate::inno::Component]) -> BTreeSet<String> {
    components
        .iter()
        .filter(|c| c.fixed && crate::install::english_or_neutral(&c.languages))
        .map(|c| c.name.to_ascii_lowercase())
        .collect()
}

/// The archives a selection takes: an optional part whose components are all unticked is left
/// out; with no base-game component selected, only the selected parts are added, to a game
/// that is already installed.
pub fn parts_for(setup: &Setup, repack_files: &[String], selected: &BTreeSet<String>) -> Parts {
    parts_of(&setup.components, repack_files, selected)
}

fn parts_of(components: &[crate::inno::Component], repack_files: &[String], selected: &BTreeSet<String>) -> Parts {
    let base = base_of(components);
    let mut skip = BTreeSet::new();
    for f in repack_files.iter().filter(|f| is_optional_archive(f)) {
        let owners: Vec<String> = components.iter().filter(|c| part_file_matches(c, f)).map(|c| c.name.to_ascii_lowercase()).collect();
        // An archive no component claims is installed, as the installer would.
        if !owners.is_empty() && owners.iter().all(|o| !selected.contains(o)) {
            skip.insert(f.to_ascii_lowercase());
        }
    }
    Parts { skip, only_optional: !base.is_empty() && base.is_disjoint(selected) }
}

/// Evaluates an Inno component expression ("a", "a\b", "a or not (b and c)").
pub fn eval_components(expr: &str, selected: &BTreeSet<String>) -> Result<bool> {
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
    /// Component names the script asked about (IsComponentSelected), lowercase.
    pub queried: BTreeSet<String>,
    /// Which repack archives to take (see `Parts`).
    pub parts: Parts,
    /// Inside a left-out part: from a skipped repack archive to the next repack archive, the
    /// steps process the skipped archive's files, so they are left out too.
    skipping: bool,
    /// Steps left out this way, in order.
    pub skipped: Vec<Step>,
    /// The component masks the script gave ISDoneInit (Comp1..Comp3), if it called it.
    isdone_components: Option<[u64; 3]>,
}

impl<'a> InnoHost<'a> {
    pub fn new(folders: Folders, setup: &'a Setup, selected: BTreeSet<String>, runner: &'a mut dyn Runner) -> InnoHost<'a> {
        InnoHost {
            folders,
            setup,
            selected,
            runner,
            messages: Vec::new(),
            steps: Vec::new(),
            dry: false,
            writes: Vec::new(),
            queried: BTreeSet::new(),
            parts: Parts::default(),
            skipping: false,
            skipped: Vec::new(),
            isdone_components: None,
        }
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
        // Each repack archive starts a part: its extraction and the steps after it, up to the
        // next repack archive, which process its files.
        if let Step::Extract { archive, .. } = &step {
            let src = normalize(&self.folders.src).unwrap_or_default();
            if normalize(archive).is_some_and(|a| starts_with_ci(&a, &src)) {
                self.skipping = archive.file_name().is_some_and(|n| self.parts.skips(&n.to_string_lossy()));
            }
        }
        if self.skipping {
            self.skipped.push(step);
            return Ok(true);
        }
        let ok = self.runner.run(&step)?;
        self.steps.push(step);
        Ok(ok)
    }

    fn exec(&mut self, program: &str, args: &str, dir: &str) -> Result<bool> {
        // FitGirl's installers flush the system file cache to free RAM first: it only costs time.
        if Path::new(program).file_name().is_some_and(|n| n.eq_ignore_ascii_case("FlushFileCache.exe")) {
            return Ok(true);
        }
        // FitGirl's scripts run `{uninstallexe} /VERYSILENT` to clean up after a failed step. In
        // a game folder the original installer made, that would remove the whole game.
        if is_uninstaller(program) || args.split(['"', ' ', '&', '|']).any(is_uninstaller) {
            self.refuse(format!("running the uninstaller {program} (the script's clean-up after a failure)"));
            return Ok(false);
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
            "dll:ISRAREXTRACT" => (6, true),
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
        // Dry run, or inside a left-out part: pretend file changes in the game folder worked
        // (the temp folder is real: its codec settings still apply to later parts).
        const CHANGES: &[&str] = &["DELETEFILE", "DELTREE", "FORCEDIRECTORIES", "CREATEDIR", "REMOVEDIR", "RENAMEFILE", "FILECOPY"];
        if (self.dry || self.skipping) && CHANGES.contains(&key) {
            let tmp = normalize(&self.folders.tmp).unwrap_or_default();
            let target = if key == "FILECOPY" { s(1) } else { s(0) };
            if !normalize(Path::new(&target)).is_some_and(|p| starts_with_ci(&p, &tmp)) {
                return Ok(yes(true));
            }
        }
        Ok(match key {
            "EXPANDCONSTANT" => Some(Value::Str(self.expand(&s(0))?)),
            "ISCOMPONENTSELECTED" => {
                let expr = s(0);
                for w in expr.replace(['(', ')'], " ").split_whitespace() {
                    let w = w.to_ascii_lowercase();
                    if !matches!(w.as_str(), "and" | "or" | "not") {
                        self.queried.insert(w);
                    }
                }
                yes(eval_components(&expr, &self.selected)?)
            }
            "ISTASKSELECTED" => yes(false),
            "WIZARDSILENT" => yes(true),
            "FILEEXISTS" => {
                let path = s(0);
                // A plan doesn't extract or run anything, so files a successful install makes in
                // the game folder don't exist yet. Scripts that check their own results (FitGirl's
                // "_Redist\QuickSFV.good" self-check in older repacks) would take their failure
                // path, and the plan would end there: no early extraction at all.
                let inside = |root: &Path| normalize(Path::new(&path)).zip(normalize(root)).is_some_and(|(p, r)| starts_with_ci(&p, &r));
                let in_app = self.dry && inside(&self.folders.app);
                // FitGirl's "Checking folder name..." step: check.bat writes {src}\fg-99.bin, the
                // script fails without it (Max Payne 3). Only that name: other files a script looks
                // for in the repack folder are downloaded archives, which really may be missing.
                let sentinel = self.dry
                    && inside(&self.folders.src)
                    && Path::new(&path).file_name().is_some_and(|n| n.eq_ignore_ascii_case("fg-99.bin"));
                yes(in_app || sentinel || Path::new(&path).is_file())
            }
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
            "dll:ISEXEC" => {
                // CurComponent, as for ISArcExtract: Kerbal Space Program ties rus.bat, esp.bat, ...
                // to their language's component, and ISDone skips the others.
                if let (n @ 1..=96, Some(masks)) = (int(0), self.isdone_components) {
                    let bit = (n - 1) as usize;
                    if masks[bit / 32] & (1 << (bit % 32)) == 0 {
                        return Ok(yes(true));
                    }
                }
                // ISDone creates a missing working folder: Sleeping Dogs runs build01.bat in
                // {app}\temp\04\, which nothing before it creates. (Only where the script may write.)
                if !self.dry
                    && let Some(dir) = self.writable(&s(5))
                    && !dir.exists()
                {
                    let _ = std::fs::create_dir_all(&dir);
                }
                yes(self.exec(&s(3), &s(4), &s(5))?)
            }
            "dll:EXEC2" => {
                let tmp = self.folders.tmp.to_string_lossy().into_owned();
                yes(self.exec(&s(0), &s(1), &tmp)?)
            }
            "dll:ISRAREXTRACT" => {
                // ISRarExtract(CurComponent, PctOfTotal, InName, OutPath, DeleteInFile, Password).
                if let (n @ 1..=96, Some(masks)) = (int(0), self.isdone_components) {
                    let bit = (n - 1) as usize;
                    if masks[bit / 32] & (1 << (bit % 32)) == 0 {
                        return Ok(yes(true));
                    }
                }
                let (archive, dest, password) = (s(2), s(3), s(5));
                let Some(out_dir) = self.writable(&dest) else {
                    self.refuse(format!("unpacking into {dest}"));
                    return Ok(yes(false));
                };
                let unrar = self.folders.tmp.join("Unrar.dll");
                let ok = self.step(Step::Rar { archive: PathBuf::from(&archive), out: out_dir, unrar, password })?;
                if ok && int(4) != 0 {
                    // DeleteInFile: only files the install itself produced, never the downloads.
                    match self.writable(&archive) {
                        Some(p) => {
                            let _ = std::fs::remove_file(p);
                        }
                        None => self.messages.push(format!("kept {archive} (the script asked to delete it after unpacking)")),
                    }
                }
                yes(ok)
            }
            "dll:ISARCEXTRACT" => {
                // CurComponent: ISDone extracts an archive of component n only if bit n-1 of the
                // masks given to ISDoneInit is set (0 = always), and reports success either way.
                if let (n @ 1..=96, Some(masks)) = (int(0), self.isdone_components) {
                    let bit = (n - 1) as usize;
                    if masks[bit / 32] & (1 << (bit % 32)) == 0 {
                        return Ok(yes(true));
                    }
                }
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
            "dll:ISDONEINIT" => {
                // ISDoneInit(RecordFileName, TimeType, Comp1, Comp2, Comp3, ...): which
                // components' archives ISArcExtract extracts.
                self.isdone_components = Some([int(2) as u64, int(3) as u64, int(4) as u64]);
                yes(true)
            }
            "dll:PRECOMPINIT" => {
                // PrecompInit(TmpPath, VirtMem, PrecompVers): ISDone gives the precomp CLS codec
                // its temp folder and memory in {tmp}\CLS.ini, [Precomp]. Without them it keeps
                // CLS.ini's "{app}" placeholder and fails (Pro Evolution Soccer 2017).
                let cls = self.folders.tmp.join("CLS.ini").to_string_lossy().into_owned();
                let (path, memory) = (s(0), int(1));
                if !path.is_empty() {
                    self.ini(true, "Precomp", "TempPath", &path, &cls)?;
                }
                if memory > 0 {
                    self.ini(true, "Precomp", "Memory", &memory.to_string(), &cls)?;
                }
                yes(true)
            }
            "dll:ISDONESTOP" | "dll:CHANGELANGUAGE" | "dll:FILESEARCHINIT" | "dll:SREPINIT" => yes(true),
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

/// A plan needs the installer's `{tmp}` files (scripts check for them and edit its INI files).
/// Unpacking them takes a moment, so one copy per installer is kept and copied for each plan.
fn fill_plan_tmp(setup: &Setup, dir: &Path) -> Result<()> {
    let template = std::env::temp_dir().join(format!("arc-unpack-plan-{}", setup.fingerprint()));
    if !template.join(".ready").is_file() {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let part = template.with_extension(format!("part-{}-{n}", std::process::id()));
        prepare_tmp(setup, &part)?;
        std::fs::write(part.join(".ready"), "")?;
        if std::fs::rename(&part, &template).is_err() {
            // Another plan finished first (fine), or a crashed one left a half-made copy.
            if !template.join(".ready").is_file() {
                let _ = std::fs::remove_dir_all(&template);
                if std::fs::rename(&part, &template).is_ok() {
                    return copy_tree(&template, dir);
                }
            }
            let _ = std::fs::remove_dir_all(&part);
        }
    }
    copy_tree(&template, dir)
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let dst = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_tree(&e.path(), &dst)?;
        } else if e.file_name() != ".ready" {
            std::fs::copy(e.path(), &dst)?;
        }
    }
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
    /// Components the script asked about (the ones worth offering as choices).
    pub queried: BTreeSet<String>,
    pub messages: Vec<String>,
    /// Set if the script stopped (e.g. it calls something arc-unpack doesn't provide).
    pub error: Option<String>,
    /// The scratch `{tmp}` the plan ran with (deleted again); step paths inside it map to the
    /// real run's `{tmp}`.
    pub tmp: PathBuf,
    /// Which repack archives the selection takes.
    pub parts: Parts,
    /// Steps left out with the archives the selection doesn't take.
    pub skipped: Vec<Step>,
}

/// Runs the install script against a scratch temp folder with nothing extracted or run.
/// Extraction "succeeds" without producing files, so steps that check for extracted files may
/// be missing from the listing.
pub fn plan(setup_exe: &Path, repack: &Path, target: &Path, selected: Option<BTreeSet<String>>) -> Result<Plan> {
    let setup = Setup::open(setup_exe)?;
    ensure!(!setup.code.is_empty(), "this installer has no install script");
    let script = Script::load(&setup.code)?;
    plan_with(&setup, &script, setup_exe, repack, target, selected)
}

/// `plan` for an installer that is already open.
pub fn plan_with(
    setup: &Setup,
    script: &Script,
    setup_exe: &Path,
    repack: &Path,
    target: &Path,
    selected: Option<BTreeSet<String>>,
) -> Result<Plan> {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("arcunp-plan-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    fill_plan_tmp(setup, &tmp)?;
    let files = repack_files(repack);
    let selected = match selected {
        Some(chosen) => with_exclusive_choices(setup, chosen),
        None => default_components(setup, &files),
    };
    let parts = parts_for(setup, &files, &selected);
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
    let mut host = InnoHost::new(folders, setup, selected, &mut runner);
    host.dry = true;
    host.parts = parts.clone();
    let mut vm = Vm::new(script);
    let mut error = None;
    for step in [SS_INSTALL, SS_POST_INSTALL, SS_DONE] {
        if let Err(e) = run_step(script, &mut vm, &mut host, step) {
            error = Some(format!("{e:#}"));
            break;
        }
    }
    let plan = Plan {
        components,
        steps: std::mem::take(&mut host.steps),
        writes: std::mem::take(&mut host.writes),
        queried: std::mem::take(&mut host.queried),
        messages: std::mem::take(&mut host.messages),
        error,
        tmp: tmp.clone(),
        parts,
        skipped: std::mem::take(&mut host.skipped),
    };
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(plan)
}

/// A component the user can choose, as the window shows it.
#[derive(Debug, Clone)]
pub struct Choice {
    /// Inno component name, lowercase (what `--components` takes).
    pub name: String,
    /// The installer's description (or the name if it only has a message reference).
    pub label: String,
    /// Always installed.
    pub fixed: bool,
    /// One of a group where only one may be chosen (e.g. a language).
    pub exclusive: bool,
    /// Selected by default.
    pub default: bool,
    /// The optional/selective archive found for it in the repack folder.
    pub file: Option<String>,
}

/// The components worth offering: fixed ones (shown as required) and the ones the install
/// script asks about. Components that only drive redistributable installers (DirectX, VC++),
/// which arc-unpack doesn't run, are left out.
pub fn choices(setup: &Setup, plan: &Plan, repack: &Path) -> Vec<Choice> {
    let files = repack_files(repack);
    let defaults = default_components(setup, &files);
    let mut out: Vec<Choice> = Vec::new();
    for c in setup.components.iter().filter(|c| crate::install::english_or_neutral(&c.languages)) {
        let name = c.name.to_ascii_lowercase();
        if out.iter().any(|x| x.name == name) || !(c.fixed || plan.queried.contains(&name)) {
            continue;
        }
        let file = files.iter().find(|f| part_file_matches(c, f)).cloned();
        let label = if c.description.trim().is_empty() || c.description.starts_with("{cm:") { c.name.clone() } else { c.description.clone() };
        out.push(Choice { name: name.clone(), label, fixed: c.fixed, exclusive: c.exclusive, default: defaults.contains(&name), file });
    }
    out
}

/// What a plan needs from the repack folder.
#[derive(Debug, Clone, Default)]
pub struct Preflight {
    /// Repack archives the script extracts, in order of first use.
    pub needed: Vec<PathBuf>,
    /// Of those, the ones that aren't there.
    pub missing: Vec<String>,
}

pub fn preflight(plan: &Plan, repack: &Path) -> Preflight {
    let src = std::path::absolute(repack).unwrap_or_else(|_| repack.to_path_buf());
    let mut out = Preflight::default();
    for step in &plan.steps {
        if let Step::Extract { archive, .. } = step
            && starts_with_ci(archive, &src)
            && !out.needed.contains(archive)
        {
            if !archive.is_file() {
                out.missing.push(archive.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
            }
            out.needed.push(archive.clone());
        }
    }
    out
}

/// A desktop shortcut the installer would create.
#[derive(Debug, Clone, PartialEq)]
pub struct Shortcut {
    /// File name without `.lnk`, e.g. "Some Game".
    pub name: String,
    pub target: PathBuf,
    pub args: String,
    pub working_dir: PathBuf,
}

/// The installer's desktop shortcuts for this selection, pointing into `app`.
pub fn desktop_shortcuts(setup: &Setup, selected: &BTreeSet<String>, app: &Path) -> Vec<Shortcut> {
    let expand = |s: &str| -> Option<PathBuf> {
        let rest = s.strip_prefix("{app}")?;
        Some(app.join(rest.trim_start_matches('\\')))
    };
    setup
        .icons
        .iter()
        .filter(|i| {
            let n = i.name.to_ascii_lowercase();
            (n.starts_with("{commondesktop}") || n.starts_with("{userdesktop}") || n.starts_with("{desktop}"))
                && crate::install::english_or_neutral(&i.languages)
                && (i.components.is_empty() || eval_components(&i.components, selected).unwrap_or(false))
                && (i.tasks.is_empty() || i.tasks.to_ascii_lowercase().contains("desktop"))
        })
        .filter_map(|i| {
            let target = expand(&i.filename)?;
            let working_dir = if i.working_dir.is_empty() { target.parent()?.to_path_buf() } else { expand(&i.working_dir)? };
            let name = i.name.rsplit('\\').next()?.to_string();
            Some(Shortcut { name, target, args: i.parameters.clone(), working_dir })
        })
        .collect()
}

/// Creates the shortcuts on the current user's desktop. Returns the .lnk paths.
#[cfg(windows)]
pub fn create_desktop_shortcuts(shortcuts: &[Shortcut]) -> Result<Vec<PathBuf>> {
    create_shortcuts(shortcuts, None)
}

/// Creates the shortcuts in `dir` (None: the current user's desktop). Returns the .lnk paths.
#[cfg(windows)]
pub fn create_shortcuts(shortcuts: &[Shortcut], dir: Option<&Path>) -> Result<Vec<PathBuf>> {
    let q = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let dir = match dir {
        Some(d) => d.to_path_buf(),
        None => desktop_dir()?,
    };
    let mut script = String::from("$ErrorActionPreference='Stop'; $w = New-Object -ComObject WScript.Shell; ");
    let mut out = Vec::new();
    for sc in shortcuts {
        let file = dir.join(format!("{}.lnk", sc.name.replace(['\\', '/', ':', '*', '?', '"', '<', '>', '|'], "_")));
        script.push_str(&format!(
            "$s = $w.CreateShortcut({}); $s.TargetPath = {}; $s.Arguments = {}; $s.WorkingDirectory = {}; $s.Save(); ",
            q(&file.to_string_lossy()),
            q(&sc.target.to_string_lossy()),
            q(&sc.args),
            q(&sc.working_dir.to_string_lossy())
        ));
        out.push(file);
    }
    let status = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .status()
        .context("start PowerShell")?;
    ensure!(status.success(), "creating the shortcut failed");
    Ok(out)
}

/// The current user's Desktop folder (it can be redirected, e.g. to OneDrive).
#[cfg(windows)]
fn desktop_dir() -> Result<PathBuf> {
    let out = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", "[Environment]::GetFolderPath('Desktop')"])
        .creation_flags(0x0800_0000)
        .output()
        .context("start PowerShell")?;
    let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
    ensure!(!dir.is_empty(), "no Desktop folder");
    Ok(PathBuf::from(dir))
}

#[cfg(not(windows))]
pub fn create_desktop_shortcuts(_: &[Shortcut]) -> Result<Vec<PathBuf>> {
    bail!("shortcuts are only supported on Windows")
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
    fn downloaded_optional_parts() {
        let files: Vec<String> = ["fg-01.bin", "fg-optional-bonus-content.bin", "fg-selective-french.bin", "fg-optional-spanish-vo.bin", "setup.exe"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let c = |name: &str, description: &str| crate::inno::Component { description: description.into(), ..component(name, false) };
        assert!(downloaded_part(&c("bonus", "Bonus Content"), &files));
        assert!(downloaded_part(&c(r"voice\french", ""), &files));
        assert!(!downloaded_part(&c(r"voice\german", "German"), &files));
        assert!(!downloaded_part(&c("text", "Main game files"), &files), "main parts aren't 'downloaded optional parts'");
        assert!(!downloaded_part(&c("fr", ""), &files), "names this short would match too much");
        // FitGirl's short codes don't always appear in the file name; the language does.
        assert!(downloaded_part(&c(r"local\fra", "French"), &files));
        assert!(downloaded_part(&c(r"local\esp", "Spanish"), &files));
        assert!(!downloaded_part(&c(r"local\ita", "Italian"), &files));
        assert!(!downloaded_part(&c("credits", "Credits Video"), &files), "only one-word descriptions are matched");
        // Sleeping Dogs: component "le", archive fg-optional-limited-edition.bin.
        let sd: Vec<String> = ["fg-01.bin", "fg-optional-limited-edition.bin"].iter().map(|s| s.to_string()).collect();
        assert!(downloaded_part(&c("le", "Install Limited Edition as well"), &sd));
        assert!(downloaded_part(&c("lim", "Install Limited Edition as well"), &sd), "the phrase alone");
        assert!(downloaded_part(&c("le", ""), &sd), "the initials alone");
        assert!(!downloaded_part(&c("text", "Definitive Edition"), &sd));
    }

    fn component(name: &str, fixed: bool) -> crate::inno::Component {
        crate::inno::Component { name: name.into(), description: String::new(), types: "full".into(), languages: String::new(), fixed, exclusive: false }
    }

    #[test]
    fn parts_follow_the_selection() {
        let comps = [component("text", true), component("bonus", false), component(r"voice\french", false), component("directx", false)];
        let files: Vec<String> = ["fg-01.bin", "fg-optional-bonus-content.bin", "fg-selective-french.bin", "fg-optional-extra.bin"].iter().map(|s| s.to_string()).collect();
        let sel = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<BTreeSet<String>>();
        // Everything ticked: nothing left out, a normal install.
        let p = parts_of(&comps, &files, &sel(&["text", "bonus", r"voice\french"]));
        assert_eq!(p, Parts::default());
        // An unticked part's archive is left out; an archive no component claims stays in.
        let p = parts_of(&comps, &files, &sel(&["text", r"voice\french"]));
        assert_eq!(p.skip, sel(&["fg-optional-bonus-content.bin"]));
        assert!(!p.only_optional);
        assert!(p.skips("FG-OPTIONAL-BONUS-CONTENT.BIN") && !p.skips("fg-01.bin") && !p.skips("fg-optional-extra.bin"));
        // Base game unticked: only the ticked parts are added; the base game's archives are left out.
        let p = parts_of(&comps, &files, &sel(&["bonus"]));
        assert!(p.only_optional);
        assert!(p.skips("fg-01.bin") && !p.skips("fg-optional-bonus-content.bin") && p.skips("fg-selective-french.bin"));
        // An installer without a fixed component has no base game to leave out.
        let p = parts_of(&[component("text", false), component("bonus", false)], &files, &sel(&["bonus"]));
        assert!(!p.only_optional);
    }

    #[test]
    fn a_ticked_group_gets_its_first_choice() {
        let pick = |name: &str| crate::inno::Component { exclusive: true, ..component(name, false) };
        let comps = [component("text", true), component("local", false), pick(r"local\eng"), pick(r"local\rus"), component("directx", false)];
        let sel = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<BTreeSet<String>>();
        // Ancient Wars: Sparta HD with /COMPONENTS=text,local: Inno Setup installs English.
        assert_eq!(exclusive_choices_of(&comps, sel(&["text", "local"])), sel(&["text", "local", r"local\eng"]));
        // Of several ticked choices only the first stays, like Inno's radio buttons.
        assert_eq!(exclusive_choices_of(&comps, sel(&["text", "local", r"local\rus", r"local\eng"])), sel(&["text", "local", r"local\eng"]));
        // A choice already made stays; an unticked group stays empty.
        assert_eq!(exclusive_choices_of(&comps, sel(&["text", "local", r"local\rus"])), sel(&["text", "local", r"local\rus"]));
        assert_eq!(exclusive_choices_of(&comps, sel(&["text"])), sel(&["text"]));
        // Ordinary (non-exclusive) children aren't ticked by their parent.
        let comps = [component("voice", false), component(r"voice\french", false)];
        assert_eq!(exclusive_choices_of(&comps, sel(&["voice"])), sel(&["voice"]));
    }

    #[test]
    fn uninstaller_is_recognised() {
        for yes in [r"C:\Games\X\unins000.exe", "unins001.exe", r#""C:\Games\X\UNINS000.EXE""#] {
            assert!(is_uninstaller(yes), "{yes}");
        }
        for no in ["uninstall.exe", "unins.exe", "unins000.dat", "x2.exe", r"C:\Games\unins000\game.exe"] {
            assert!(!is_uninstaller(no), "{no}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn shortcut_files_are_created() {
        let dir = std::env::temp_dir().join(format!("arcunp-lnk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = std::env::current_exe().unwrap();
        let sc = Shortcut {
            name: "Game's \"Test\": 2".into(),
            target: target.clone(),
            args: "-x 'quoted'".into(),
            working_dir: target.parent().unwrap().to_path_buf(),
        };
        let made = create_shortcuts(&[sc], Some(&dir)).unwrap();
        assert_eq!(made.len(), 1);
        assert!(made[0].is_file(), "{} missing", made[0].display());
        assert!(made[0].file_name().unwrap().to_string_lossy().ends_with(".lnk"));
        let _ = std::fs::remove_dir_all(&dir);
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
