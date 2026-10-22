{
    if self.offsets.is_empty() { return self.ops.is_empty() && self.constants.is_empty(); }
    if self.offsets.len() != blocks.len() + 1 || self.offsets[0] != 0 { return false; }
    let mut literals = CheckLiterals { constants: self.constants.as_slice(), next: 0 };
    let mut cursor = 0usize;
    for (bb, block) in blocks.iter().enumerate() {
        for (index, stmt) in block.stmts.iter().enumerate() {
            let mark = literals.next;
            let expected = lower(stmt, dictionary, &mut literals).unwrap_or_else(|| {
                literals.next = mark;
                Operation::fallback(index as u32)
            });
            let Some(actual) = self.ops.get(cursor) else { return false };
            if (actual.code, actual.flags, actual.aux, actual.dst, actual.a, actual.b)
                != (expected.code, expected.flags, expected.aux, expected.dst, expected.a, expected.b)
            { return false; }
            cursor += 1;
        }
        if self.offsets[bb + 1] as usize != cursor { return false; }
    }
    cursor == self.ops.len() && literals.next == self.constants.len()
}
