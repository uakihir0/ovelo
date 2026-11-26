//! DEX→native AOT via Cranelift (design: "DEX→IR→ARM64 starting with
//! low-side-effect methods containing branches and integers").
//!
//! A method is compiled when it only uses an integer-typed whitelist:
//! moves, consts, int/long arithmetic and shifts, int/long literals,
//! integer comparisons, branches, and returns — plus a signature whose
//! parameters and return are `V|Z|B|S|C|I|J` (the receiver slot may
//! still carry a handle for non-static methods). Everything else —
//! objects, arrays, fields, invokes, switches, exceptions — stays on
//! the interpreter, which shares the register/ABI contract, so a
//! compiled method and an interpreted one observe identical results.
//!
//! Compiled ABI: `unsafe extern "C" fn(*mut i64 regs, *mut u8 trap)
//! -> i64`. `regs` is the method's register file (one i64 per dex
//! register) with arguments preloaded in the last `ins_size` slots —
//! the same layout `Vm::exec` builds for `Frame.regs`. `trap` receives
//! a fault code: `TRAP_DIV_ZERO` = div/rem by zero (the caller raises
//! `ArithmeticException`).

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    types, AbiParam, Block, Function, InstBuilder, MemFlagsData, Signature, UserFuncName, Value,
};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module};
use hashbrown::HashMap;
use ovelo_dex::CodeItem;
use ovelo_dex::{decode_all, Insn};

/// Compiled method: `(regs, trap) -> i64`.
pub type AotFn = unsafe extern "C" fn(*mut i64, *mut u8) -> i64;

/// Trap codes stored through `trap` for div/rem by zero.
pub const TRAP_DIV_ZERO: u8 = 1;

/// `rem` by zero — the interpreter distinguishes the messages.
pub const TRAP_REM_ZERO: u8 = 2;

/// Opcodes the compiler supports (see module docs).
fn whitelisted(op: u8) -> bool {
    match op {
        // nop, move/move-wide/move-object (+/from16, /16)
        0x00..=0x09 => true,
        // return-void / return / return-wide
        0x0e..=0x10 => true,
        // const*/4..const-wide/high16 (no strings/classes)
        0x12..=0x19 => true,
        // goto (+/16, /32)
        0x28..=0x2a => true,
        // cmp-long
        0x31 => true,
        // if-* / if-*z
        0x32..=0x3d => true,
        // neg-int, not-int, neg-long, not-long
        0x7b..=0x7e => true,
        // int-to-long, long-to-int, int-to-byte/char/short
        0x81 | 0x84 | 0x8d | 0x8e | 0x8f => true,
        // int + long binops
        0x90..=0xa5 => true,
        // int + long /2addr
        0xb0..=0xc5 => true,
        // int /lit16, /lit8
        0xd0..=0xe2 => true,
        _ => false,
    }
}

/// Method signature eligibility: params + return all integer-typed.
pub fn int_typed(d: &str) -> bool {
    matches!(d, "V" | "Z" | "B" | "S" | "C" | "I" | "J")
}

/// Compiled-code cache. Owns the JIT module that backs the code pages.
pub struct Aot {
    fns: HashMap<u64, AotFn>,
    /// Kept alive: function pointers point into its code pages.
    /// `Option` so `Drop` can `free_memory` (a consuming call).
    module: Option<JITModule>,
}

impl Drop for Aot {
    fn drop(&mut self) {
        if let Some(m) = self.module.take() {
            #[allow(unsafe_code)]
            unsafe {
                m.free_memory();
            }
        }
    }
}

impl Aot {
    /// A compiler for the host ISA; `None` when Cranelift can't target it.
    pub fn new() -> Option<Self> {
        let mut flags = settings::builder();
        // Compile speed beats peak codegen: guest methods are small.
        let _ = flags.set("opt_level", "speed");
        let isa = cranelift_native::builder()
            .ok()
            .and_then(|b| b.finish(settings::Flags::new(flags)).ok())?;
        let jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        Some(Self {
            fns: HashMap::new(),
            module: Some(JITModule::new(jb)),
        })
    }

    /// Compile `midx` when its code is whitelisted. Eligibility of the
    /// signature is the caller's job (`int_typed` on params + return).
    pub fn compile(&mut self, midx: u64, code: &CodeItem) {
        if self.fns.contains_key(&midx) {
            return;
        }
        let module = self.module.as_mut().expect("jit module");
        let cfg = module.isa().frontend_config();
        let mut ctx = module.make_context();
        let mut fbcx = FunctionBuilderContext::new();
        let Some(func) = build_function(midx, code, &mut fbcx, cfg) else {
            return;
        };
        let Ok(fid) =
            module.declare_function(&format!("dex_m{midx}"), Linkage::Export, &func.signature)
        else {
            return;
        };
        ctx.func = func;
        if module.define_function(fid, &mut ctx).is_err() {
            return;
        }
        module.clear_context(&mut ctx);
        if module.finalize_definitions().is_err() {
            return;
        }
        let ptr = module.get_finalized_function(fid);
        if !ptr.is_null() {
            self.fns.insert(
                midx,
                #[allow(unsafe_code)]
                unsafe {
                    std::mem::transmute::<*const u8, AotFn>(ptr)
                },
            );
        }
    }

    /// Compiled code for a (resolved) method id, if any.
    pub fn get(&self, midx: u64) -> Option<AotFn> {
        self.fns.get(&midx).copied()
    }

    /// How many methods were compiled.
    pub fn len(&self) -> usize {
        self.fns.len()
    }

    /// Whether nothing compiled.
    pub fn is_empty(&self) -> bool {
        self.fns.is_empty()
    }
}

/// IR-build one method. `None` = stay on the interpreter.
fn build_function(
    midx: u64,
    code: &CodeItem,
    fbcx: &mut FunctionBuilderContext,
    cfg: TargetFrontendConfig,
) -> Option<Function> {
    if !code.tries.is_empty() {
        return None; // guest catch handlers stay interpreted
    }
    let table = decode_all(&code.insns).ok()?;
    let last = table.last()?;
    if !matches!(last.opcode, 0x0e..=0x10) {
        return None; // method must end in a return
    }

    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(types::I64)); // regs
    sig.params.push(AbiParam::new(types::I64)); // trap
    sig.returns.push(AbiParam::new(types::I64));

    let mut func = Function::with_name_signature(UserFuncName::user(0, midx as u32), sig);
    let mut b = FunctionBuilder::new(&mut func, fbcx);
    let nregs = code.registers_size as usize;
    let vars: Vec<Variable> = (0..nregs).map(|_| b.declare_var(types::I64)).collect();

    // One IR block per instruction offset (payloads excluded below).
    let mut blocks = HashMap::new();
    for ins in &table {
        blocks.insert(ins.offset, b.create_block());
    }
    let entry = blocks[&table[0].offset];
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    let regs_ptr = b.block_params(entry)[0];
    let flag_ptr = b.block_params(entry)[1];

    // Zero every register, then load incoming args into the last
    // ins_size slots (mirrors Vm::exec's arg copy).
    let ins_size = code.ins_size as usize;
    let start = nregs.saturating_sub(ins_size);
    for (i, var) in vars.iter().enumerate().take(nregs) {
        let v = if i >= start {
            b.ins()
                .load(types::I64, MemFlagsData::trusted(), regs_ptr, i as i32 * 8)
        } else {
            b.ins().iconst(types::I64, 0)
        };
        b.def_var(*var, v);
    }

    for (i, ins) in table.iter().enumerate() {
        if ins.is_payload() || !whitelisted(ins.opcode) {
            return None;
        }
        if i > 0 {
            // insn 0 shares the entry block (already the current block).
            b.switch_to_block(blocks[&ins.offset]);
        }
        let next = table.get(i + 1).map(|t| blocks[&t.offset]);
        if !emit(&mut b, ins, &vars, &blocks, flag_ptr, next) {
            return None;
        }
        if let Some(n) = next {
            if !is_terminator(ins) {
                b.ins().jump(n, &[]);
            }
        } else if !is_terminator(ins) {
            return None; // falls off the end without a return
        }
    }
    for blk in blocks.values() {
        b.seal_block(*blk);
    }
    b.finalize(cfg);
    Some(func)
}

fn is_terminator(ins: &Insn) -> bool {
    matches!(
        ins.opcode,
        0x0e..=0x10 | 0x28..=0x2a | 0x32..=0x3d
    )
}

/// Emit one instruction. `false` = bail to the interpreter.
/// `next` = the block of the next instruction (brif's not-taken edge).
#[allow(clippy::too_many_arguments)]
fn emit(
    b: &mut FunctionBuilder,
    ins: &Insn,
    vars: &[Variable],
    blocks: &HashMap<u32, Block>,
    flag_ptr: Value,
    next: Option<Block>,
) -> bool {
    let op = ins.opcode;
    let r = |b: &mut FunctionBuilder, i: usize| b.use_var(vars[i]);
    match op {
        0x00 => {}
        0x01..=0x09 => {
            let v = r(b, ins.regs[1] as usize);
            b.def_var(vars[ins.regs[0] as usize], v);
        }
        0x0e => {
            let z = b.ins().iconst(types::I64, 0);
            b.ins().return_(&[z]);
        }
        0x0f | 0x10 => {
            let v = r(b, ins.regs[0] as usize);
            b.ins().return_(&[v]);
        }
        0x12..=0x15 => {
            let lit = if op == 0x15 {
                ins.lit.unwrap_or(0) << 16
            } else {
                ins.lit.unwrap_or(0)
            };
            let c = b.ins().iconst(types::I64, lit);
            let t = b.ins().ireduce(types::I32, c);
            let v = b.ins().sextend(types::I64, t);
            b.def_var(vars[ins.regs[0] as usize], v);
        }
        0x16..=0x18 => {
            let v = b.ins().iconst(types::I64, ins.lit.unwrap_or(0));
            b.def_var(vars[ins.regs[0] as usize], v);
        }
        0x19 => {
            let v = b.ins().iconst(types::I64, ins.lit.unwrap_or(0) << 48);
            b.def_var(vars[ins.regs[0] as usize], v);
        }
        0x28..=0x2a => {
            let Some(&t) = ins.target.and_then(|t| blocks.get(&t)) else {
                return false;
            };
            b.ins().jump(t, &[]);
        }
        0x31 => {
            // cmp-long: -1 / 0 / 1
            let a = r(b, ins.regs[1] as usize);
            let c = r(b, ins.regs[2] as usize);
            let lt = b.ins().icmp(IntCC::SignedLessThan, a, c);
            let gt = b.ins().icmp(IntCC::SignedGreaterThan, a, c);
            let lt64 = b.ins().uextend(types::I64, lt);
            let gt64 = b.ins().uextend(types::I64, gt);
            let v = b.ins().isub(gt64, lt64);
            b.def_var(vars[ins.regs[0] as usize], v);
        }
        0x32..=0x3d => {
            let cc = match op {
                0x32 | 0x38 => IntCC::Equal,
                0x33 | 0x39 => IntCC::NotEqual,
                0x34 | 0x3a => IntCC::SignedLessThan,
                0x35 | 0x3b => IntCC::SignedGreaterThanOrEqual,
                0x36 | 0x3c => IntCC::SignedGreaterThan,
                _ => IntCC::SignedLessThanOrEqual,
            };
            let cond = if op >= 0x38 {
                let v = r(b, ins.regs[0] as usize);
                b.ins().icmp_imm_s(cc, v, 0)
            } else {
                let a = r(b, ins.regs[0] as usize);
                let c = r(b, ins.regs[1] as usize);
                b.ins().icmp(cc, a, c)
            };
            let (Some(&taken), Some(not)) = (ins.target.and_then(|t| blocks.get(&t)), next) else {
                return false;
            };
            b.ins().brif(cond, taken, &[], not, &[]);
        }
        0x7b..=0x7e => {
            let src = r(b, ins.regs[1] as usize);
            let v = match op {
                0x7b => {
                    let t = b.ins().ireduce(types::I32, src);
                    let n = b.ins().ineg(t);
                    b.ins().sextend(types::I64, n)
                }
                0x7c => {
                    let t = b.ins().ireduce(types::I32, src);
                    let m = b.ins().iconst(types::I32, -1);
                    let n = b.ins().bxor(t, m);
                    b.ins().sextend(types::I64, n)
                }
                0x7d => b.ins().ineg(src),
                _ => {
                    let m = b.ins().iconst(types::I64, -1);
                    b.ins().bxor(src, m)
                }
            };
            b.def_var(vars[ins.regs[0] as usize], v);
        }
        0x81 => {
            // int-to-long: values already sign-extended — copy.
            let v = r(b, ins.regs[1] as usize);
            b.def_var(vars[ins.regs[0] as usize], v);
        }
        0x84 => {
            // long-to-int: truncate + sign-extend.
            let src = r(b, ins.regs[1] as usize);
            let t = b.ins().ireduce(types::I32, src);
            let v = b.ins().sextend(types::I64, t);
            b.def_var(vars[ins.regs[0] as usize], v);
        }
        0x8d..=0x8f => {
            let w = r(b, ins.regs[1] as usize);
            let v = match op {
                0x8d => {
                    let t = b.ins().ireduce(types::I8, w);
                    b.ins().sextend(types::I64, t)
                }
                0x8e => {
                    let t = b.ins().ireduce(types::I16, w);
                    b.ins().uextend(types::I64, t)
                }
                _ => {
                    let t = b.ins().ireduce(types::I16, w);
                    b.ins().sextend(types::I64, t)
                }
            };
            b.def_var(vars[ins.regs[0] as usize], v);
        }
        0x90..=0xa5 => {
            emit_binop(
                b,
                op,
                ins.regs[0] as usize,
                ins.regs[1] as usize,
                ins.regs[2] as usize,
                vars,
                flag_ptr,
            );
        }
        0xb0..=0xc5 => {
            // /2addr: vA op= vB; same op at (op - 0xb0 + 0x90).
            emit_binop(
                b,
                op - 0xb0 + 0x90,
                ins.regs[0] as usize,
                ins.regs[0] as usize,
                ins.regs[1] as usize,
                vars,
                flag_ptr,
            );
        }
        0xd0..=0xe2 => {
            emit_lit_binop(
                b,
                op,
                ins.regs[0] as usize,
                ins.regs[1] as usize,
                ins.lit.unwrap_or(0) as i32,
                vars,
                flag_ptr,
            );
        }
        _ => return false,
    }
    true
}

/// Guard `divisor != 0`; on zero, store the trap code and return.
fn zero_guard(b: &mut FunctionBuilder, divisor: Value, flag_ptr: Value, code: u8) {
    let trap = b.create_block();
    let cont = b.create_block();
    b.set_cold_block(trap);
    let zero = b.ins().icmp_imm_s(IntCC::Equal, divisor, 0);
    b.ins().brif(zero, trap, &[], cont, &[]);
    b.switch_to_block(trap);
    let one = b.ins().iconst(types::I64, code as i64);
    b.ins().istore8(MemFlagsData::trusted(), one, flag_ptr, 0);
    let z = b.ins().iconst(types::I64, 0);
    b.ins().return_(&[z]);
    b.seal_block(trap);
    b.switch_to_block(cont);
    b.seal_block(cont);
}

/// Keep `sdiv`/`srem` off the INT_MIN / -1 case (a real CPU trap, not a
/// guest exception). The interpreter wraps: MIN/-1 = MIN, MIN%-1 = 0 —
/// the same results a divisor of 1 gives, so we substitute 1.
fn safe_divisor(b: &mut FunctionBuilder, a: Value, c: Value) -> Value {
    let ty = b.func.dfg.value_type(c);
    let min = b.ins().iconst(
        ty,
        if ty == types::I64 {
            i64::MIN
        } else {
            i32::MIN as i64
        },
    );
    let neg1 = b.ins().iconst(ty, -1);
    let one = b.ins().iconst(ty, 1);
    let is_min = b.ins().icmp(IntCC::Equal, a, min);
    let is_neg1 = b.ins().icmp(IntCC::Equal, c, neg1);
    let ovf = b.ins().band(is_min, is_neg1);
    b.ins().select(ovf, one, c)
}

/// Emit a reg-reg int/long binop (opcodes 0x90..=0xa5 space).
fn emit_binop(
    b: &mut FunctionBuilder,
    op: u8,
    dst: usize,
    x: usize,
    y: usize,
    vars: &[Variable],
    flag_ptr: Value,
) {
    let wide = op >= 0x9b;
    let (a, c) = (b.use_var(vars[x]), b.use_var(vars[y]));
    // For int ops reduce to i32, compute, sign-extend back.
    let (a, c) = if wide {
        (a, c)
    } else {
        (
            b.ins().ireduce(types::I32, a),
            b.ins().ireduce(types::I32, c),
        )
    };
    let kind = (op - 0x90) % 0x0b;
    let v = match kind {
        0 => b.ins().iadd(a, c),
        1 => b.ins().isub(a, c),
        2 => b.ins().imul(a, c),
        3 => {
            zero_guard(b, c, flag_ptr, TRAP_DIV_ZERO);
            let c = safe_divisor(b, a, c);
            b.ins().sdiv(a, c)
        }
        4 => {
            zero_guard(b, c, flag_ptr, TRAP_REM_ZERO);
            let c = safe_divisor(b, a, c);
            b.ins().srem(a, c)
        }
        5 => b.ins().band(a, c),
        6 => b.ins().bor(a, c),
        7 => b.ins().bxor(a, c),
        8 => {
            let k = b.ins().band_imm_u(c, if wide { 0x3f } else { 0x1f });
            b.ins().ishl(a, k)
        }
        9 => {
            let k = b.ins().band_imm_u(c, if wide { 0x3f } else { 0x1f });
            b.ins().sshr(a, k)
        }
        _ => {
            let k = b.ins().band_imm_u(c, if wide { 0x3f } else { 0x1f });
            b.ins().ushr(a, k)
        }
    };
    let v = if wide {
        v
    } else {
        b.ins().sextend(types::I64, v)
    };
    b.def_var(vars[dst], v);
}

/// Emit an int literal binop (0xd0..=0xe2 space).
fn emit_lit_binop(
    b: &mut FunctionBuilder,
    op: u8,
    dst: usize,
    src: usize,
    lit: i32,
    vars: &[Variable],
    flag_ptr: Value,
) {
    let s = b.use_var(vars[src]);
    let a = b.ins().ireduce(types::I32, s);
    let c = b.ins().iconst(types::I32, lit as i64);
    let v = match op {
        0xd0 | 0xd8 => b.ins().iadd(a, c),
        // rsub-int: lit - reg
        0xd1 | 0xd9 => b.ins().isub(c, a),
        0xd2 | 0xda => b.ins().imul(a, c),
        0xd3 | 0xdb => {
            zero_guard(b, c, flag_ptr, TRAP_DIV_ZERO);
            let c = safe_divisor(b, a, c);
            b.ins().sdiv(a, c)
        }
        0xd4 | 0xdc => {
            zero_guard(b, c, flag_ptr, TRAP_REM_ZERO);
            let c = safe_divisor(b, a, c);
            b.ins().srem(a, c)
        }
        0xd5 | 0xdd => b.ins().band(a, c),
        0xd6 | 0xde => b.ins().bor(a, c),
        0xd7 | 0xdf => b.ins().bxor(a, c),
        0xe0 => {
            let k = b.ins().band_imm_u(c, 0x1f);
            b.ins().ishl(a, k)
        }
        0xe1 => {
            let k = b.ins().band_imm_u(c, 0x1f);
            b.ins().sshr(a, k)
        }
        _ => {
            let k = b.ins().band_imm_u(c, 0x1f);
            b.ins().ushr(a, k)
        }
    };
    let v = b.ins().sextend(types::I64, v);
    b.def_var(vars[dst], v);
}
