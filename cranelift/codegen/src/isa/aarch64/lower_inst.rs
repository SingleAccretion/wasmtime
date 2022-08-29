//! Lower a single Cranelift instruction into vcode.

use super::lower::*;
use crate::binemit::CodeOffset;
use crate::ir::types::*;
use crate::ir::Inst as IRInst;
use crate::ir::Opcode;
use crate::isa::aarch64::inst::*;
use crate::isa::aarch64::settings as aarch64_settings;
use crate::machinst::lower::*;
use crate::machinst::*;
use crate::settings::{Flags, TlsModel};
use crate::{CodegenError, CodegenResult};
use alloc::boxed::Box;
use alloc::vec::Vec;
use target_lexicon::Triple;

/// Actually codegen an instruction's results into registers.
pub(crate) fn lower_insn_to_regs(
    ctx: &mut Lower<Inst>,
    insn: IRInst,
    triple: &Triple,
    flags: &Flags,
    isa_flags: &aarch64_settings::Flags,
) -> CodegenResult<()> {
    let op = ctx.data(insn).opcode();
    let inputs = insn_inputs(ctx, insn);
    let outputs = insn_outputs(ctx, insn);
    let ty = if outputs.len() > 0 {
        Some(ctx.output_ty(insn, 0))
    } else {
        None
    };

    if let Ok(()) = super::lower::isle::lower(ctx, triple, flags, isa_flags, &outputs, insn) {
        return Ok(());
    }

    let implemented_in_isle = |ctx: &mut Lower<Inst>| -> ! {
        unreachable!(
            "implemented in ISLE: inst = `{}`, type = `{:?}`",
            ctx.dfg().display_inst(insn),
            ty
        );
    };

    match op {
        Opcode::Iconst | Opcode::Bconst | Opcode::Null => implemented_in_isle(ctx),

        Opcode::F32const | Opcode::F64const => unreachable!(
            "Should never see constant ops at top level lowering entry
            point, as constants are rematerialized at use-sites"
        ),

        Opcode::GetFramePointer | Opcode::GetStackPointer | Opcode::GetReturnAddress => {
            implemented_in_isle(ctx)
        }

        Opcode::Iadd => implemented_in_isle(ctx),
        Opcode::Isub => implemented_in_isle(ctx),
        Opcode::UaddSat | Opcode::SaddSat | Opcode::UsubSat | Opcode::SsubSat => {
            implemented_in_isle(ctx)
        }

        Opcode::Ineg => implemented_in_isle(ctx),

        Opcode::Imul => implemented_in_isle(ctx),

        Opcode::Umulhi | Opcode::Smulhi => implemented_in_isle(ctx),

        Opcode::Udiv | Opcode::Sdiv | Opcode::Urem | Opcode::Srem => implemented_in_isle(ctx),

        Opcode::Uextend | Opcode::Sextend => implemented_in_isle(ctx),

        Opcode::Bnot => implemented_in_isle(ctx),

        Opcode::Band
        | Opcode::Bor
        | Opcode::Bxor
        | Opcode::BandNot
        | Opcode::BorNot
        | Opcode::BxorNot => implemented_in_isle(ctx),

        Opcode::Ishl | Opcode::Ushr | Opcode::Sshr => implemented_in_isle(ctx),

        Opcode::Rotr | Opcode::Rotl => implemented_in_isle(ctx),

        Opcode::Bitrev | Opcode::Clz | Opcode::Cls | Opcode::Ctz => implemented_in_isle(ctx),

        Opcode::Popcnt => implemented_in_isle(ctx),

        Opcode::Load
        | Opcode::Uload8
        | Opcode::Sload8
        | Opcode::Uload16
        | Opcode::Sload16
        | Opcode::Uload32
        | Opcode::Sload32
        | Opcode::Sload8x8
        | Opcode::Uload8x8
        | Opcode::Sload16x4
        | Opcode::Uload16x4
        | Opcode::Sload32x2
        | Opcode::Uload32x2 => {
            let sign_extend = match op {
                Opcode::Sload8 | Opcode::Sload16 | Opcode::Sload32 => true,
                _ => false,
            };
            let flags = ctx
                .memflags(insn)
                .expect("Load instruction should have memflags");

            let out_ty = ctx.output_ty(insn, 0);
            if out_ty == I128 {
                let off = ctx.data(insn).load_store_offset().unwrap();
                let mem = lower_pair_address(ctx, &inputs[..], off);
                let dst = get_output_reg(ctx, outputs[0]);
                ctx.emit(Inst::LoadP64 {
                    rt: dst.regs()[0],
                    rt2: dst.regs()[1],
                    mem,
                    flags,
                });
            } else {
                lower_load(
                    ctx,
                    insn,
                    &inputs[..],
                    outputs[0],
                    |ctx, dst, mut elem_ty, mem| {
                        if elem_ty.is_dynamic_vector() {
                            elem_ty = dynamic_to_fixed(elem_ty);
                        }
                        let rd = dst.only_reg().unwrap();
                        let is_float = ty_has_float_or_vec_representation(elem_ty);
                        ctx.emit(match (ty_bits(elem_ty), sign_extend, is_float) {
                            (1, _, _) => Inst::ULoad8 { rd, mem, flags },
                            (8, false, _) => Inst::ULoad8 { rd, mem, flags },
                            (8, true, _) => Inst::SLoad8 { rd, mem, flags },
                            (16, false, _) => Inst::ULoad16 { rd, mem, flags },
                            (16, true, _) => Inst::SLoad16 { rd, mem, flags },
                            (32, false, false) => Inst::ULoad32 { rd, mem, flags },
                            (32, true, false) => Inst::SLoad32 { rd, mem, flags },
                            (32, _, true) => Inst::FpuLoad32 { rd, mem, flags },
                            (64, _, false) => Inst::ULoad64 { rd, mem, flags },
                            // Note that we treat some of the vector loads as scalar floating-point loads,
                            // which is correct in a little endian environment.
                            (64, _, true) => Inst::FpuLoad64 { rd, mem, flags },
                            (128, _, true) => Inst::FpuLoad128 { rd, mem, flags },
                            _ => {
                                return Err(CodegenError::Unsupported(format!(
                                    "Unsupported type in load: {:?}",
                                    elem_ty
                                )))
                            }
                        });

                        let vec_extend = match op {
                            Opcode::Sload8x8 => Some((VecExtendOp::Sxtl, ScalarSize::Size16)),
                            Opcode::Uload8x8 => Some((VecExtendOp::Uxtl, ScalarSize::Size16)),
                            Opcode::Sload16x4 => Some((VecExtendOp::Sxtl, ScalarSize::Size32)),
                            Opcode::Uload16x4 => Some((VecExtendOp::Uxtl, ScalarSize::Size32)),
                            Opcode::Sload32x2 => Some((VecExtendOp::Sxtl, ScalarSize::Size64)),
                            Opcode::Uload32x2 => Some((VecExtendOp::Uxtl, ScalarSize::Size64)),
                            _ => None,
                        };

                        if let Some((t, lane_size)) = vec_extend {
                            let rd = dst.only_reg().unwrap();
                            ctx.emit(Inst::VecExtend {
                                t,
                                rd,
                                rn: rd.to_reg(),
                                high_half: false,
                                lane_size,
                            });
                        }

                        Ok(())
                    },
                )?;
            }
        }

        Opcode::Store | Opcode::Istore8 | Opcode::Istore16 | Opcode::Istore32 => {
            let off = ctx.data(insn).load_store_offset().unwrap();
            let mut elem_ty = match op {
                Opcode::Istore8 => I8,
                Opcode::Istore16 => I16,
                Opcode::Istore32 => I32,
                Opcode::Store => ctx.input_ty(insn, 0),
                _ => unreachable!(),
            };
            let is_float = ty_has_float_or_vec_representation(elem_ty);
            let flags = ctx
                .memflags(insn)
                .expect("Store instruction should have memflags");

            let dst = put_input_in_regs(ctx, inputs[0]);

            if elem_ty == I128 {
                let mem = lower_pair_address(ctx, &inputs[1..], off);
                ctx.emit(Inst::StoreP64 {
                    rt: dst.regs()[0],
                    rt2: dst.regs()[1],
                    mem,
                    flags,
                });
            } else {
                if elem_ty.is_dynamic_vector() {
                    elem_ty = dynamic_to_fixed(elem_ty);
                }
                let rd = dst.only_reg().unwrap();
                let mem = lower_address(ctx, elem_ty, &inputs[1..], off);
                ctx.emit(match (ty_bits(elem_ty), is_float) {
                    (1, _) | (8, _) => Inst::Store8 { rd, mem, flags },
                    (16, _) => Inst::Store16 { rd, mem, flags },
                    (32, false) => Inst::Store32 { rd, mem, flags },
                    (32, true) => Inst::FpuStore32 { rd, mem, flags },
                    (64, false) => Inst::Store64 { rd, mem, flags },
                    (64, true) => Inst::FpuStore64 { rd, mem, flags },
                    (128, _) => Inst::FpuStore128 { rd, mem, flags },
                    _ => {
                        return Err(CodegenError::Unsupported(format!(
                            "Unsupported type in store: {:?}",
                            elem_ty
                        )))
                    }
                });
            }
        }

        Opcode::StackAddr => implemented_in_isle(ctx),

        Opcode::DynamicStackAddr => implemented_in_isle(ctx),

        Opcode::AtomicRmw => implemented_in_isle(ctx),

        Opcode::AtomicCas => implemented_in_isle(ctx),

        Opcode::AtomicLoad => implemented_in_isle(ctx),

        Opcode::AtomicStore => implemented_in_isle(ctx),

        Opcode::Fence => implemented_in_isle(ctx),

        Opcode::StackLoad
        | Opcode::StackStore
        | Opcode::DynamicStackStore
        | Opcode::DynamicStackLoad => {
            panic!("Direct stack memory access not supported; should not be used by Wasm");
        }

        Opcode::HeapAddr => {
            panic!("heap_addr should have been removed by legalization!");
        }

        Opcode::TableAddr => {
            panic!("table_addr should have been removed by legalization!");
        }

        Opcode::Nop => {
            // Nothing.
        }

        Opcode::Select => {
            let flag_input = inputs[0];
            let cond = if let Some(icmp_insn) =
                maybe_input_insn_via_conv(ctx, flag_input, Opcode::Icmp, Opcode::Bint)
            {
                let condcode = ctx.data(icmp_insn).cond_code().unwrap();
                lower_icmp(ctx, icmp_insn, condcode, IcmpOutput::CondCode)?.unwrap_cond()
            } else if let Some(fcmp_insn) =
                maybe_input_insn_via_conv(ctx, flag_input, Opcode::Fcmp, Opcode::Bint)
            {
                let condcode = ctx.data(fcmp_insn).fp_cond_code().unwrap();
                let cond = lower_fp_condcode(condcode);
                lower_fcmp_or_ffcmp_to_flags(ctx, fcmp_insn);
                cond
            } else {
                let (size, narrow_mode) = if ty_bits(ctx.input_ty(insn, 0)) > 32 {
                    (OperandSize::Size64, NarrowValueMode::ZeroExtend64)
                } else {
                    (OperandSize::Size32, NarrowValueMode::ZeroExtend32)
                };

                let rcond = put_input_in_reg(ctx, inputs[0], narrow_mode);
                // cmp rcond, #0
                ctx.emit(Inst::AluRRR {
                    alu_op: ALUOp::SubS,
                    size,
                    rd: writable_zero_reg(),
                    rn: rcond,
                    rm: zero_reg(),
                });
                Cond::Ne
            };

            // csel.cond rd, rn, rm
            let ty = ctx.output_ty(insn, 0);
            let bits = ty_bits(ty);
            let is_float = ty_has_float_or_vec_representation(ty);

            let dst = get_output_reg(ctx, outputs[0]);
            let lhs = put_input_in_regs(ctx, inputs[1]);
            let rhs = put_input_in_regs(ctx, inputs[2]);

            let rd = dst.regs()[0];
            let rn = lhs.regs()[0];
            let rm = rhs.regs()[0];

            match (is_float, bits) {
                (true, 32) => ctx.emit(Inst::FpuCSel32 { cond, rd, rn, rm }),
                (true, 64) => ctx.emit(Inst::FpuCSel64 { cond, rd, rn, rm }),
                (true, 128) => ctx.emit(Inst::VecCSel { cond, rd, rn, rm }),
                (false, 128) => {
                    ctx.emit(Inst::CSel {
                        cond,
                        rd: dst.regs()[0],
                        rn: lhs.regs()[0],
                        rm: rhs.regs()[0],
                    });
                    ctx.emit(Inst::CSel {
                        cond,
                        rd: dst.regs()[1],
                        rn: lhs.regs()[1],
                        rm: rhs.regs()[1],
                    });
                }
                (false, bits) if bits <= 64 => ctx.emit(Inst::CSel { cond, rd, rn, rm }),
                _ => {
                    return Err(CodegenError::Unsupported(format!(
                        "Select: Unsupported type: {:?}",
                        ty
                    )));
                }
            }
        }

        Opcode::Selectif | Opcode::SelectifSpectreGuard => {
            let condcode = ctx.data(insn).cond_code().unwrap();
            // Verification ensures that the input is always a
            // single-def ifcmp.
            let ifcmp_insn = maybe_input_insn(ctx, inputs[0], Opcode::Ifcmp).unwrap();
            let cond = lower_icmp(ctx, ifcmp_insn, condcode, IcmpOutput::CondCode)?.unwrap_cond();

            // csel.COND rd, rn, rm
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let rm = put_input_in_reg(ctx, inputs[2], NarrowValueMode::None);
            let ty = ctx.output_ty(insn, 0);
            let bits = ty_bits(ty);
            let is_float = ty_has_float_or_vec_representation(ty);
            if is_float && bits == 32 {
                ctx.emit(Inst::FpuCSel32 { cond, rd, rn, rm });
            } else if is_float && bits == 64 {
                ctx.emit(Inst::FpuCSel64 { cond, rd, rn, rm });
            } else if !is_float && bits <= 64 {
                ctx.emit(Inst::CSel { cond, rd, rn, rm });
            } else {
                return Err(CodegenError::Unsupported(format!(
                    "{}: Unsupported type: {:?}",
                    op, ty
                )));
            }

            if op == Opcode::SelectifSpectreGuard {
                ctx.emit(Inst::Csdb);
            }
        }

        Opcode::Bitselect | Opcode::Vselect => implemented_in_isle(ctx),

        Opcode::Trueif => {
            let condcode = ctx.data(insn).cond_code().unwrap();
            // Verification ensures that the input is always a
            // single-def ifcmp.
            let ifcmp_insn = maybe_input_insn(ctx, inputs[0], Opcode::Ifcmp).unwrap();
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            lower_icmp(ctx, ifcmp_insn, condcode, IcmpOutput::Register(rd))?;
        }

        Opcode::Trueff => {
            let condcode = ctx.data(insn).fp_cond_code().unwrap();
            let cond = lower_fp_condcode(condcode);
            let ffcmp_insn = maybe_input_insn(ctx, inputs[0], Opcode::Ffcmp).unwrap();
            lower_fcmp_or_ffcmp_to_flags(ctx, ffcmp_insn);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            materialize_bool_result(ctx, insn, rd, cond);
        }

        Opcode::IsNull | Opcode::IsInvalid => implemented_in_isle(ctx),

        Opcode::Copy => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let ty = ctx.input_ty(insn, 0);
            ctx.emit(Inst::gen_move(rd, rn, ty));
        }

        Opcode::Breduce | Opcode::Ireduce => implemented_in_isle(ctx),

        Opcode::Bextend | Opcode::Bmask => implemented_in_isle(ctx),

        Opcode::Bint => implemented_in_isle(ctx),

        Opcode::Bitcast => implemented_in_isle(ctx),

        Opcode::Return => implemented_in_isle(ctx),

        Opcode::Ifcmp | Opcode::Ffcmp => {
            // An Ifcmp/Ffcmp must always be seen as a use of a brif/brff or trueif/trueff
            // instruction. This will always be the case as long as the IR uses an Ifcmp/Ffcmp from
            // the same block, or a dominating block. In other words, it cannot pass through a BB
            // param (phi). The flags pass of the verifier will ensure this.
            panic!("Should never reach ifcmp as isel root!");
        }

        Opcode::Icmp => {
            let condcode = ctx.data(insn).cond_code().unwrap();
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            lower_icmp(ctx, insn, condcode, IcmpOutput::Register(rd))?;
        }

        Opcode::Fcmp => {
            let condcode = ctx.data(insn).fp_cond_code().unwrap();
            let cond = lower_fp_condcode(condcode);
            let ty = ctx.input_ty(insn, 0);
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

            if !ty.is_vector() {
                ctx.emit(Inst::FpuCmp {
                    size: ScalarSize::from_ty(ty),
                    rn,
                    rm,
                });
                materialize_bool_result(ctx, insn, rd, cond);
            } else {
                lower_vector_compare(ctx, rd, rn, rm, ty, cond)?;
            }
        }

        Opcode::Debugtrap => implemented_in_isle(ctx),

        Opcode::Trap | Opcode::ResumableTrap => implemented_in_isle(ctx),

        Opcode::Trapif | Opcode::Trapff => {
            let trap_code = ctx.data(insn).trap_code().unwrap();

            let cond = if maybe_input_insn(ctx, inputs[0], Opcode::IaddIfcout).is_some() {
                let condcode = ctx.data(insn).cond_code().unwrap();
                let cond = lower_condcode(condcode);
                // The flags must not have been clobbered by any other
                // instruction between the iadd_ifcout and this instruction, as
                // verified by the CLIF validator; so we can simply use the
                // flags here.
                cond
            } else if op == Opcode::Trapif {
                let condcode = ctx.data(insn).cond_code().unwrap();

                // Verification ensures that the input is always a single-def ifcmp.
                let ifcmp_insn = maybe_input_insn(ctx, inputs[0], Opcode::Ifcmp).unwrap();
                lower_icmp(ctx, ifcmp_insn, condcode, IcmpOutput::CondCode)?.unwrap_cond()
            } else {
                let condcode = ctx.data(insn).fp_cond_code().unwrap();
                let cond = lower_fp_condcode(condcode);

                // Verification ensures that the input is always a
                // single-def ffcmp.
                let ffcmp_insn = maybe_input_insn(ctx, inputs[0], Opcode::Ffcmp).unwrap();
                lower_fcmp_or_ffcmp_to_flags(ctx, ffcmp_insn);
                cond
            };

            ctx.emit(Inst::TrapIf {
                trap_code,
                kind: CondBrKind::Cond(cond),
            });
        }

        Opcode::Trapz | Opcode::Trapnz | Opcode::ResumableTrapnz => {
            panic!("trapz / trapnz / resumable_trapnz should have been removed by legalization!");
        }

        Opcode::FuncAddr => implemented_in_isle(ctx),

        Opcode::GlobalValue => {
            panic!("global_value should have been removed by legalization!");
        }

        Opcode::SymbolValue => implemented_in_isle(ctx),

        Opcode::Call | Opcode::CallIndirect => implemented_in_isle(ctx),

        Opcode::GetPinnedReg | Opcode::SetPinnedReg => implemented_in_isle(ctx),

        Opcode::Jump
        | Opcode::Brz
        | Opcode::Brnz
        | Opcode::BrIcmp
        | Opcode::Brif
        | Opcode::Brff
        | Opcode::BrTable => {
            panic!("Branch opcode reached non-branch lowering logic!");
        }

        Opcode::Vconst => implemented_in_isle(ctx),

        Opcode::RawBitcast => implemented_in_isle(ctx),

        Opcode::Extractlane => implemented_in_isle(ctx),

        Opcode::Insertlane => implemented_in_isle(ctx),

        Opcode::Splat => implemented_in_isle(ctx),

        Opcode::ScalarToVector => implemented_in_isle(ctx),

        Opcode::VallTrue | Opcode::VanyTrue => implemented_in_isle(ctx),

        Opcode::VhighBits => implemented_in_isle(ctx),

        Opcode::Shuffle => implemented_in_isle(ctx),

        Opcode::Swizzle => implemented_in_isle(ctx),

        Opcode::Isplit => implemented_in_isle(ctx),

        Opcode::Iconcat => implemented_in_isle(ctx),

        Opcode::Imax | Opcode::Umax | Opcode::Umin | Opcode::Imin => implemented_in_isle(ctx),

        Opcode::IaddPairwise => implemented_in_isle(ctx),

        Opcode::WideningPairwiseDotProductS => implemented_in_isle(ctx),

        Opcode::Fadd | Opcode::Fsub | Opcode::Fmul | Opcode::Fdiv | Opcode::Fmin | Opcode::Fmax => {
            implemented_in_isle(ctx)
        }

        Opcode::FminPseudo | Opcode::FmaxPseudo => implemented_in_isle(ctx),

        Opcode::Sqrt | Opcode::Fneg | Opcode::Fabs | Opcode::Fpromote | Opcode::Fdemote => {
            implemented_in_isle(ctx)
        }

        Opcode::Ceil | Opcode::Floor | Opcode::Trunc | Opcode::Nearest => implemented_in_isle(ctx),

        Opcode::Fma => implemented_in_isle(ctx),

        Opcode::Fcopysign => implemented_in_isle(ctx),

        Opcode::FcvtToUint | Opcode::FcvtToSint => implemented_in_isle(ctx),

        Opcode::FcvtFromUint | Opcode::FcvtFromSint => implemented_in_isle(ctx),

        Opcode::FcvtToUintSat | Opcode::FcvtToSintSat => implemented_in_isle(ctx),

        Opcode::IaddIfcout => implemented_in_isle(ctx),

        Opcode::IaddImm
        | Opcode::ImulImm
        | Opcode::UdivImm
        | Opcode::SdivImm
        | Opcode::UremImm
        | Opcode::SremImm
        | Opcode::IrsubImm
        | Opcode::IaddCin
        | Opcode::IaddIfcin
        | Opcode::IaddCout
        | Opcode::IaddCarry
        | Opcode::IaddIfcarry
        | Opcode::IsubBin
        | Opcode::IsubIfbin
        | Opcode::IsubBout
        | Opcode::IsubIfbout
        | Opcode::IsubBorrow
        | Opcode::IsubIfborrow
        | Opcode::BandImm
        | Opcode::BorImm
        | Opcode::BxorImm
        | Opcode::RotlImm
        | Opcode::RotrImm
        | Opcode::IshlImm
        | Opcode::UshrImm
        | Opcode::SshrImm
        | Opcode::IcmpImm
        | Opcode::IfcmpImm => {
            panic!("ALU+imm and ALU+carry ops should not appear here!");
        }

        Opcode::Iabs => implemented_in_isle(ctx),
        Opcode::AvgRound => implemented_in_isle(ctx),

        Opcode::Snarrow | Opcode::Unarrow | Opcode::Uunarrow => implemented_in_isle(ctx),

        Opcode::SwidenLow | Opcode::SwidenHigh | Opcode::UwidenLow | Opcode::UwidenHigh => {
            implemented_in_isle(ctx)
        }

        Opcode::TlsValue => match flags.tls_model() {
            TlsModel::ElfGd => {
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                let (name, _, _) = ctx.symbol_value(insn).unwrap();
                let symbol = name.clone();
                ctx.emit(Inst::ElfTlsGetAddr { symbol });

                let x0 = xreg(0);
                ctx.emit(Inst::gen_move(dst, x0, I64));
            }
            _ => {
                return Err(CodegenError::Unsupported(format!(
                    "Unimplemented TLS model in AArch64 backend: {:?}",
                    flags.tls_model()
                )));
            }
        },

        Opcode::SqmulRoundSat => implemented_in_isle(ctx),

        Opcode::FcvtLowFromSint => implemented_in_isle(ctx),

        Opcode::FvpromoteLow => implemented_in_isle(ctx),

        Opcode::Fvdemote => implemented_in_isle(ctx),

        Opcode::ExtractVector => implemented_in_isle(ctx),

        Opcode::ConstAddr | Opcode::Vconcat | Opcode::Vsplit => {
            return Err(CodegenError::Unsupported(format!(
                "Unimplemented lowering: {}",
                op
            )));
        }
    }

    Ok(())
}

pub(crate) fn lower_branch(
    ctx: &mut Lower<Inst>,
    branches: &[IRInst],
    targets: &[MachLabel],
) -> CodegenResult<()> {
    // A block should end with at most two branches. The first may be a
    // conditional branch; a conditional branch can be followed only by an
    // unconditional branch or fallthrough. Otherwise, if only one branch,
    // it may be an unconditional branch, a fallthrough, a return, or a
    // trap. These conditions are verified by `is_ebb_basic()` during the
    // verifier pass.
    assert!(branches.len() <= 2);

    if branches.len() == 2 {
        // Must be a conditional branch followed by an unconditional branch.
        let op0 = ctx.data(branches[0]).opcode();
        let op1 = ctx.data(branches[1]).opcode();

        assert!(op1 == Opcode::Jump);
        let taken = BranchTarget::Label(targets[0]);
        // not_taken target is the target of the second branch, even if it is a Fallthrough
        // instruction: because we reorder blocks while we lower, the fallthrough in the new
        // order is not (necessarily) the same as the fallthrough in CLIF. So we use the
        // explicitly-provided target.
        let not_taken = BranchTarget::Label(targets[1]);

        match op0 {
            Opcode::Brz | Opcode::Brnz => {
                let ty = ctx.input_ty(branches[0], 0);
                let flag_input = InsnInput {
                    insn: branches[0],
                    input: 0,
                };
                if let Some(icmp_insn) =
                    maybe_input_insn_via_conv(ctx, flag_input, Opcode::Icmp, Opcode::Bint)
                {
                    let condcode = ctx.data(icmp_insn).cond_code().unwrap();
                    let cond =
                        lower_icmp(ctx, icmp_insn, condcode, IcmpOutput::CondCode)?.unwrap_cond();
                    let negated = op0 == Opcode::Brz;
                    let cond = if negated { cond.invert() } else { cond };

                    ctx.emit(Inst::CondBr {
                        taken,
                        not_taken,
                        kind: CondBrKind::Cond(cond),
                    });
                } else if let Some(fcmp_insn) =
                    maybe_input_insn_via_conv(ctx, flag_input, Opcode::Fcmp, Opcode::Bint)
                {
                    let condcode = ctx.data(fcmp_insn).fp_cond_code().unwrap();
                    let cond = lower_fp_condcode(condcode);
                    let negated = op0 == Opcode::Brz;
                    let cond = if negated { cond.invert() } else { cond };

                    lower_fcmp_or_ffcmp_to_flags(ctx, fcmp_insn);
                    ctx.emit(Inst::CondBr {
                        taken,
                        not_taken,
                        kind: CondBrKind::Cond(cond),
                    });
                } else {
                    let rt = if ty == I128 {
                        let tmp = ctx.alloc_tmp(I64).only_reg().unwrap();
                        let input = put_input_in_regs(ctx, flag_input);
                        ctx.emit(Inst::AluRRR {
                            alu_op: ALUOp::Orr,
                            size: OperandSize::Size64,
                            rd: tmp,
                            rn: input.regs()[0],
                            rm: input.regs()[1],
                        });
                        tmp.to_reg()
                    } else {
                        put_input_in_reg(ctx, flag_input, NarrowValueMode::ZeroExtend64)
                    };
                    let kind = match op0 {
                        Opcode::Brz => CondBrKind::Zero(rt),
                        Opcode::Brnz => CondBrKind::NotZero(rt),
                        _ => unreachable!(),
                    };
                    ctx.emit(Inst::CondBr {
                        taken,
                        not_taken,
                        kind,
                    });
                }
            }
            Opcode::BrIcmp => {
                let condcode = ctx.data(branches[0]).cond_code().unwrap();
                let cond =
                    lower_icmp(ctx, branches[0], condcode, IcmpOutput::CondCode)?.unwrap_cond();

                ctx.emit(Inst::CondBr {
                    taken,
                    not_taken,
                    kind: CondBrKind::Cond(cond),
                });
            }

            Opcode::Brif => {
                let condcode = ctx.data(branches[0]).cond_code().unwrap();

                let flag_input = InsnInput {
                    insn: branches[0],
                    input: 0,
                };
                if let Some(ifcmp_insn) = maybe_input_insn(ctx, flag_input, Opcode::Ifcmp) {
                    let cond =
                        lower_icmp(ctx, ifcmp_insn, condcode, IcmpOutput::CondCode)?.unwrap_cond();
                    ctx.emit(Inst::CondBr {
                        taken,
                        not_taken,
                        kind: CondBrKind::Cond(cond),
                    });
                } else {
                    // If the ifcmp result is actually placed in a
                    // register, we need to move it back into the flags.
                    let rn = put_input_in_reg(ctx, flag_input, NarrowValueMode::None);
                    ctx.emit(Inst::MovToNZCV { rn });
                    ctx.emit(Inst::CondBr {
                        taken,
                        not_taken,
                        kind: CondBrKind::Cond(lower_condcode(condcode)),
                    });
                }
            }

            Opcode::Brff => {
                let condcode = ctx.data(branches[0]).fp_cond_code().unwrap();
                let cond = lower_fp_condcode(condcode);
                let kind = CondBrKind::Cond(cond);
                let flag_input = InsnInput {
                    insn: branches[0],
                    input: 0,
                };
                if let Some(ffcmp_insn) = maybe_input_insn(ctx, flag_input, Opcode::Ffcmp) {
                    lower_fcmp_or_ffcmp_to_flags(ctx, ffcmp_insn);
                    ctx.emit(Inst::CondBr {
                        taken,
                        not_taken,
                        kind,
                    });
                } else {
                    // If the ffcmp result is actually placed in a
                    // register, we need to move it back into the flags.
                    let rn = put_input_in_reg(ctx, flag_input, NarrowValueMode::None);
                    ctx.emit(Inst::MovToNZCV { rn });
                    ctx.emit(Inst::CondBr {
                        taken,
                        not_taken,
                        kind,
                    });
                }
            }

            _ => unimplemented!(),
        }
    } else {
        // Must be an unconditional branch or an indirect branch.
        let op = ctx.data(branches[0]).opcode();
        match op {
            Opcode::Jump => {
                assert!(branches.len() == 1);
                ctx.emit(Inst::Jump {
                    dest: BranchTarget::Label(targets[0]),
                });
            }

            Opcode::BrTable => {
                // Expand `br_table index, default, JT` to:
                //
                //   emit_island  // this forces an island at this point
                //                // if the jumptable would push us past
                //                // the deadline
                //   subs idx, #jt_size
                //   b.hs default
                //   adr vTmp1, PC+16
                //   ldr vTmp2, [vTmp1, idx, lsl #2]
                //   add vTmp2, vTmp2, vTmp1
                //   br vTmp2
                //   [jumptable offsets relative to JT base]
                let jt_size = targets.len() - 1;
                assert!(jt_size <= std::u32::MAX as usize);

                ctx.emit(Inst::EmitIsland {
                    needed_space: 4 * (6 + jt_size) as CodeOffset,
                });

                let ridx = put_input_in_reg(
                    ctx,
                    InsnInput {
                        insn: branches[0],
                        input: 0,
                    },
                    NarrowValueMode::ZeroExtend32,
                );

                let rtmp1 = ctx.alloc_tmp(I32).only_reg().unwrap();
                let rtmp2 = ctx.alloc_tmp(I32).only_reg().unwrap();

                // Bounds-check, leaving condition codes for JTSequence's
                // branch to default target below.
                if let Some(imm12) = Imm12::maybe_from_u64(jt_size as u64) {
                    ctx.emit(Inst::AluRRImm12 {
                        alu_op: ALUOp::SubS,
                        size: OperandSize::Size32,
                        rd: writable_zero_reg(),
                        rn: ridx,
                        imm12,
                    });
                } else {
                    lower_constant_u64(ctx, rtmp1, jt_size as u64);
                    ctx.emit(Inst::AluRRR {
                        alu_op: ALUOp::SubS,
                        size: OperandSize::Size32,
                        rd: writable_zero_reg(),
                        rn: ridx,
                        rm: rtmp1.to_reg(),
                    });
                }

                // Emit the compound instruction that does:
                //
                // b.hs default
                // adr rA, jt
                // ldrsw rB, [rA, rIndex, UXTW 2]
                // add rA, rA, rB
                // br rA
                // [jt entries]
                //
                // This must be *one* instruction in the vcode because
                // we cannot allow regalloc to insert any spills/fills
                // in the middle of the sequence; otherwise, the ADR's
                // PC-rel offset to the jumptable would be incorrect.
                // (The alternative is to introduce a relocation pass
                // for inlined jumptables, which is much worse, IMHO.)

                let jt_targets: Vec<BranchTarget> = targets
                    .iter()
                    .skip(1)
                    .map(|bix| BranchTarget::Label(*bix))
                    .collect();
                let default_target = BranchTarget::Label(targets[0]);
                ctx.emit(Inst::JTSequence {
                    ridx,
                    rtmp1,
                    rtmp2,
                    info: Box::new(JTSequenceInfo {
                        targets: jt_targets,
                        default_target,
                    }),
                });
            }

            _ => panic!("Unknown branch type!"),
        }
    }

    Ok(())
}
