// Compiled against owned and archived vocabularies, so validation does not decode semantic IR.
fn width(width: Width) -> u8 {
    match width { Width::W8 => 0, Width::W16 => 1, Width::W32 => 2, Width::W64 => 3 }
}

fn local(expr: &PlaceExpr) -> Option<u32> {
    let PlaceBase::Local(off) = expr.base else { return None };
    let mut off = i64::from(off);
    for step in expr.steps.iter() {
        let PlaceStep::Offset(delta) = step else { return None };
        off = off.checked_add(i64::from(*delta))?;
    }
    u32::try_from(off).ok().filter(|off| *off < DICTIONARY)
}

fn operand(op: &Operand, dictionary: Option<u32>, literals: &mut impl Literals) -> Option<(u32, u8)> {
    match op {
        Operand::Slot(slot) => (slot.off < DICTIONARY).then_some((slot.off, width(slot.width))),
        Operand::Imm { bits, width: w } => {
            Some((literals.literal(*bits)?, width(*w)))
        }
        Operand::Mem { expr, width: w } => {
            if let Some(off) = local(expr) { return Some((off, width(*w))); }
            if matches!(expr.base, PlaceBase::Local(off) if Some(off) == dictionary)
                && let [PlaceStep::Deref, PlaceStep::Offset(off)] = expr.steps.as_ref()
                && *off >= 0 && off % 8 == 0
            { return Some((DICTIONARY | (*off as u32 / 8), width(*w))); }
            None
        }
        _ => None,
    }
}

pub(super) fn lower(stmt: &Stmt, dictionary: Option<u32>, literals: &mut impl Literals) -> Option<Operation> {
    let Stmt::Assign { dst, rv } = stmt else { return None };
    let (dst, dest_width) = match dst {
        ScalarPlace::Slot(slot) => (slot.off, width(slot.width)),
        ScalarPlace::Mem { expr, width: w } => (local(expr)?, width(*w)),
    };
    if dst >= DICTIONARY { return None; }
    let (code, aux, signed, first, second) = match rv {
        Rvalue::Use(a) => (1, 0, false, a, None),
        Rvalue::IntBin { op, signed, a, b } => {
            let aux = match op { IntBinOp::Add => 0, IntBinOp::Sub => 1, IntBinOp::Mul => 2, IntBinOp::Div => 3,
                IntBinOp::Rem => 4, IntBinOp::BitAnd => 5, IntBinOp::BitOr => 6, IntBinOp::BitXor => 7,
                IntBinOp::Shl => 8, IntBinOp::Shr => 9 };
            (2, aux, *signed, a, Some(b))
        }
        Rvalue::IntCmp { cc, signed, a, b } => {
            let aux = match cc { IntCc::Eq => 0, IntCc::Ne => 1, IntCc::Lt => 2, IntCc::Le => 3, IntCc::Gt => 4, IntCc::Ge => 5 };
            (3, aux, *signed, a, Some(b))
        }
        Rvalue::NotBits(a) => (4, 0, false, a, None),
        Rvalue::NotBool(a) => (5, 0, false, a, None),
        Rvalue::Neg(a) => (6, 0, false, a, None),
        Rvalue::Cast { a, from, to } => (7, width(from.0) | width(*to) << 2, from.1, a, None),
        _ => return None,
    };
    let (a, aw) = operand(first, dictionary, literals)?;
    let (b, bw) = match second { Some(b) => operand(b, dictionary, literals)?, None => (0, 0) };
    Some(Operation { code, aux, flags: dest_width | aw << 2 | bw << 4 | u8::from(signed) << 6, dst, a, b })
}
