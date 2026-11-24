//! codecs.toml: how each FreeArc method name is decoded.
//!
//! Resolution order for a method name: [methods.<name>] in the config, then
//! `<codec_dir>/cls-<name>.dll`, then the built-ins (storing, lzma).

use crate::lzma::LzmaParams;
use crate::method::{Method, parse_chain};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Folder with codec executables / DLLs. Relative program names resolve against it, and
    /// it is the default working directory for codec processes. Relative to the config file.
    pub codec_dir: Option<PathBuf>,
    /// FreeArc's own unarc.dll (32-bit). Archives with a method that has no other decoder
    /// (FreeArc built-ins such as delta, dispack, 4x4, tor) are extracted whole through it.
    pub unarc: Option<PathBuf>,
    /// arc.ini handed to unarc.dll (default: codec_dir/arc.ini when it exists).
    pub arc_ini: Option<PathBuf>,
    #[serde(default)]
    pub methods: BTreeMap<String, MethodSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MethodSpec {
    /// External decoder command line. Placeholders:
    ///   {input}   temp file with the compressed data   (absent = data is piped to stdin)
    ///   {output}  temp file the decoder must create    (absent = stdout is read)
    ///   {options} FreeArc style: "-p1 -p2 " for method params p1, p2 ("srep:m3yf" -> "-m3yf ")
    ///   {X option Y}  FreeArc style: "Xp1Y Xp2Y" (e.g. "{-option }", "{option}", "-o{option}")
    ///   {params}  params joined with ':' ("m3yf")
    ///   {codec_dir}
    /// With no params, {options}/{X option Y} become `default`.
    pub cmd: Option<String>,
    /// A FreeArc CLS codec DLL, run through cls-host-x86.exe / cls-host-x64.exe.
    pub cls: Option<PathBuf>,
    /// Replace this method with another chain, e.g. `alias = "lolz:dtb1"`.
    pub alias: Option<String>,
    /// FreeArc's per-stage flag byte (arc.ini `header`, default on): the stage's data starts with
    /// 1 = "run the decoder on the rest" or 0 = "the rest is stored raw".
    #[serde(default = "yes")]
    pub header: bool,
    /// Text used for {options} when the method has no params (arc.ini `default`).
    #[serde(default)]
    pub default: String,
    /// Working directory for the process (default: codec_dir).
    pub workdir: Option<PathBuf>,
    /// Keep this codec on the fastest core class (P-cores on hybrid CPUs).
    #[serde(default)]
    pub performance_cores: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone)]
pub enum Codec {
    Storing,
    Lzma(LzmaParams),
    External(External),
    Cls { dll: PathBuf, method: String, performance_cores: bool },
}

#[derive(Debug, Clone)]
pub struct External {
    pub argv: Vec<String>,
    pub workdir: PathBuf,
    pub input_file: bool,
    pub output_file: bool,
    pub header: bool,
    pub performance_cores: bool,
}

#[derive(Debug, Clone)]
pub struct Stage {
    pub method: Method,
    pub codec: Codec,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        let base = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let dir = match cfg.codec_dir.take() {
            Some(d) if d.is_relative() => base.join(d),
            Some(d) => d,
            None => base.to_path_buf(),
        };
        // Absolute, because codec processes run with their working directory set to it.
        cfg.codec_dir = Some(std::path::absolute(&dir).with_context(|| format!("codec_dir {}", dir.display()))?);
        Ok(cfg)
    }

    pub fn unarc_dll(&self) -> Option<PathBuf> {
        let p = self.unarc.as_ref()?;
        Some(if p.is_relative() { self.codec_dir().join(p) } else { p.clone() })
    }

    pub fn arc_ini_path(&self) -> Option<PathBuf> {
        match &self.arc_ini {
            Some(p) if p.is_relative() => Some(self.codec_dir().join(p)),
            Some(p) => Some(p.clone()),
            None => Some(self.codec_dir().join("arc.ini")).filter(|p| p.is_file()),
        }
    }

    pub fn codec_dir(&self) -> PathBuf {
        self.codec_dir.clone().unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }

    /// Resolves a block's method chain into stages, in *compression* order (decode runs them reversed).
    pub fn resolve(&self, chain: &str) -> Result<Vec<Stage>> {
        let mut out = Vec::new();
        for m in parse_chain(chain) {
            self.resolve_into(m, &mut out, 0)?;
        }
        Ok(out)
    }

    /// Lists methods in `chain` that have no usable decoder, with the reason.
    pub fn missing(&self, chain: &str) -> Vec<String> {
        parse_chain(chain)
            .into_iter()
            .filter_map(|m| {
                let mut v = Vec::new();
                self.resolve_into(m.clone(), &mut v, 0).err().map(|e| format!("{}: {e:#}", m.full()))
            })
            .collect()
    }

    fn resolve_into(&self, m: Method, out: &mut Vec<Stage>, depth: usize) -> Result<()> {
        ensure!(depth < 16, "alias loop at '{}'", m.name);
        let dir = self.codec_dir();
        if let Some(spec) = self.methods.get(&m.name) {
            if let Some(alias) = &spec.alias {
                for am in parse_chain(alias) {
                    self.resolve_into(am, out, depth + 1)?;
                }
                return Ok(());
            }
            let codec = match (&spec.cmd, &spec.cls) {
                (Some(cmd), None) => Codec::External(external(cmd, spec, &m, &dir)?),
                (None, Some(dll)) => {
                    let dll = if dll.is_relative() { dir.join(dll) } else { dll.clone() };
                    ensure!(dll.is_file(), "CLS dll not found: {}", dll.display());
                    Codec::Cls { dll, method: m.full(), performance_cores: spec.performance_cores }
                }
                (None, None) => bail!("[methods.{}] needs one of cmd / cls / alias", m.name),
                (Some(_), Some(_)) => bail!("[methods.{}] has both cmd and cls", m.name),
            };
            out.push(Stage { method: m, codec });
            return Ok(());
        }
        let dll = dir.join(format!("cls-{}.dll", m.name));
        let codec = if dll.is_file() {
            Codec::Cls { dll, method: m.full(), performance_cores: false }
        } else {
            match m.name.as_str() {
                "storing" => Codec::Storing,
                "lzma" => Codec::Lzma(LzmaParams::from_method(&m)),
                _ => bail!("no decoder (add [methods.{}] to the config or put cls-{}.dll in {})", m.name, m.name, dir.display()),
            }
        };
        out.push(Stage { method: m, codec });
        Ok(())
    }
}

/// FreeArc's {options} / {X option Y} substitution (C_External.cpp `prepare_cmd`): only the
/// first template in the command is replaced.
pub fn expand_options(cmd: &str, params: &[String], default: &str) -> String {
    let (start, end, before, after) = if let Some(i) = cmd.find("{options}") {
        (i, i + "{options}".len(), "-".to_string(), " ".to_string())
    } else {
        let mut found = None;
        let mut search = 0;
        while let Some(o) = cmd[search..].find('{').map(|o| o + search) {
            let Some(c) = cmd[o..].find('}').map(|c| c + o) else { break };
            let inner = &cmd[o + 1..c];
            if let Some(k) = inner.find("option") {
                found = Some((o, c + 1, inner[..k].to_string(), inner[k + "option".len()..].to_string()));
                break;
            }
            search = o + 1;
        }
        match found {
            Some(f) => f,
            None => return cmd.to_string(),
        }
    };
    let replacement = if params.is_empty() {
        default.to_string()
    } else {
        params.iter().map(|p| format!("{before}{p}{after}")).collect()
    };
    format!("{}{}{}", &cmd[..start], replacement, &cmd[end..])
}

pub fn split_cmdline(s: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut have = false;
    for ch in s.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                have = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if have {
                    args.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            c => {
                cur.push(c);
                have = true;
            }
        }
    }
    if have {
        args.push(cur);
    }
    args
}

fn external(cmd: &str, spec: &MethodSpec, m: &Method, dir: &Path) -> Result<External> {
    let line = expand_options(cmd, &m.params, &spec.default)
        .replace("{params}", &m.params_str())
        .replace("{codec_dir}", &dir.to_string_lossy());
    let mut argv: Vec<String> = split_cmdline(&line).into_iter().filter(|a| a != "<stdin>" && a != "<stdout>").collect();
    ensure!(!argv.is_empty(), "[methods.{}] cmd is empty", m.name);
    let input_file = argv.iter().any(|a| a.contains("{input}"));
    let output_file = argv.iter().any(|a| a.contains("{output}"));
    // Like CreateProcess (which searches the host program's folder first), a bare program
    // name is looked up in codec_dir.
    let prog = Path::new(&argv[0]);
    if prog.is_relative() {
        for cand in [dir.join(prog), dir.join(format!("{}.exe", argv[0]))] {
            if cand.is_file() {
                argv[0] = cand.to_string_lossy().into_owned();
                break;
            }
        }
    }
    let workdir = match &spec.workdir {
        Some(w) if w.is_relative() => dir.join(w),
        Some(w) => w.clone(),
        None => dir.to_path_buf(),
    };
    Ok(External { argv, workdir, input_file, output_file, header: spec.header, performance_cores: spec.performance_cores })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn freearc_option_templates() {
        assert_eq!(expand_options("srep {options}-d - -", &p(&["m3yf"]), ""), "srep -m3yf -d - -");
        assert_eq!(expand_options("x {-option } y", &p(&["a", "b"]), ""), "x -a -b  y");
        assert_eq!(expand_options("x {option} y", &p(&["m3"]), ""), "x m3 y");
        assert_eq!(expand_options("x -o{option} y", &p(&["5"]), ""), "x -o5 y");
        assert_eq!(expand_options("x {options}y", &[], "-m1 "), "x -m1 y");
        assert_eq!(expand_options("no template", &p(&["a"]), ""), "no template");
    }

    #[test]
    fn resolve_builtin_alias_and_cmd() {
        let cfg: Config = toml::from_str(
            r#"
            [methods.magic2]
            alias = "lzma:d4m"
            [methods.srep]
            cmd = 'srep.exe {options}-d {input} {output}'
            header = false
            "#,
        )
        .unwrap();
        let st = cfg.resolve("srep:m3yf+magic2").unwrap();
        assert_eq!(st.len(), 2);
        match &st[0].codec {
            Codec::External(e) => {
                assert_eq!(e.argv, p(&["srep.exe", "-m3yf", "-d", "{input}", "{output}"]));
                assert!(e.input_file && e.output_file && !e.header);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(st[1].codec, Codec::Lzma(p) if p.dict == 4 << 20));
        let missing = cfg.missing("xt3uo+srep");
        assert_eq!(missing.len(), 1);
        assert!(missing[0].starts_with("xt3uo: no decoder"));
    }

    #[test]
    fn stdin_stdout_markers_are_dropped() {
        let cfg: Config = toml::from_str("[methods.x]\ncmd = 'x.exe -d - - <stdin> <stdout>'").unwrap();
        match &cfg.resolve("x").unwrap()[0].codec {
            Codec::External(e) => {
                assert_eq!(e.argv, p(&["x.exe", "-d", "-", "-"]));
                assert!(!e.input_file && !e.output_file && e.header);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cmdline_quotes() {
        assert_eq!(split_cmdline(r#"a "b c" d"#), vec!["a", "b c", "d"]);
        assert_eq!(split_cmdline(r#"x """#), vec!["x", ""]);
    }
}
