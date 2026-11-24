//! Interpreter for RemObjects Pascal Script (IFPS) bytecode: the form Inno Setup compiles an
//! installer's `[Code]` section into. Repack installers do their real work there (which
//! archives to extract, in what order, which tools to run on the results), so running that
//! code, with arc-unpack supplying the functions it calls, reproduces their install exactly.
//!
//! Covers the instruction set and value model the RemObjects runtime (uPSRuntime) defines;
//! try/except blocks are not supported. External functions are resolved by name through
//! [`Host`]; anything the host does not know stops the script with a clear error.

use anyhow::{Context, Result, bail, ensure};

const MAGIC: u32 = 0x5350_4649; // "IFPS"
/// Variable numbers at or above this address the stack, relative to the current frame.
const STACK_START: u32 = 0x6000_0000;
const NEGATIVE_STACK_START: u32 = 0x4000_0000;
/// Stop runaway scripts (an install procedure runs well under a million instructions).
const MAX_STEPS: u64 = 200_000_000;
const MAX_DEPTH: usize = 256;

/// Base type numbers of the compiled format.
pub mod bt {
    pub const RETURN_ADDRESS: u8 = 0;
    pub const U8: u8 = 1;
    pub const S8: u8 = 2;
    pub const U16: u8 = 3;
    pub const S16: u8 = 4;
    pub const U32: u8 = 5;
    pub const S32: u8 = 6;
    pub const SINGLE: u8 = 7;
    pub const DOUBLE: u8 = 8;
    pub const EXTENDED: u8 = 9;
    pub const STRING: u8 = 10;
    pub const RECORD: u8 = 11;
    pub const ARRAY: u8 = 12;
    pub const POINTER: u8 = 13;
    pub const PCHAR: u8 = 14;
    pub const RESOURCE_POINTER: u8 = 15;
    pub const VARIANT: u8 = 16;
    pub const S64: u8 = 17;
    pub const CHAR: u8 = 18;
    pub const WIDE_STRING: u8 = 19;
    pub const WIDE_CHAR: u8 = 20;
    pub const PROC_PTR: u8 = 21;
    pub const STATIC_ARRAY: u8 = 22;
    pub const SET: u8 = 23;
    pub const CURRENCY: u8 = 24;
    pub const CLASS: u8 = 25;
    pub const INTERFACE: u8 = 26;
    pub const NOTIFICATION_VARIANT: u8 = 27;
    pub const UNICODE_STRING: u8 = 28;
}

#[derive(Debug, Clone)]
pub enum TypeInfo {
    Plain,
    Record(Vec<u32>),
    Array(u32),
    StaticArray(u32, u32),
    Set(u32),
    Named(String),
}

#[derive(Debug, Clone)]
pub struct Type {
    pub base: u8,
    pub info: TypeInfo,
}

#[derive(Debug, Clone)]
pub enum Proc {
    Internal { name: Option<String>, start: usize, len: usize },
    /// `key` is the name the host resolves: the import name, or for DLL imports
    /// `dll:<function>`, or for class members `class:CLASS|MEMBER|` (`@` marks a setter).
    External { key: String },
}

/// A loaded script.
pub struct Script {
    pub types: Vec<Type>,
    pub procs: Vec<Proc>,
    globals: Vec<u32>,
    code: Vec<u8>,
}

struct Bytes<'a> {
    b: &'a [u8],
    o: usize,
}

impl Bytes<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let v = self.b.get(self.o..self.o + n).context("script truncated")?;
        self.o += n;
        Ok(v)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bstr(&mut self) -> Result<&[u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}

fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

impl Script {
    pub fn load(code: &[u8]) -> Result<Script> {
        let mut r = Bytes { b: code, o: 0 };
        ensure!(r.u32()? == MAGIC, "not a compiled Pascal Script");
        let build = r.u32()?;
        ensure!((20..=23).contains(&build), "unsupported Pascal Script build {build}");
        let (ntypes, nprocs, nvars) = (r.u32()?, r.u32()?, r.u32()?);
        let _main = r.u32()?;
        let _import_table = r.u32()?;
        let mut script = Script { types: Vec::new(), procs: Vec::new(), globals: Vec::new(), code: code.to_vec() };
        for _ in 0..ntypes {
            let flags = r.u8()?;
            let base = flags & 127;
            let info = match base {
                bt::CLASS | bt::PROC_PTR => TypeInfo::Named(latin1(r.bstr()?)),
                bt::INTERFACE => {
                    r.take(16)?;
                    TypeInfo::Plain
                }
                bt::ARRAY => TypeInfo::Array(r.u32()?),
                bt::STATIC_ARRAY => {
                    let (t, n) = (r.u32()?, r.u32()?);
                    if build >= 21 {
                        r.u32()?; // start offset
                    }
                    TypeInfo::StaticArray(t, n)
                }
                bt::RECORD => {
                    let n = r.u32()?;
                    TypeInfo::Record((0..n).map(|_| r.u32()).collect::<Result<_>>()?)
                }
                bt::SET => TypeInfo::Set(r.u32()?),
                _ => TypeInfo::Plain,
            };
            if flags & 128 != 0 {
                r.bstr()?; // exported name
            }
            script.types.push(Type { base, info });
            if build >= 21 {
                script.skip_attributes(&mut r)?;
            }
        }
        for _ in 0..nprocs {
            let flags = r.u8()?;
            let proc = if flags & 1 != 0 {
                let n = r.u8()? as usize;
                let name = latin1(r.take(n)?);
                let decl = if flags & 3 == 3 { r.bstr()?.to_vec() } else { Vec::new() };
                Proc::External { key: external_key(&name, &decl) }
            } else {
                let (start, len) = (r.u32()? as usize, r.u32()? as usize);
                ensure!(start + len <= code.len(), "procedure outside the script");
                let name = if flags & 2 != 0 {
                    let n = latin1(r.bstr()?);
                    r.bstr()?; // declaration
                    Some(n)
                } else {
                    None
                };
                Proc::Internal { name, start, len }
            };
            if flags & 4 != 0 {
                script.skip_attributes(&mut r)?;
            }
            script.procs.push(proc);
        }
        for _ in 0..nvars {
            let t = r.u32()?;
            ensure!((t as usize) < script.types.len(), "global of unknown type");
            if r.u8()? & 1 != 0 {
                r.bstr()?;
            }
            script.globals.push(t);
        }
        Ok(script)
    }

    fn skip_attributes(&self, r: &mut Bytes) -> Result<()> {
        for _ in 0..r.u32()? {
            r.bstr()?;
            for _ in 0..r.u32()? {
                let t = r.u32()?;
                self.read_const(r, t)?;
            }
        }
        Ok(())
    }

    pub fn find(&self, name: &str) -> Option<u32> {
        self.procs
            .iter()
            .position(|p| matches!(p, Proc::Internal { name: Some(n), .. } if n.eq_ignore_ascii_case(name)))
            .map(|i| i as u32)
    }

    fn base(&self, t: u32) -> u8 {
        self.types.get(t as usize).map(|t| t.base).unwrap_or(bt::VARIANT)
    }

    fn read_const(&self, r: &mut Bytes, t: u32) -> Result<Value> {
        let ty = self.types.get(t as usize).context("constant of unknown type")?;
        Ok(match ty.base {
            bt::U8 => Value::Int(r.u8()? as i64),
            bt::S8 => Value::Int(r.u8()? as i8 as i64),
            bt::CHAR => Value::Str((r.u8()? as char).to_string()),
            bt::U16 => Value::Int(r.u16()? as i64),
            bt::S16 => Value::Int(r.u16()? as i16 as i64),
            bt::WIDE_CHAR => Value::Str(char::from_u32(r.u16()? as u32).unwrap_or('?').to_string()),
            bt::U32 => Value::Int(r.u32()? as i64),
            bt::S32 => Value::Int(r.i32()? as i64),
            bt::S64 => Value::Int(r.u64()? as i64),
            bt::SINGLE => Value::Float(f32::from_le_bytes(r.take(4)?.try_into().unwrap()) as f64),
            bt::DOUBLE => Value::Float(f64::from_le_bytes(r.take(8)?.try_into().unwrap())),
            bt::CURRENCY => Value::Float(r.u64()? as i64 as f64 / 10_000.0),
            bt::EXTENDED => Value::Float(extended_to_f64(r.take(10)?)),
            bt::STRING | bt::PCHAR => Value::Str(latin1(r.bstr()?)),
            bt::WIDE_STRING | bt::UNICODE_STRING => {
                let n = r.u32()? as usize;
                let units: Vec<u16> = r.take(2 * n)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                Value::Str(String::from_utf16_lossy(&units))
            }
            bt::PROC_PTR => Value::Proc(r.u32()?),
            bt::SET => {
                let TypeInfo::Set(bits) = ty.info else { bail!("bad set type") };
                Value::Set(r.take(bits.div_ceil(8) as usize)?.to_vec())
            }
            b => bail!("constant of type {b} is not supported"),
        })
    }

    fn default_value(&self, t: u32) -> Value {
        let Some(ty) = self.types.get(t as usize) else { return Value::Nil };
        match (ty.base, &ty.info) {
            (bt::U8 | bt::S8 | bt::U16 | bt::S16 | bt::U32 | bt::S32 | bt::S64, _) => Value::Int(0),
            (bt::SINGLE | bt::DOUBLE | bt::EXTENDED | bt::CURRENCY, _) => Value::Float(0.0),
            (bt::STRING | bt::PCHAR | bt::WIDE_STRING | bt::UNICODE_STRING, _) => Value::Str(String::new()),
            (bt::CHAR | bt::WIDE_CHAR, _) => Value::Str("\0".into()),
            (bt::RECORD, TypeInfo::Record(fields)) => Value::Rec(fields.iter().map(|&f| self.default_value(f)).collect()),
            (bt::STATIC_ARRAY, TypeInfo::StaticArray(e, n)) => Value::Arr((0..*n).map(|_| self.default_value(*e)).collect()),
            (bt::ARRAY, _) => Value::Arr(Vec::new()),
            (bt::SET, TypeInfo::Set(bits)) => Value::Set(vec![0; bits.div_ceil(8) as usize]),
            (bt::CLASS | bt::INTERFACE, _) => Value::Obj(0),
            (bt::PROC_PTR, _) => Value::Proc(u32::MAX),
            _ => Value::Nil,
        }
    }

    /// Element type of a record field / array element, for conversions on assignment.
    fn child_type(&self, t: u32, index: usize) -> Option<u32> {
        match &self.types.get(t as usize)?.info {
            TypeInfo::Record(f) => f.get(index).copied(),
            TypeInfo::Array(e) | TypeInfo::StaticArray(e, _) => Some(*e),
            _ => None,
        }
    }
}

fn external_key(name: &str, decl: &[u8]) -> String {
    if !name.is_empty() {
        return name.to_ascii_uppercase();
    }
    let parts: Vec<&[u8]> = decl.split(|&b| b == 0).collect();
    let first = latin1(parts.first().copied().unwrap_or_default());
    if first.starts_with("dll:") {
        let func = latin1(parts.get(1).copied().unwrap_or_default());
        format!("dll:{}", func.trim().to_ascii_uppercase())
    } else {
        first.trim().to_ascii_uppercase()
    }
}

fn extended_to_f64(b: &[u8]) -> f64 {
    let mant = u64::from_le_bytes(b[..8].try_into().unwrap());
    let se = u16::from_le_bytes([b[8], b[9]]);
    if mant == 0 {
        return 0.0;
    }
    let sign = if se & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = (se & 0x7FFF) as i32 - 16383 - 63;
    sign * (mant as f64) * 2f64.powi(exp)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Str(String),
    Rec(Vec<Value>),
    Arr(Vec<Value>),
    /// A class instance handle (0 = nil). The host decides what handles mean.
    Obj(u32),
    Proc(u32),
    Set(Vec<u8>),
    Ref(Ref),
    Ret(RetAddr),
    Nil,
}

impl Value {
    pub fn as_str(&self) -> String {
        match self {
            Value::Str(s) => s.clone(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => f.to_string(),
            _ => String::new(),
        }
    }
    pub fn as_int(&self) -> i64 {
        match self {
            Value::Int(i) => *i,
            Value::Float(f) => *f as i64,
            Value::Obj(o) => *o as i64,
            Value::Str(s) => s.chars().next().map(|c| c as i64).unwrap_or(0),
            _ => 0,
        }
    }
    pub fn as_bool(&self) -> bool {
        self.as_int() != 0
    }
    fn as_float(&self) -> f64 {
        match self {
            Value::Float(f) => *f,
            v => v.as_int() as f64,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ref {
    root: Root,
    path: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Root {
    Global(usize),
    Stack(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub struct RetAddr {
    proc: Option<u32>,
    pos: usize,
    base: usize,
}

#[derive(Clone)]
struct Slot {
    ty: Option<u32>,
    v: Value,
}

enum Operand {
    Var(Ref),
    Const(Value),
}

/// What the interpreter needs from its embedder: the external functions.
pub trait Host {
    /// (number of parameters, has a result) for an external, or None if it isn't provided.
    fn signature(&self, key: &str) -> Option<(usize, bool)>;
    /// Runs an external. `args[0]` is the first declared parameter (var parameters are passed
    /// by value; set `out[i]` to write one back). Returns the result for functions.
    fn call(&mut self, key: &str, args: &[Value], out: &mut Vec<(usize, Value)>) -> Result<Option<Value>>;
}

pub struct Vm<'s> {
    s: &'s Script,
    globals: Vec<Slot>,
    stack: Vec<Slot>,
    proc: Option<u32>,
    pos: usize,
    base: usize,
    flag: bool,
    steps: u64,
}

impl<'s> Vm<'s> {
    pub fn new(s: &'s Script) -> Vm<'s> {
        let globals = s.globals.iter().map(|&t| Slot { ty: Some(t), v: s.default_value(t) }).collect();
        Vm { s, globals, stack: Vec::new(), proc: None, pos: 0, base: 0, flag: false, steps: 0 }
    }

    /// Calls a script procedure (e.g. `CurStepChanged`) with plain arguments. Parameters are
    /// pushed as untyped values, which is fine for the ordinal/string arguments Inno passes.
    pub fn call(&mut self, name: &str, args: &[Value], host: &mut dyn Host) -> Result<()> {
        let p = self.s.find(name).with_context(|| format!("the script has no procedure {name}"))?;
        for a in args.iter().rev() {
            self.stack.push(Slot { ty: None, v: a.clone() });
        }
        let depth = self.stack.len();
        self.enter(p)?;
        self.run(host).with_context(|| format!("in the installer script's {name}"))?;
        self.stack.truncate(depth - args.len());
        Ok(())
    }

    fn enter(&mut self, p: u32) -> Result<()> {
        let Proc::Internal { start, .. } = self.s.procs[p as usize] else { bail!("not a script procedure") };
        let depth = self.stack.iter().filter(|s| matches!(s.v, Value::Ret(_))).count();
        ensure!(depth < MAX_DEPTH, "script recursion too deep");
        self.stack.push(Slot { ty: None, v: Value::Ret(RetAddr { proc: self.proc, pos: self.pos, base: self.base }) });
        self.base = self.stack.len() - 1;
        self.proc = Some(p);
        self.pos = start;
        Ok(())
    }

    fn code_end(&self) -> usize {
        match self.proc.map(|p| &self.s.procs[p as usize]) {
            Some(Proc::Internal { start, len, .. }) => start + len,
            _ => 0,
        }
    }

    fn proc_start(&self) -> usize {
        match self.proc.map(|p| &self.s.procs[p as usize]) {
            Some(Proc::Internal { start, .. }) => *start,
            _ => 0,
        }
    }

    fn run(&mut self, host: &mut dyn Host) -> Result<()> {
        let s = self.s;
        loop {
            self.steps += 1;
            ensure!(self.steps < MAX_STEPS, "the script ran too long");
            ensure!(self.pos < self.code_end(), "ran past the end of a procedure");
            let at = self.pos;
            let mut r = Bytes { b: &s.code, o: self.pos };
            let op = r.u8()?;
            let res: Result<bool> = (|| {
                match op {
                    0 => {
                        let d = self.operand(&mut r)?;
                        let v = self.operand(&mut r)?;
                        let v = self.value_of(&v)?;
                        self.assign(&d, v)?;
                    }
                    1 => {
                        let o = r.u8()?;
                        let d = self.operand(&mut r)?;
                        let v = self.operand(&mut r)?;
                        let (a, b) = (self.value_of(&d)?, self.value_of(&v)?);
                        self.assign(&d, calc(o, a, b)?)?;
                    }
                    2 => {
                        let v = self.operand(&mut r)?;
                        let ty = self.type_of(&v);
                        let v = self.value_of(&v)?;
                        self.stack.push(Slot { ty, v });
                    }
                    3 => {
                        let Operand::Var(rf) = self.operand(&mut r)? else { bail!("push of a constant by reference") };
                        let rf = self.resolve(rf)?;
                        self.stack.push(Slot { ty: None, v: Value::Ref(rf) });
                    }
                    4 => {
                        ensure!(self.stack.len() > self.base + 1, "pop past the frame");
                        self.stack.pop();
                    }
                    5 => {
                        let p = r.u32()?;
                        self.pos = r.o;
                        return self.call_proc(p, host);
                    }
                    6 => {
                        let d = r.i32()?;
                        r.o = jump(r.o, d)?;
                    }
                    7 | 8 => {
                        let d = r.i32()?;
                        let v = self.operand(&mut r)?;
                        if self.value_of(&v)?.as_bool() == (op == 7) {
                            r.o = jump(r.o, d)?;
                        }
                    }
                    9 => return self.ret(),
                    11 => {
                        let t = r.u32()?;
                        ensure!((t as usize) < s.types.len(), "push of unknown type");
                        self.stack.push(Slot { ty: Some(t), v: s.default_value(t) });
                    }
                    12 => {
                        let o = r.u8()?;
                        let d = self.operand(&mut r)?;
                        let a = self.operand(&mut r)?;
                        let b = self.operand(&mut r)?;
                        let v = compare(o, &self.value_of(&a)?, &self.value_of(&b)?)?;
                        self.assign(&d, Value::Int(v as i64))?;
                    }
                    13 => {
                        let v = self.operand(&mut r)?;
                        let Value::Proc(p) = self.value_of(&v)? else { bail!("call through a non-procedure") };
                        ensure!(p != u32::MAX, "call through a nil procedure variable");
                        self.pos = r.o;
                        return self.call_proc(p, host);
                    }
                    14 | 22 => {
                        // Store a reference in a pointer variable (the slot itself, not its target).
                        let Operand::Var(d) = self.operand(&mut r)? else { bail!("pointer store into a constant") };
                        let Operand::Var(src) = self.operand(&mut r)? else { bail!("pointer to a constant") };
                        let src = self.resolve(src)?;
                        ensure!(d.path.is_empty(), "pointer store into a field");
                        self.slot_mut(d.root)?.v = Value::Ref(src);
                    }
                    15 => {
                        let d = self.operand(&mut r)?;
                        let v = self.value_of(&d)?;
                        self.assign(&d, Value::Int(!v.as_bool() as i64))?;
                    }
                    16 => {
                        let d = self.operand(&mut r)?;
                        let v = match self.value_of(&d)? {
                            Value::Float(f) => Value::Float(-f),
                            v => Value::Int(-v.as_int()),
                        };
                        self.assign(&d, v)?;
                    }
                    17 => {
                        let v = self.operand(&mut r)?;
                        let negate = r.u8()? == 1;
                        self.flag = self.value_of(&v)?.as_bool() != negate;
                    }
                    18 => {
                        let d = r.i32()?;
                        if self.flag {
                            r.o = jump(r.o, d)?;
                        }
                    }
                    19 | 20 => bail!("try/except blocks are not supported"),
                    21 => {
                        let d = self.operand(&mut r)?;
                        let v = self.value_of(&d)?;
                        self.assign(&d, Value::Int(!v.as_int()))?;
                    }
                    23 | 24 => {
                        let d = self.operand(&mut r)?;
                        let v = self.value_of(&d)?.as_int();
                        self.assign(&d, Value::Int(if op == 23 { v + 1 } else { v - 1 }))?;
                    }
                    255 => {}
                    10 => bail!("instruction 'set stack type' is not supported"),
                    _ => bail!("unknown instruction {op}"),
                }
                self.pos = r.o;
                Ok(false)
            })();
            let finished = res.with_context(|| format!("at offset {} of {}", at - self.proc_start(), self.proc_name()))?;
            if finished {
                return Ok(());
            }
        }
    }

    fn proc_name(&self) -> String {
        match self.proc.map(|p| (p, &self.s.procs[p as usize])) {
            Some((_, Proc::Internal { name: Some(n), .. })) => n.clone(),
            Some((p, _)) => format!("procedure #{p}"),
            None => "?".into(),
        }
    }

    /// Calls procedure `p` (the return position is already saved in `self.pos`). Returns
    /// true when the outermost procedure returned.
    fn call_proc(&mut self, p: u32, host: &mut dyn Host) -> Result<bool> {
        match self.s.procs.get(p as usize).context("call of unknown procedure")? {
            Proc::Internal { .. } => {
                self.enter(p)?;
                Ok(false)
            }
            Proc::External { key } => {
                let (n, has_result) = host
                    .signature(key)
                    .with_context(|| format!("the installer script calls {key}, which arc-unpack does not provide"))?;
                let top = self.stack.len();
                let need = n + has_result as usize;
                ensure!(top >= self.base + 1 + need, "{key}: not enough arguments on the stack");
                let first_arg = if has_result { top - 2 } else { top - 1 };
                let mut args = Vec::with_capacity(n);
                let mut refs = Vec::with_capacity(n);
                for i in 0..n {
                    let slot = &self.stack[first_arg - i];
                    match &slot.v {
                        Value::Ref(rf) => {
                            args.push(self.read(rf)?);
                            refs.push(Some(rf.clone()));
                        }
                        v => {
                            args.push(v.clone());
                            refs.push(None);
                        }
                    }
                }
                let mut out = Vec::new();
                let result = host.call(key, &args, &mut out).with_context(|| format!("in {key}"))?;
                for (i, v) in out {
                    if let Some(Some(rf)) = refs.get(i) {
                        self.write(rf, v)?;
                    }
                }
                if has_result {
                    let Value::Ref(rf) = self.stack[top - 1].v.clone() else { bail!("{key}: result is not a variable") };
                    let v = match result {
                        Some(v) => v,
                        None => self.default_at(&rf),
                    };
                    self.write(&rf, v)?;
                }
                Ok(false)
            }
        }
    }

    fn ret(&mut self) -> Result<bool> {
        let Value::Ret(ra) = self.stack.get(self.base).map(|s| s.v.clone()).context("return without a frame")? else {
            bail!("corrupt stack frame")
        };
        self.stack.truncate(self.base);
        self.proc = ra.proc;
        self.pos = ra.pos;
        self.base = ra.base;
        Ok(self.proc.is_none())
    }

    fn operand(&self, r: &mut Bytes) -> Result<Operand> {
        let kind = r.u8()?;
        let var = |n: u32| -> Result<Ref> {
            if n < NEGATIVE_STACK_START {
                ensure!((n as usize) < self.globals.len(), "unknown global {n}");
                Ok(Ref { root: Root::Global(n as usize), path: Vec::new() })
            } else {
                let off = n.wrapping_sub(STACK_START) as i32 as i64;
                let at = self.base as i64 + off;
                ensure!(at >= 0 && (at as usize) < self.stack.len(), "stack variable out of range");
                Ok(Ref { root: Root::Stack(at as usize), path: Vec::new() })
            }
        };
        Ok(match kind {
            0 => Operand::Var(var(r.u32()?)?),
            1 => {
                let t = r.u32()?;
                Operand::Const(self.s.read_const(r, t)?)
            }
            2 => {
                let mut rf = var(r.u32()?)?;
                rf.path.push(r.u32()? as usize);
                Operand::Var(rf)
            }
            3 => {
                let mut rf = var(r.u32()?)?;
                let idx = self.read(&var(r.u32()?)?)?.as_int();
                ensure!(idx >= 0, "negative array index");
                rf.path.push(idx as usize);
                Operand::Var(rf)
            }
            k => bail!("unknown operand kind {k}"),
        })
    }

    /// Follows var parameters (slots holding a reference) to the variable they stand for.
    fn resolve(&self, mut rf: Ref) -> Result<Ref> {
        for _ in 0..64 {
            let slot = self.slot(rf.root)?;
            match &slot.v {
                Value::Ref(inner) => {
                    let mut path = inner.path.clone();
                    path.extend_from_slice(&rf.path);
                    rf = Ref { root: inner.root, path };
                }
                _ => return Ok(rf),
            }
        }
        bail!("reference cycle")
    }

    fn slot(&self, root: Root) -> Result<&Slot> {
        match root {
            Root::Global(i) => self.globals.get(i),
            Root::Stack(i) => self.stack.get(i),
        }
        .context("dangling variable")
    }

    fn slot_mut(&mut self, root: Root) -> Result<&mut Slot> {
        match root {
            Root::Global(i) => self.globals.get_mut(i),
            Root::Stack(i) => self.stack.get_mut(i),
        }
        .context("dangling variable")
    }

    fn read(&self, rf: &Ref) -> Result<Value> {
        let rf = self.resolve(rf.clone())?;
        let mut v = &self.slot(rf.root)?.v;
        for &i in &rf.path {
            v = match v {
                Value::Rec(items) | Value::Arr(items) => items.get(i).context("index out of range")?,
                Value::Str(s) => return Ok(Value::Str(s.chars().nth(i.saturating_sub(1)).map(String::from).unwrap_or_default())),
                _ => bail!("indexing a non-container"),
            };
        }
        Ok(v.clone())
    }

    fn type_at(&self, rf: &Ref) -> Option<u32> {
        let mut t = self.slot(rf.root).ok()?.ty?;
        for &i in &rf.path {
            t = self.s.child_type(t, i)?;
        }
        Some(t)
    }

    fn default_at(&self, rf: &Ref) -> Value {
        match self.resolve(rf.clone()).ok().and_then(|r| self.type_at(&r)) {
            Some(t) => self.s.default_value(t),
            None => Value::Nil,
        }
    }

    fn write(&mut self, rf: &Ref, v: Value) -> Result<()> {
        let rf = self.resolve(rf.clone())?;
        let ty = self.type_at(&rf);
        let v = match ty {
            Some(t) => convert(self.s.base(t), v),
            None => v,
        };
        let mut cur = &mut self.slot_mut(rf.root)?.v;
        for &i in &rf.path {
            cur = match cur {
                Value::Rec(items) => items.get_mut(i).context("field out of range")?,
                Value::Arr(items) => {
                    if i >= items.len() {
                        bail!("array index {i} out of range");
                    }
                    &mut items[i]
                }
                _ => bail!("indexing a non-container"),
            };
        }
        *cur = v;
        Ok(())
    }

    fn assign(&mut self, d: &Operand, v: Value) -> Result<()> {
        match d {
            Operand::Var(rf) => self.write(rf, v),
            Operand::Const(_) => bail!("assignment to a constant"),
        }
    }

    fn value_of(&self, o: &Operand) -> Result<Value> {
        match o {
            Operand::Var(rf) => self.read(rf),
            Operand::Const(v) => Ok(v.clone()),
        }
    }

    fn type_of(&self, o: &Operand) -> Option<u32> {
        match o {
            Operand::Var(rf) => self.resolve(rf.clone()).ok().and_then(|r| self.type_at(&r)),
            Operand::Const(_) => None,
        }
    }

    /// Reads a global variable (for tests and diagnostics).
    pub fn global(&self, i: usize) -> Option<&Value> {
        self.globals.get(i).map(|s| &s.v)
    }
}

fn jump(after: usize, d: i32) -> Result<usize> {
    let to = after as i64 + d as i64;
    ensure!(to >= 0, "jump before the script");
    Ok(to as usize)
}

/// Converts a value to the variable's base type on assignment.
fn convert(base: u8, v: Value) -> Value {
    let wrap = |i: i64| -> i64 {
        match base {
            bt::U8 => i as u8 as i64,
            bt::S8 => i as i8 as i64,
            bt::U16 => i as u16 as i64,
            bt::S16 => i as i16 as i64,
            bt::U32 => i as u32 as i64,
            bt::S32 => i as i32 as i64,
            _ => i,
        }
    };
    match base {
        bt::U8 | bt::S8 | bt::U16 | bt::S16 | bt::U32 | bt::S32 | bt::S64 => match v {
            Value::Int(i) => Value::Int(wrap(i)),
            Value::Float(f) => Value::Int(wrap(f as i64)),
            Value::Str(s) => Value::Int(wrap(s.chars().next().map(|c| c as i64).unwrap_or(0))),
            other => other,
        },
        bt::SINGLE | bt::DOUBLE | bt::EXTENDED | bt::CURRENCY => Value::Float(v.as_float()),
        bt::STRING | bt::PCHAR | bt::WIDE_STRING | bt::UNICODE_STRING => match v {
            Value::Str(s) => Value::Str(s),
            Value::Int(i) => Value::Str(char::from_u32(i as u32).map(String::from).unwrap_or_default()),
            other => other,
        },
        bt::CHAR | bt::WIDE_CHAR => match v {
            Value::Str(s) => Value::Str(s.chars().next().unwrap_or('\0').to_string()),
            Value::Int(i) => Value::Str(char::from_u32(i as u32).unwrap_or('\0').to_string()),
            other => other,
        },
        _ => v,
    }
}

fn calc(op: u8, a: Value, b: Value) -> Result<Value> {
    Ok(match (a, b) {
        (Value::Str(x), y) => {
            ensure!(op == 0, "string operation {op}");
            Value::Str(x + &y.as_str())
        }
        (Value::Set(mut x), Value::Set(y)) => {
            for (i, by) in y.iter().enumerate() {
                if i < x.len() {
                    x[i] = match op {
                        0 => x[i] | by,
                        1 => x[i] & !by,
                        2 => x[i] & by,
                        _ => bail!("set operation {op}"),
                    };
                }
            }
            Value::Set(x)
        }
        (Value::Float(x), y) => float_op(op, x, y.as_float())?,
        (x, Value::Float(y)) => float_op(op, x.as_float(), y)?,
        (x, y) => {
            let (x, y) = (x.as_int(), y.as_int());
            Value::Int(match op {
                0 => x.wrapping_add(y),
                1 => x.wrapping_sub(y),
                2 => x.wrapping_mul(y),
                3 => {
                    ensure!(y != 0, "division by zero");
                    x / y
                }
                4 => {
                    ensure!(y != 0, "division by zero");
                    x % y
                }
                5 => x.wrapping_shl(y as u32),
                6 => ((x as u64) >> (y as u32 & 63)) as i64,
                7 => x & y,
                8 => x | y,
                9 => x ^ y,
                _ => bail!("unknown operation {op}"),
            })
        }
    })
}

fn float_op(op: u8, x: f64, y: f64) -> Result<Value> {
    Ok(Value::Float(match op {
        0 => x + y,
        1 => x - y,
        2 => x * y,
        3 => x / y,
        _ => bail!("float operation {op}"),
    }))
}

fn compare(op: u8, a: &Value, b: &Value) -> Result<bool> {
    use std::cmp::Ordering;
    if op == 6 {
        let Value::Set(bits) = b else { bail!("'in' needs a set") };
        let i = a.as_int();
        return Ok(i >= 0 && bits.get(i as usize / 8).is_some_and(|by| by >> (i % 8) & 1 == 1));
    }
    if op == 7 {
        return Ok(false); // 'is': class hierarchy is not modelled
    }
    let ord = match (a, b) {
        (Value::Str(x), Value::Str(y)) => x.cmp(y),
        (Value::Str(_), _) | (_, Value::Str(_)) => a.as_str().cmp(&b.as_str()),
        (Value::Float(_), _) | (_, Value::Float(_)) => a.as_float().partial_cmp(&b.as_float()).unwrap_or(Ordering::Equal),
        (Value::Set(x), Value::Set(y)) => {
            return match op {
                4 => Ok(x != y),
                5 => Ok(x == y),
                _ => bail!("set comparison {op}"),
            };
        }
        _ => a.as_int().cmp(&b.as_int()),
    };
    Ok(match op {
        0 => ord != Ordering::Less,
        1 => ord != Ordering::Greater,
        2 => ord == Ordering::Greater,
        3 => ord == Ordering::Less,
        4 => ord != Ordering::Equal,
        5 => ord == Ordering::Equal,
        _ => bail!("unknown comparison {op}"),
    })
}

/// Human-readable listing of one procedure (for `arc-unpack script --disasm`).
pub fn disassemble(s: &Script, p: u32) -> Result<Vec<String>> {
    let Proc::Internal { start, len, .. } = s.procs.get(p as usize).context("no such procedure")? else {
        bail!("external procedure")
    };
    let mut out = Vec::new();
    let mut r = Bytes { b: &s.code, o: *start };
    let end = start + len;
    let name = |i: u32| match s.procs.get(i as usize) {
        Some(Proc::Internal { name: Some(n), .. }) => n.clone(),
        Some(Proc::External { key }) => key.clone(),
        _ => format!("proc{i}"),
    };
    let operand = |r: &mut Bytes| -> Result<String> {
        let kind = r.u8()?;
        let v = |n: u32| {
            if n < NEGATIVE_STACK_START { format!("G{n}") } else { format!("S{}", n.wrapping_sub(STACK_START) as i32) }
        };
        Ok(match kind {
            0 => v(r.u32()?),
            1 => {
                let t = r.u32()?;
                format!("{:?}", s.read_const(r, t)?)
            }
            2 => {
                let (a, i) = (r.u32()?, r.u32()?);
                format!("{}[{i}]", v(a))
            }
            3 => {
                let (a, i) = (r.u32()?, r.u32()?);
                format!("{}[{}]", v(a), v(i))
            }
            k => bail!("operand kind {k}"),
        })
    };
    const CALC: [&str; 10] = ["+", "-", "*", "/", "mod", "shl", "shr", "and", "or", "xor"];
    const CMP: [&str; 8] = [">=", "<=", ">", "<", "<>", "=", "in", "is"];
    while r.o < end {
        let at = r.o - start;
        let op = r.u8()?;
        let text = match op {
            0 => format!("{} := {}", operand(&mut r)?, operand(&mut r)?),
            1 => {
                let o = r.u8()? as usize;
                format!("{} {}= {}", operand(&mut r)?, CALC.get(o).unwrap_or(&"?"), operand(&mut r)?)
            }
            2 => format!("push {}", operand(&mut r)?),
            3 => format!("pushvar {}", operand(&mut r)?),
            4 => "pop".into(),
            5 => format!("call {}", name(r.u32()?)),
            6 => {
                let d = r.i32()?;
                format!("goto {}", (r.o - start) as i64 + d as i64)
            }
            7 | 8 => {
                let d = r.i32()?;
                let x = operand(&mut r)?;
                format!("{} {x} goto {}", if op == 7 { "if" } else { "ifnot" }, (r.o - start) as i64 + d as i64)
            }
            9 => "ret".into(),
            10 => format!("settype {} {}", r.u32()?, r.u32()?),
            11 => {
                let t = r.u32()?;
                format!("pushtype #{t} (base {})", s.base(t))
            }
            12 => {
                let o = r.u8()? as usize;
                let d = operand(&mut r)?;
                format!("{d} := {} {} {}", operand(&mut r)?, CMP.get(o).unwrap_or(&"?"), operand(&mut r)?)
            }
            13 => format!("callvar {}", operand(&mut r)?),
            14 | 22 => format!("setptr {} {}", operand(&mut r)?, operand(&mut r)?),
            15 => format!("not {}", operand(&mut r)?),
            16 => format!("neg {}", operand(&mut r)?),
            17 => {
                let x = operand(&mut r)?;
                format!("setflag {x} {}", r.u8()?)
            }
            18 => {
                let d = r.i32()?;
                format!("flaggoto {}", (r.o - start) as i64 + d as i64)
            }
            19 => format!("try {} {} {} {}", r.u32()?, r.u32()?, r.u32()?, r.u32()?),
            20 => format!("endtry {}", r.u8()?),
            21 => format!("inot {}", operand(&mut r)?),
            23 => format!("inc {}", operand(&mut r)?),
            24 => format!("dec {}", operand(&mut r)?),
            255 => "nop".into(),
            _ => {
                out.push(format!("{at:6}  ?? {op}"));
                break;
            }
        };
        out.push(format!("{at:6}  {text}"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiny assembler for test programs.
    struct Asm {
        code: Vec<u8>,
    }

    impl Asm {
        fn op(&mut self, b: u8) -> &mut Self {
            self.code.push(b);
            self
        }
        fn u32(&mut self, v: u32) -> &mut Self {
            self.code.extend_from_slice(&v.to_le_bytes());
            self
        }
        fn i32(&mut self, v: i32) -> &mut Self {
            self.code.extend_from_slice(&v.to_le_bytes());
            self
        }
        fn var(&mut self, n: u32) -> &mut Self {
            self.op(0).u32(n)
        }
        fn local(&mut self, n: i32) -> &mut Self {
            self.op(0).u32(STACK_START.wrapping_add(n as u32))
        }
        fn int(&mut self, t: u32, v: u32) -> &mut Self {
            self.op(1).u32(t).u32(v)
        }
        fn ustr(&mut self, t: u32, s: &str) -> &mut Self {
            let w: Vec<u16> = s.encode_utf16().collect();
            self.op(1).u32(t).u32(w.len() as u32);
            for c in w {
                self.code.extend_from_slice(&c.to_le_bytes());
            }
            self
        }
    }

    /// Builds a script: types [u32, unicodestring, u8], globals [u32 G0, ustring G1],
    /// procs [0: external CONCATX, 1: MAIN (code), 2: external NOTE].
    fn script(body: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        for v in [MAGIC, 23, 3, 3, 2, 1, 0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for base in [bt::U32, bt::UNICODE_STRING, bt::U8] {
            b.push(base);
            b.extend_from_slice(&0u32.to_le_bytes()); // no attributes
        }
        // external "CONCATX"
        b.push(1);
        b.push(7);
        b.extend_from_slice(b"CONCATX");
        // internal MAIN, exported
        // header, 3 types, CONCATX, MAIN (flags, start, len, name, empty decl), NOTE, 2 globals
        let header_len = 28 + 3 * 5 + (2 + 7) + (1 + 8 + 4 + 4 + 4) + (2 + 4) + 2 * 5;
        b.push(2);
        b.extend_from_slice(&(header_len as u32).to_le_bytes());
        b.extend_from_slice(&(body.len() as u32).to_le_bytes());
        b.extend_from_slice(&4u32.to_le_bytes());
        b.extend_from_slice(b"MAIN");
        b.extend_from_slice(&0u32.to_le_bytes());
        // external "NOTE"
        b.push(1);
        b.push(4);
        b.extend_from_slice(b"NOTE");
        for t in [0u32, 1] {
            b.extend_from_slice(&t.to_le_bytes());
            b.push(0);
        }
        assert_eq!(b.len(), header_len, "header length bookkeeping");
        b.extend_from_slice(body);
        b
    }

    #[derive(Default)]
    struct TestHost {
        notes: Vec<String>,
    }

    impl Host for TestHost {
        fn signature(&self, key: &str) -> Option<(usize, bool)> {
            match key {
                "CONCATX" => Some((2, true)),
                "NOTE" => Some((1, false)),
                _ => None,
            }
        }
        fn call(&mut self, key: &str, args: &[Value], _out: &mut Vec<(usize, Value)>) -> Result<Option<Value>> {
            Ok(match key {
                "CONCATX" => Some(Value::Str(format!("{}|{}", args[0].as_str(), args[1].as_str()))),
                "NOTE" => {
                    self.notes.push(args[0].as_str());
                    None
                }
                _ => unreachable!(),
            })
        }
    }

    #[test]
    fn loop_calls_and_branches() {
        // G0 := 0; repeat G0 += 1 until G0 >= 3;  G1 := CONCATX('a', 'b'); NOTE(G1);
        // if G0 = 3 then NOTE('three')
        let mut a = Asm { code: Vec::new() };
        a.op(0).var(0).int(0, 0); // G0 := 0          @0
        let loop_top = a.code.len();
        a.op(1).op(0).var(0).int(0, 1); // G0 += 1
        a.op(11).u32(2); // pushtype u8 -> S1
        a.op(12).op(0).local(1).var(0).int(0, 3); // S1 := G0 >= 3
        a.op(17).local(1).op(1); // flag := not S1
        a.op(4); // pop S1
        let back = loop_top as i32 - (a.code.len() as i32 + 5);
        a.op(18).i32(back); // flaggoto loop_top
        // G1 := CONCATX('a', 'b'): push params reversed, then result reference
        a.op(11).u32(1).op(0).local(1).ustr(1, "b"); // S1 := 'b' (second parameter)
        a.op(11).u32(1).op(0).local(2).ustr(1, "a"); // S2 := 'a' (first parameter)
        a.op(3).var(1); // pushvar G1
        a.op(5).u32(0); // call CONCATX
        a.op(4).op(4).op(4);
        a.op(2).var(1); // push G1
        a.op(5).u32(2); // call NOTE
        a.op(4);
        a.op(11).u32(2); // S1: u8
        a.op(12).op(5).local(1).var(0).int(0, 3); // S1 := G0 = 3
        a.op(17).local(1).op(1); // flag := not S1
        a.op(4);
        let skip_at = a.code.len();
        a.op(18).i32(0); // placeholder
        let skip_from = a.code.len();
        a.op(11).u32(1).op(0).local(1).ustr(1, "three");
        a.op(5).u32(2).op(4);
        let skip = (a.code.len() - skip_from) as i32;
        a.code[skip_at + 1..skip_at + 5].copy_from_slice(&skip.to_le_bytes());
        a.op(9);

        let bytes = script(&a.code);
        let s = Script::load(&bytes).unwrap();
        let mut vm = Vm::new(&s);
        let mut host = TestHost::default();
        vm.call("MAIN", &[], &mut host).unwrap();
        assert_eq!(vm.global(0), Some(&Value::Int(3)));
        assert_eq!(vm.global(1), Some(&Value::Str("a|b".into())));
        assert_eq!(host.notes, ["a|b", "three"]);
    }

    #[test]
    fn unknown_external_is_a_clear_error() {
        struct NoHost;
        impl Host for NoHost {
            fn signature(&self, _: &str) -> Option<(usize, bool)> {
                None
            }
            fn call(&mut self, _: &str, _: &[Value], _: &mut Vec<(usize, Value)>) -> Result<Option<Value>> {
                unreachable!()
            }
        }
        let mut a = Asm { code: Vec::new() };
        a.op(5).u32(2).op(9);
        let bytes = script(&a.code);
        let s = Script::load(&bytes).unwrap();
        let err = Vm::new(&s).call("MAIN", &[], &mut NoHost).unwrap_err();
        assert!(format!("{err:#}").contains("calls NOTE"), "{err:#}");
    }

    #[test]
    fn extended_constants() {
        // 1.0 as an 80-bit extended: mantissa 0x8000000000000000, exponent 16383
        let mut b = 0x8000_0000_0000_0000u64.to_le_bytes().to_vec();
        b.extend_from_slice(&16383u16.to_le_bytes());
        assert_eq!(extended_to_f64(&b), 1.0);
    }
}
