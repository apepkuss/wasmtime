//! Assembler library implementation for x64.

use crate::{
    isa::reg::Reg,
    masm::{DivKind, IntCmpKind, OperandSize, RemKind, RoundingMode, ShiftKind},
};
use cranelift_codegen::{
    entity::EntityRef,
    ir::{types, ConstantPool, ExternalName, LibCall, Opcode, TrapCode, UserExternalNameRef},
    isa::{
        x64::{
            args::{
                self, AluRmiROpcode, Amode, CmpOpcode, DivSignedness, ExtMode, FromWritableReg,
                Gpr, GprMem, GprMemImm, Imm8Gpr, Imm8Reg, RegMem, RegMemImm,
                ShiftKind as CraneliftShiftKind, SseOpcode, SyntheticAmode, WritableGpr,
                WritableXmm, Xmm, XmmMem, XmmMemAligned, CC,
            },
            settings as x64_settings, CallInfo, EmitInfo, EmitState, Inst,
        },
        CallConv,
    },
    settings, Final, MachBuffer, MachBufferFinalized, MachInstEmit, MachInstEmitState, MachLabel,
    VCodeConstantData, VCodeConstants, Writable,
};

use super::address::Address;
use smallvec::{smallvec, SmallVec};

// Conversions between winch-codegen x64 types and cranelift-codegen x64 types.

impl From<Reg> for RegMemImm {
    fn from(reg: Reg) -> Self {
        RegMemImm::reg(reg.into())
    }
}

impl From<Reg> for RegMem {
    fn from(value: Reg) -> Self {
        RegMem::Reg { reg: value.into() }
    }
}

impl From<Reg> for WritableGpr {
    fn from(reg: Reg) -> Self {
        let writable = Writable::from_reg(reg.into());
        WritableGpr::from_writable_reg(writable).expect("valid writable gpr")
    }
}

impl From<Reg> for WritableXmm {
    fn from(reg: Reg) -> Self {
        let writable = Writable::from_reg(reg.into());
        WritableXmm::from_writable_reg(writable).expect("valid writable xmm")
    }
}

impl From<Reg> for Gpr {
    fn from(reg: Reg) -> Self {
        Gpr::new(reg.into()).expect("valid gpr")
    }
}

impl From<Reg> for GprMem {
    fn from(value: Reg) -> Self {
        GprMem::new(value.into()).expect("valid gpr")
    }
}

impl From<Reg> for GprMemImm {
    fn from(reg: Reg) -> Self {
        GprMemImm::new(reg.into()).expect("valid gpr")
    }
}

impl From<Reg> for Imm8Gpr {
    fn from(value: Reg) -> Self {
        Imm8Gpr::new(Imm8Reg::Reg { reg: value.into() }).expect("valid Imm8Gpr")
    }
}

impl From<Reg> for Xmm {
    fn from(reg: Reg) -> Self {
        Xmm::new(reg.into()).expect("valid Xmm register")
    }
}

impl From<OperandSize> for args::OperandSize {
    fn from(size: OperandSize) -> Self {
        match size {
            OperandSize::S32 => Self::Size32,
            OperandSize::S64 => Self::Size64,
            s => panic!("Invalid operand size {:?}", s),
        }
    }
}

impl From<DivKind> for DivSignedness {
    fn from(kind: DivKind) -> DivSignedness {
        match kind {
            DivKind::Signed => DivSignedness::Signed,
            DivKind::Unsigned => DivSignedness::Unsigned,
        }
    }
}

impl From<IntCmpKind> for CC {
    fn from(value: IntCmpKind) -> Self {
        match value {
            IntCmpKind::Eq => CC::Z,
            IntCmpKind::Ne => CC::NZ,
            IntCmpKind::LtS => CC::L,
            IntCmpKind::LtU => CC::B,
            IntCmpKind::GtS => CC::NLE,
            IntCmpKind::GtU => CC::NBE,
            IntCmpKind::LeS => CC::LE,
            IntCmpKind::LeU => CC::BE,
            IntCmpKind::GeS => CC::NL,
            IntCmpKind::GeU => CC::NB,
        }
    }
}

impl From<ShiftKind> for CraneliftShiftKind {
    fn from(value: ShiftKind) -> Self {
        match value {
            ShiftKind::Shl => CraneliftShiftKind::ShiftLeft,
            ShiftKind::ShrS => CraneliftShiftKind::ShiftRightArithmetic,
            ShiftKind::ShrU => CraneliftShiftKind::ShiftRightLogical,
            ShiftKind::Rotl => CraneliftShiftKind::RotateLeft,
            ShiftKind::Rotr => CraneliftShiftKind::RotateRight,
        }
    }
}

/// Low level assembler implementation for x64.
pub(crate) struct Assembler {
    /// The machine instruction buffer.
    buffer: MachBuffer<Inst>,
    /// Constant emission information.
    emit_info: EmitInfo,
    /// Emission state.
    emit_state: EmitState,
    /// x64 flags.
    isa_flags: x64_settings::Flags,
    /// Constant pool.
    pool: ConstantPool,
    /// Constants that will be emitted separately by the MachBuffer.
    constants: VCodeConstants,
}

impl Assembler {
    /// Create a new x64 assembler.
    pub fn new(shared_flags: settings::Flags, isa_flags: x64_settings::Flags) -> Self {
        Self {
            buffer: MachBuffer::<Inst>::new(),
            emit_state: Default::default(),
            emit_info: EmitInfo::new(shared_flags, isa_flags.clone()),
            constants: Default::default(),
            pool: ConstantPool::new(),
            isa_flags,
        }
    }

    /// Get a mutable reference to underlying
    /// machine buffer.
    pub fn buffer_mut(&mut self) -> &mut MachBuffer<Inst> {
        &mut self.buffer
    }

    /// Adds a constant to the constant pool and returns its address.
    pub fn add_constant(&mut self, constant: &[u8]) -> Address {
        let handle = self.pool.insert(constant.into());
        Address::constant(handle)
    }

    /// Return the emitted code.
    pub fn finalize(mut self) -> MachBufferFinalized<Final> {
        let stencil = self
            .buffer
            .finish(&self.constants, self.emit_state.ctrl_plane_mut());
        stencil.apply_base_srcloc(Default::default())
    }

    fn emit(&mut self, inst: Inst) {
        inst.emit(&[], &mut self.buffer, &self.emit_info, &mut self.emit_state);
    }

    fn to_synthetic_amode(
        addr: &Address,
        pool: &mut ConstantPool,
        constants: &mut VCodeConstants,
        buffer: &mut MachBuffer<Inst>,
    ) -> SyntheticAmode {
        match addr {
            Address::Offset { base, offset } => {
                SyntheticAmode::real(Amode::imm_reg(*offset as i32, (*base).into()))
            }
            Address::Const(c) => {
                // Defer the creation of the
                // `SyntheticAmode::ConstantOffset` addressing mode
                // until the address is referenced by an actual
                // instrunction.
                let constant_data = pool.get(*c);
                let data = VCodeConstantData::Pool(*c, constant_data.clone());
                // If the constaant data is not marked as used, it will be
                // inserted, therefore, it needs to be registered.
                let needs_registration = !constants.pool_uses(&data);
                let constant = constants.insert(VCodeConstantData::Pool(*c, constant_data.clone()));

                if needs_registration {
                    buffer.register_constant(&constant, &data);
                }
                SyntheticAmode::ConstantOffset(constant)
            }
        }
    }

    /// Push register.
    pub fn push_r(&mut self, reg: Reg) {
        self.emit(Inst::Push64 { src: reg.into() });
    }

    /// Pop to register.
    pub fn pop_r(&mut self, dst: Reg) {
        let writable = Writable::from_reg(dst.into());
        let dst = WritableGpr::from_writable_reg(writable).expect("valid writable gpr");
        self.emit(Inst::Pop64 { dst });
    }

    /// Return instruction.
    pub fn ret(&mut self) {
        self.emit(Inst::Ret {
            stack_bytes_to_pop: 0,
        });
    }

    /// Register-to-register move.
    pub fn mov_rr(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        self.emit(Inst::MovRR {
            src: src.into(),
            dst: dst.into(),
            size: size.into(),
        });
    }

    /// Register-to-memory move.
    pub fn mov_rm(&mut self, src: Reg, addr: &Address, size: OperandSize) {
        assert!(addr.is_offset());
        let dst =
            Self::to_synthetic_amode(addr, &mut self.pool, &mut self.constants, &mut self.buffer);
        self.emit(Inst::MovRM {
            size: size.into(),
            src: src.into(),
            dst,
        });
    }

    /// Immediate-to-memory move.
    pub fn mov_im(&mut self, src: i32, addr: &Address, size: OperandSize) {
        assert!(addr.is_offset());
        let dst =
            Self::to_synthetic_amode(addr, &mut self.pool, &mut self.constants, &mut self.buffer);
        self.emit(Inst::MovImmM {
            size: size.into(),
            simm32: src,
            dst,
        });
    }

    /// Immediate-to-register move.
    pub fn mov_ir(&mut self, imm: u64, dst: Reg, size: OperandSize) {
        let dst = WritableGpr::from_writable_reg(Writable::from_reg(dst.into()))
            .expect("valid writable gpr");

        self.emit(Inst::Imm {
            dst_size: size.into(),
            simm64: imm,
            dst,
        });
    }

    /// Memory-to-register load.
    pub fn mov_mr(&mut self, addr: &Address, dst: Reg, size: OperandSize) {
        use OperandSize::S64;

        let src =
            Self::to_synthetic_amode(addr, &mut self.pool, &mut self.constants, &mut self.buffer);

        if size == S64 {
            self.emit(Inst::Mov64MR {
                src,
                dst: dst.into(),
            });
        } else {
            let reg_mem = RegMem::mem(src);
            self.emit(Inst::MovzxRmR {
                ext_mode: ExtMode::LQ,
                src: GprMem::new(reg_mem).expect("valid memory address"),
                dst: dst.into(),
            });
        }
    }

    /// Integer register conditional move.
    pub fn cmov(&mut self, src: Reg, dst: Reg, cc: IntCmpKind, size: OperandSize) {
        self.emit(Inst::Cmove {
            size: size.into(),
            cc: cc.into(),
            consequent: src.into(),
            alternative: dst.into(),
            dst: dst.into(),
        })
    }

    /// Single and double precision floating point
    /// register-to-register move.
    pub fn xmm_mov_rr(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        use OperandSize::*;

        let op = match size {
            S32 => SseOpcode::Movaps,
            S64 => SseOpcode::Movapd,
            S128 => SseOpcode::Movdqa,
        };

        self.emit(Inst::XmmUnaryRmRUnaligned {
            op,
            src: XmmMem::new(src.into()).expect("valid xmm unaligned"),
            dst: dst.into(),
        });
    }

    /// Single and double precision floating point load.
    pub fn xmm_mov_mr(&mut self, src: &Address, dst: Reg, size: OperandSize) {
        use OperandSize::*;

        assert!(dst.is_float());
        let op = match size {
            S32 => SseOpcode::Movss,
            S64 => SseOpcode::Movsd,
            S128 => SseOpcode::Movdqu,
        };

        let src =
            Self::to_synthetic_amode(src, &mut self.pool, &mut self.constants, &mut self.buffer);
        self.emit(Inst::XmmUnaryRmRUnaligned {
            op,
            src: XmmMem::new(RegMem::mem(src)).expect("valid xmm unaligned"),
            dst: dst.into(),
        });
    }

    /// Single and double precision floating point store.
    pub fn xmm_mov_rm(&mut self, src: Reg, dst: &Address, size: OperandSize) {
        use OperandSize::*;

        assert!(src.is_float());

        let op = match size {
            S32 => SseOpcode::Movss,
            S64 => SseOpcode::Movsd,
            S128 => SseOpcode::Movdqu,
        };

        let dst =
            Self::to_synthetic_amode(dst, &mut self.pool, &mut self.constants, &mut self.buffer);
        self.emit(Inst::XmmMovRM {
            op,
            src: src.into(),
            dst,
        });
    }

    /// Floating point register conditional move.
    pub fn xmm_cmov(&mut self, src: Reg, dst: Reg, cc: IntCmpKind, size: OperandSize) {
        let ty = match size {
            OperandSize::S32 => types::F32,
            OperandSize::S64 => types::F64,
            // Move the entire 128 bits via movdqa.
            OperandSize::S128 => types::I128,
        };

        self.emit(Inst::XmmCmove {
            ty,
            cc: cc.into(),
            consequent: XmmMemAligned::new(src.into()).expect("valid XmmMemAligned"),
            alternative: dst.into(),
            dst: dst.into(),
        })
    }

    /// Subtract register and register
    pub fn sub_rr(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        self.emit(Inst::AluRmiR {
            size: size.into(),
            op: AluRmiROpcode::Sub,
            src1: dst.into(),
            src2: src.into(),
            dst: dst.into(),
        });
    }

    /// Subtact immediate register.
    pub fn sub_ir(&mut self, imm: i32, dst: Reg, size: OperandSize) {
        let imm = RegMemImm::imm(imm as u32);

        self.emit(Inst::AluRmiR {
            size: size.into(),
            op: AluRmiROpcode::Sub,
            src1: dst.into(),
            src2: GprMemImm::new(imm).expect("valid immediate"),
            dst: dst.into(),
        });
    }

    /// "and" two registers.
    pub fn and_rr(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        self.emit(Inst::AluRmiR {
            size: size.into(),
            op: AluRmiROpcode::And,
            src1: dst.into(),
            src2: src.into(),
            dst: dst.into(),
        });
    }

    pub fn and_ir(&mut self, imm: i32, dst: Reg, size: OperandSize) {
        let imm = RegMemImm::imm(imm as u32);
        self.emit(Inst::AluRmiR {
            size: size.into(),
            op: AluRmiROpcode::And,
            src1: dst.into(),
            src2: GprMemImm::new(imm).expect("valid immediate"),
            dst: dst.into(),
        });
    }

    /// "and" two float registers.
    pub fn xmm_and_rr(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        let op = match size {
            OperandSize::S32 => SseOpcode::Andps,
            OperandSize::S64 => SseOpcode::Andpd,
            OperandSize::S128 => unreachable!(),
        };

        self.emit(Inst::XmmRmR {
            op,
            src1: dst.into(),
            src2: XmmMemAligned::from(Xmm::from(src)),
            dst: dst.into(),
        });
    }

    /// "and not" two float registers.
    pub fn xmm_andn_rr(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        let op = match size {
            OperandSize::S32 => SseOpcode::Andnps,
            OperandSize::S64 => SseOpcode::Andnpd,
            OperandSize::S128 => unreachable!(),
        };

        self.emit(Inst::XmmRmR {
            op,
            src1: dst.into(),
            src2: Xmm::from(src).into(),
            dst: dst.into(),
        });
    }

    pub fn gpr_to_xmm(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        let op = match size {
            OperandSize::S32 => SseOpcode::Movd,
            OperandSize::S64 => SseOpcode::Movq,
            OperandSize::S128 => unreachable!(),
        };

        self.emit(Inst::GprToXmm {
            op,
            src: src.into(),
            dst: dst.into(),
            src_size: size.into(),
        })
    }

    pub fn or_rr(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        self.emit(Inst::AluRmiR {
            size: size.into(),
            op: AluRmiROpcode::Or,
            src1: dst.into(),
            src2: src.into(),
            dst: dst.into(),
        });
    }

    pub fn or_ir(&mut self, imm: i32, dst: Reg, size: OperandSize) {
        let imm = RegMemImm::imm(imm as u32);

        self.emit(Inst::AluRmiR {
            size: size.into(),
            op: AluRmiROpcode::Or,
            src1: dst.into(),
            src2: GprMemImm::new(imm).expect("valid immediate"),
            dst: dst.into(),
        });
    }

    pub fn xmm_or_rr(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        let op = match size {
            OperandSize::S32 => SseOpcode::Orps,
            OperandSize::S64 => SseOpcode::Orpd,
            OperandSize::S128 => unreachable!(),
        };

        self.emit(Inst::XmmRmR {
            op,
            src1: dst.into(),
            src2: XmmMemAligned::from(Xmm::from(src)),
            dst: dst.into(),
        });
    }

    /// Logical exclusive or with registers.
    pub fn xor_rr(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        self.emit(Inst::AluRmiR {
            size: size.into(),
            op: AluRmiROpcode::Xor,
            src1: dst.into(),
            src2: src.into(),
            dst: dst.into(),
        });
    }

    pub fn xor_ir(&mut self, imm: i32, dst: Reg, size: OperandSize) {
        let imm = RegMemImm::imm(imm as u32);

        self.emit(Inst::AluRmiR {
            size: size.into(),
            op: AluRmiROpcode::Xor,
            src1: dst.into(),
            src2: GprMemImm::new(imm).expect("valid immediate"),
            dst: dst.into(),
        });
    }

    /// Logical exclusive or with float registers.
    pub fn xmm_xor_rr(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        let op = match size {
            OperandSize::S32 => SseOpcode::Xorps,
            OperandSize::S64 => SseOpcode::Xorpd,
            OperandSize::S128 => unreachable!(),
        };

        self.emit(Inst::XmmRmR {
            op,
            src1: dst.into(),
            src2: XmmMemAligned::from(Xmm::from(src)),
            dst: dst.into(),
        });
    }

    /// Shift with register and register.
    pub fn shift_rr(&mut self, src: Reg, dst: Reg, kind: ShiftKind, size: OperandSize) {
        self.emit(Inst::ShiftR {
            size: size.into(),
            kind: kind.into(),
            src: dst.into(),
            num_bits: src.into(),
            dst: dst.into(),
        });
    }

    /// Shift with immediate and register.
    pub fn shift_ir(&mut self, imm: u8, dst: Reg, kind: ShiftKind, size: OperandSize) {
        let imm = imm.into();

        self.emit(Inst::ShiftR {
            size: size.into(),
            kind: kind.into(),
            src: dst.into(),
            num_bits: Imm8Gpr::new(imm).expect("valid immediate"),
            dst: dst.into(),
        });
    }

    /// Signed/unsigned division.
    ///
    /// Emits a sequence of instructions to ensure the correctness of
    /// the division invariants.  This function assumes that the
    /// caller has correctly allocated the dividend as `(rdx:rax)` and
    /// accounted for the quotient to be stored in `rax`.
    pub fn div(&mut self, divisor: Reg, dst: (Reg, Reg), kind: DivKind, size: OperandSize) {
        let trap = match kind {
            // Signed division has two trapping conditions, integer overflow and
            // divide-by-zero. Check for divide-by-zero explicitly and let the
            // hardware detect overflow.
            //
            // The dividend is sign extended to initialize `rdx`.
            DivKind::Signed => {
                self.emit(Inst::CmpRmiR {
                    size: size.into(),
                    src: GprMemImm::new(RegMemImm::imm(0)).unwrap(),
                    dst: divisor.into(),
                    opcode: CmpOpcode::Cmp,
                });
                self.emit(Inst::TrapIf {
                    cc: CC::Z,
                    trap_code: TrapCode::IntegerDivisionByZero,
                });
                self.emit(Inst::SignExtendData {
                    size: size.into(),
                    src: dst.0.into(),
                    dst: dst.1.into(),
                });
                TrapCode::IntegerOverflow
            }

            // Unsigned division only traps in one case, on divide-by-zero, so
            // defer that to the trap opcode.
            //
            // The divisor_hi reg is initialized with zero through an
            // xor-against-itself op.
            DivKind::Unsigned => {
                self.emit(Inst::AluRmiR {
                    size: size.into(),
                    op: AluRmiROpcode::Xor,
                    src1: dst.1.into(),
                    src2: dst.1.into(),
                    dst: dst.1.into(),
                });
                TrapCode::IntegerDivisionByZero
            }
        };
        self.emit(Inst::Div {
            sign: kind.into(),
            size: size.into(),
            trap,
            divisor: GprMem::new(RegMem::reg(divisor.into())).unwrap(),
            dividend_lo: dst.0.into(),
            dividend_hi: dst.1.into(),
            dst_quotient: dst.0.into(),
            dst_remainder: dst.1.into(),
        });
    }

    /// Signed/unsigned remainder.
    ///
    /// Emits a sequence of instructions to ensure the correctness of the
    /// division invariants and ultimately calculate the remainder.
    /// This function assumes that the
    /// caller has correctly allocated the dividend as `(rdx:rax)` and
    /// accounted for the remainder to be stored in `rdx`.
    pub fn rem(&mut self, divisor: Reg, dst: (Reg, Reg), kind: RemKind, size: OperandSize) {
        match kind {
            // Signed remainder goes through a pseudo-instruction which has
            // some internal branching. The `dividend_hi`, or `rdx`, is
            // initialized here with a `SignExtendData` instruction.
            RemKind::Signed => {
                self.emit(Inst::SignExtendData {
                    size: size.into(),
                    src: dst.0.into(),
                    dst: dst.1.into(),
                });
                self.emit(Inst::CheckedSRemSeq {
                    size: size.into(),
                    divisor: divisor.into(),
                    dividend_lo: dst.0.into(),
                    dividend_hi: dst.1.into(),
                    dst_quotient: dst.0.into(),
                    dst_remainder: dst.1.into(),
                });
            }

            // Unsigned remainder initializes `dividend_hi` with zero and
            // then executes a normal `div` instruction.
            RemKind::Unsigned => {
                self.emit(Inst::AluRmiR {
                    size: size.into(),
                    op: AluRmiROpcode::Xor,
                    src1: dst.1.into(),
                    src2: dst.1.into(),
                    dst: dst.1.into(),
                });
                self.emit(Inst::Div {
                    sign: DivSignedness::Unsigned,
                    trap: TrapCode::IntegerDivisionByZero,
                    size: size.into(),
                    divisor: GprMem::new(RegMem::reg(divisor.into())).unwrap(),
                    dividend_lo: dst.0.into(),
                    dividend_hi: dst.1.into(),
                    dst_quotient: dst.0.into(),
                    dst_remainder: dst.1.into(),
                });
            }
        }
    }

    /// Multiply immediate and register.
    pub fn mul_ir(&mut self, imm: i32, dst: Reg, size: OperandSize) {
        let imm = RegMemImm::imm(imm as u32);

        self.emit(Inst::AluRmiR {
            size: size.into(),
            op: AluRmiROpcode::Mul,
            src1: dst.into(),
            src2: GprMemImm::new(imm).expect("valid immediate"),
            dst: dst.into(),
        });
    }

    /// Multiply register and register.
    pub fn mul_rr(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        self.emit(Inst::AluRmiR {
            size: size.into(),
            op: AluRmiROpcode::Mul,
            src1: dst.into(),
            src2: src.into(),
            dst: dst.into(),
        });
    }

    /// Add immediate and register.
    pub fn add_ir(&mut self, imm: i32, dst: Reg, size: OperandSize) {
        let imm = RegMemImm::imm(imm as u32);

        self.emit(Inst::AluRmiR {
            size: size.into(),
            op: AluRmiROpcode::Add,
            src1: dst.into(),
            src2: GprMemImm::new(imm).expect("valid immediate"),
            dst: dst.into(),
        });
    }

    /// Add register and register.
    pub fn add_rr(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        self.emit(Inst::AluRmiR {
            size: size.into(),
            op: AluRmiROpcode::Add,
            src1: dst.into(),
            src2: src.into(),
            dst: dst.into(),
        });
    }

    pub fn cmp_ir(&mut self, imm: i32, dst: Reg, size: OperandSize) {
        let imm = RegMemImm::imm(imm as u32);

        self.emit(Inst::CmpRmiR {
            size: size.into(),
            opcode: CmpOpcode::Cmp,
            src: GprMemImm::new(imm).expect("valid immediate"),
            dst: dst.into(),
        });
    }

    pub fn cmp_rr(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        self.emit(Inst::CmpRmiR {
            size: size.into(),
            opcode: CmpOpcode::Cmp,
            src: src.into(),
            dst: dst.into(),
        });
    }

    /// Compares values in src and dst and sets ZF, PF, and CF flags in EFLAGS
    /// register.
    pub fn ucomis(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        let op = match size {
            OperandSize::S32 => SseOpcode::Ucomiss,
            OperandSize::S64 => SseOpcode::Ucomisd,
            OperandSize::S128 => unreachable!(),
        };

        self.emit(Inst::XmmCmpRmR {
            op,
            src: Xmm::from(src).into(),
            dst: dst.into(),
        });
    }

    pub fn popcnt(&mut self, src: Reg, size: OperandSize) {
        assert!(
            self.isa_flags.has_popcnt() && self.isa_flags.has_sse42(),
            "Requires has_popcnt and has_sse42 flags"
        );
        self.emit(Inst::UnaryRmR {
            size: size.into(),
            op: args::UnaryRmROpcode::Popcnt,
            src: src.into(),
            dst: src.into(),
        });
    }

    /// Emit a test instruction with two register operands.
    pub fn test_rr(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        self.emit(Inst::CmpRmiR {
            size: size.into(),
            opcode: CmpOpcode::Test,
            src: src.into(),
            dst: dst.into(),
        })
    }

    /// Set value in dst to `0` or `1` based on flags in status register and
    /// [`CmpKind`].
    pub fn setcc(&mut self, kind: IntCmpKind, dst: Reg) {
        self.setcc_impl(kind.into(), dst);
    }

    /// Set value in dst to `1` if parity flag in status register is set, `0`
    /// otherwise.
    pub fn setp(&mut self, dst: Reg) {
        self.setcc_impl(CC::P, dst);
    }

    /// Set value in dst to `1` if parity flag in status register is not set,
    /// `0` otherwise.
    pub fn setnp(&mut self, dst: Reg) {
        self.setcc_impl(CC::NP, dst);
    }

    fn setcc_impl(&mut self, cc: CC, dst: Reg) {
        // Clear the dst register or bits 1 to 31 may be incorrectly set.
        // Don't use xor since it updates the status register.
        self.emit(Inst::Imm {
            dst_size: args::OperandSize::Size32, // Always going to be an i32 result.
            simm64: 0,
            dst: dst.into(),
        });
        // Copy correct bit from status register into dst register.
        self.emit(Inst::Setcc {
            cc,
            dst: dst.into(),
        });
    }

    /// Store the count of leading zeroes in src in dst.
    /// Requires `has_lzcnt` flag.
    pub fn lzcnt(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        assert!(self.isa_flags.has_lzcnt(), "Requires has_lzcnt flag");
        self.emit(Inst::UnaryRmR {
            size: size.into(),
            op: args::UnaryRmROpcode::Lzcnt,
            src: src.into(),
            dst: dst.into(),
        });
    }

    /// Store the count of trailing zeroes in src in dst.
    /// Requires `has_bmi1` flag.
    pub fn tzcnt(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        assert!(self.isa_flags.has_bmi1(), "Requires has_bmi1 flag");
        self.emit(Inst::UnaryRmR {
            size: size.into(),
            op: args::UnaryRmROpcode::Tzcnt,
            src: src.into(),
            dst: dst.into(),
        });
    }

    /// Stores position of the most significant bit set in src in dst.
    /// Zero flag is set if src is equal to 0.
    pub fn bsr(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        self.emit(Inst::UnaryRmR {
            size: size.into(),
            op: args::UnaryRmROpcode::Bsr,
            src: src.into(),
            dst: dst.into(),
        });
    }

    /// Performs integer negation on src and places result in dst.
    pub fn neg(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        self.emit(Inst::Neg {
            size: size.into(),
            src: src.into(),
            dst: dst.into(),
        });
    }

    /// Stores position of the least significant bit set in src in dst.
    /// Zero flag is set if src is equal to 0.
    pub fn bsf(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        self.emit(Inst::UnaryRmR {
            size: size.into(),
            op: args::UnaryRmROpcode::Bsf,
            src: src.into(),
            dst: dst.into(),
        });
    }

    /// Performs float addition on src and dst and places result in dst.
    pub fn xmm_add_rr(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        let op = match size {
            OperandSize::S32 => SseOpcode::Addss,
            OperandSize::S64 => SseOpcode::Addsd,
            OperandSize::S128 => unreachable!(),
        };

        self.emit(Inst::XmmRmRUnaligned {
            op,
            src1: Xmm::from(dst).into(),
            src2: Xmm::from(src).into(),
            dst: dst.into(),
        });
    }

    /// Performs float subtraction on src and dst and places result in dst.
    pub fn xmm_sub_rr(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        let op = match size {
            OperandSize::S32 => SseOpcode::Subss,
            OperandSize::S64 => SseOpcode::Subsd,
            OperandSize::S128 => unreachable!(),
        };

        self.emit(Inst::XmmRmRUnaligned {
            op,
            src1: Xmm::from(dst).into(),
            src2: Xmm::from(src).into(),
            dst: dst.into(),
        });
    }

    /// Performs float multiplication on src and dst and places result in dst.
    pub fn xmm_mul_rr(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        let op = match size {
            OperandSize::S32 => SseOpcode::Mulss,
            OperandSize::S64 => SseOpcode::Mulsd,
            OperandSize::S128 => unreachable!(),
        };

        self.emit(Inst::XmmRmRUnaligned {
            op,
            src1: Xmm::from(dst).into(),
            src2: Xmm::from(src).into(),
            dst: dst.into(),
        });
    }

    /// Performs float division on src and dst and places result in dst.
    pub fn xmm_div_rr(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        let op = match size {
            OperandSize::S32 => SseOpcode::Divss,
            OperandSize::S64 => SseOpcode::Divsd,
            OperandSize::S128 => unreachable!(),
        };

        self.emit(Inst::XmmRmRUnaligned {
            op,
            src1: Xmm::from(dst).into(),
            src2: Xmm::from(src).into(),
            dst: dst.into(),
        });
    }

    /// Mininum for src and dst XMM registers with results put in dst.
    pub fn xmm_min_seq(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        self.emit(Inst::XmmMinMaxSeq {
            size: size.into(),
            is_min: true,
            lhs: src.into(),
            rhs: dst.into(),
            dst: dst.into(),
        });
    }

    /// Maximum for src and dst XMM registers with results put in dst.
    pub fn xmm_max_seq(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        self.emit(Inst::XmmMinMaxSeq {
            size: size.into(),
            is_min: false,
            lhs: src.into(),
            rhs: dst.into(),
            dst: dst.into(),
        });
    }

    /// Perform rounding operation on float register src and place results in
    /// float register dst.
    pub fn xmm_rounds_rr(&mut self, src: Reg, dst: Reg, mode: RoundingMode, size: OperandSize) {
        let op = match size {
            OperandSize::S32 => SseOpcode::Roundss,
            OperandSize::S64 => SseOpcode::Roundsd,
            OperandSize::S128 => unreachable!(),
        };

        let imm: u8 = match mode {
            RoundingMode::Nearest => 0x00,
            RoundingMode::Down => 0x01,
            RoundingMode::Up => 0x02,
            RoundingMode::Zero => 0x03,
        };

        self.emit(Inst::XmmUnaryRmRImm {
            op,
            src: XmmMemAligned::from(Xmm::from(src)),
            imm,
            dst: dst.into(),
        })
    }

    pub fn sqrt(&mut self, src: Reg, dst: Reg, size: OperandSize) {
        let op = match size {
            OperandSize::S32 => SseOpcode::Sqrtss,
            OperandSize::S64 => SseOpcode::Sqrtsd,
            OperandSize::S128 => unreachable!(),
        };

        self.emit(Inst::XmmUnaryRmR {
            op,
            src: Xmm::from(src).into(),
            dst: dst.into(),
        })
    }

    /// Emit a call to an unknown location through a register.
    pub fn call_with_reg(&mut self, callee: Reg) {
        self.emit(Inst::CallUnknown {
            dest: RegMem::reg(callee.into()),
            info: Box::new(CallInfo {
                uses: smallvec![],
                defs: smallvec![],
                clobbers: Default::default(),
                opcode: Opcode::Call,
                callee_pop_size: 0,
                callee_conv: CallConv::SystemV,
            }),
        });
    }

    /// Emit a call to a locally defined function through an index.
    pub fn call_with_index(&mut self, index: u32) {
        let dest = ExternalName::user(UserExternalNameRef::new(index as usize));
        self.emit(Inst::CallKnown {
            dest,
            info: Box::new(CallInfo {
                uses: smallvec![],
                defs: smallvec![],
                clobbers: Default::default(),
                opcode: Opcode::Call,
                callee_pop_size: 0,
                callee_conv: CallConv::SystemV,
            }),
        });
    }

    /// Emit a call to a well-known libcall.
    pub fn call_with_lib(&mut self, lib: LibCall) {
        let dest = ExternalName::LibCall(lib);
        self.emit(Inst::CallKnown {
            dest,
            info: Box::new(CallInfo {
                uses: smallvec![],
                defs: smallvec![],
                clobbers: Default::default(),
                opcode: Opcode::Call,
                callee_pop_size: 0,
                callee_conv: CallConv::SystemV,
            }),
        });
    }

    /// Emits a conditional jump to the given label.
    pub fn jmp_if(&mut self, cc: impl Into<CC>, taken: MachLabel) {
        self.emit(Inst::JmpIf {
            cc: cc.into(),
            taken,
        });
    }

    /// Performs an unconditional jump to the given label.
    pub fn jmp(&mut self, target: MachLabel) {
        self.emit(Inst::JmpKnown { dst: target });
    }

    /// Emits a jump table sequence.
    pub fn jmp_table(
        &mut self,
        targets: SmallVec<[MachLabel; 4]>,
        default: MachLabel,
        index: Reg,
        tmp1: Reg,
        tmp2: Reg,
    ) {
        self.emit(Inst::JmpTableSeq {
            idx: index.into(),
            tmp1: Writable::from_reg(tmp1.into()),
            tmp2: Writable::from_reg(tmp2.into()),
            default_target: default,
            targets: Box::new(targets.to_vec()),
        })
    }

    /// Emit a trap instruction.
    pub fn trap(&mut self, code: TrapCode) {
        self.emit(Inst::Ud2 { trap_code: code })
    }

    /// Conditional trap.
    pub fn trapif(&mut self, cc: IntCmpKind, trap_code: TrapCode) {
        self.emit(Inst::TrapIf {
            cc: cc.into(),
            trap_code,
        });
    }
}
